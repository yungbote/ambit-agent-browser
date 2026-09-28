//! The agent's `scroll` in the owned window, as a person scrolls: the
//! pointer goes where the wheel reaches the scroller, and the wheel turns one
//! notch per frame until the scroller is where the command asked. Chrome's
//! own smooth scrolling animates each notch.
//!
//! The loop is closed on the scroller. It reads the scroller's offsets once
//! per frame while it waits, and each round sends the notches the remaining
//! distance needs at the distance per notch the scroller has shown. A
//! scroller the wheel leaves unmoved for `motion::WHEEL_STALL` after a
//! round's first notch (a canvas that takes the wheel, overflow a person
//! cannot scroll), one no visible point turns, and a distance shorter than
//! half a notch are scrolled by script instead: published as `scrolling`,
//! with no pointer path. No motion is invented.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{stopped, Acted, Mapping, NativeMouse, Pacing};
use crate::native::actions::CommandError;
use crate::native::browser_control::interrupts::Interrupts;
use crate::native::browser_control::motion::{self, Glide};
use crate::native::cdp::client::CdpClient;
use crate::native::display::DisplayClient;

/// The device-independent pixels Chrome on X11 scrolls per wheel click
/// (measured: 120 CSS pixels a notch at 100 % zoom). Each scroll starts
/// from it and learns the distance its scroller really moves per notch.
const NOTCH_DIP: f64 = 120.0;

/// Rounds of notches per scroll: the first, then corrections while the
/// scroller settled short of its goal.
const ROUNDS: usize = 3;

/// A scroller that has not moved for this many frames in a row has settled.
const SETTLED_FRAMES: u32 = 3;

/// The longest a scroller may keep moving after a round's last notch.
const SETTLE_LIMIT: Duration = Duration::from_secs(1);

pub(super) const OFFSETS: &str = "function() { return [this.scrollLeft, this.scrollTop, this.scrollWidth - this.clientWidth, this.scrollHeight - this.clientHeight]; }";

/// Where a wheel turned in the direction `(dx, dy)` reaches this scroller
/// first, as near the pointer `(px, py)` (null when unknown) as a point can
/// be: `[x, y, width]` in the session's top document, where `width` is the
/// smaller side of the scroller's visible box, or null when no visible point
/// does. A point over a frame or a canvas is never chosen: those take the
/// wheel themselves.
pub(super) const WHEEL_POINT: &str = r#"function(px, py, dx, dy) {
    const doc = this.ownerDocument, win = doc.defaultView;
    const root = doc.scrollingElement || doc.documentElement;
    const page = this === root;
    let ox = 0, oy = 0;
    for (let w = win; w.frameElement; w = w.parent) {
        const frame = w.frameElement, box = frame.getBoundingClientRect();
        ox += box.left + frame.clientLeft;
        oy += box.top + frame.clientTop;
    }
    const overflow = (el, axis) => {
        const own = getComputedStyle(el)[axis];
        return el === root && own === 'visible' && doc.body ? getComputedStyle(doc.body)[axis] : own;
    };
    const turns = (el, x, delta) => {
        if (!delta) return false;
        const flow = overflow(el, x ? 'overflowX' : 'overflowY');
        if (el === root ? /^(hidden|clip)$/.test(flow) : !/^(auto|scroll|overlay)$/.test(flow)) return false;
        const at = x ? el.scrollLeft : el.scrollTop;
        const end = x ? el.scrollWidth - el.clientWidth : el.scrollHeight - el.clientHeight;
        return delta > 0 ? at < end - 1 : at > 0;
    };
    const scrolls = el => turns(el, true, dx) || turns(el, false, dy);
    const parent = el => el.assignedSlot || el.parentElement || (el.parentNode && el.parentNode.host) || null;
    const reaches = (x, y) => {
        let el = doc.elementFromPoint(x, y);
        while (el && el.shadowRoot) {
            const inner = el.shadowRoot.elementFromPoint(x, y);
            if (!inner || inner === el) break;
            el = inner;
        }
        if (!el || /^(IFRAME|FRAME|OBJECT|EMBED|CANVAS)$/.test(el.tagName)) return false;
        for (; el && el !== root; el = parent(el)) {
            if (el === this) return scrolls(this);
            if (scrolls(el)) return false;
        }
        return page && scrolls(root);
    };
    const box = page ? { left: 0, top: 0, right: win.innerWidth, bottom: win.innerHeight } : this.getBoundingClientRect();
    const left = Math.max(0, box.left), top = Math.max(0, box.top);
    const right = Math.min(win.innerWidth, box.right), bottom = Math.min(win.innerHeight, box.bottom);
    if (!(right - left >= 1 && bottom - top >= 1)) return null;
    const width = Math.min(right - left, bottom - top);
    const known = typeof px === 'number' && typeof py === 'number';
    const lx = known ? px - ox : (left + right) / 2, ly = known ? py - oy : (top + bottom) / 2;
    if (known && lx >= left && lx < right && ly >= top && ly < bottom && reaches(lx, ly)) return [px, py, width];
    let best = null;
    for (let i = 0; i < 7; i++) for (let j = 0; j < 7; j++) {
        const x = Math.floor(left + (i + 0.5) * (right - left) / 7);
        const y = Math.floor(top + (j + 0.5) * (bottom - top) / 7);
        const distance = (x - lx) ** 2 + (y - ly) ** 2;
        if ((!best || distance < best.distance) && reaches(x, y)) best = { x, y, distance };
    }
    return best && [best.x + ox, best.y + oy, width];
}"#;

/// Two values, one per axis: horizontal, then vertical.
type Axes<T> = [T; 2];

/// Where a scroller is and how far it can go, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Offsets {
    at: Axes<f64>,
    most: Axes<f64>,
}

impl Offsets {
    /// How far the scroller still has to go to be `delta` from `start`, as
    /// far as it can go now: content that grows lets it go further.
    fn remaining(&self, start: Axes<f64>, delta: Axes<f64>) -> Axes<f64> {
        std::array::from_fn(|axis| {
            (start[axis] + delta[axis]).clamp(0.0, self.most[axis].max(0.0)) - self.at[axis]
        })
    }
}

/// The direction a scroll asks for on one axis: +1, -1, or 0 for none.
fn direction(delta: f64) -> f64 {
    if delta > 0.0 {
        1.0
    } else if delta < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// The distance per notch a round showed on one axis: what the scroller
/// moved over the notches it turned, unless it stopped at an end, where the
/// notches may have moved it less than they would elsewhere.
fn learned(previous: f64, moved: f64, notches: u32, at: f64, most: f64) -> f64 {
    if notches > 0 && moved != 0.0 && at > 0.0 && at < most {
        moved.abs() / f64::from(notches)
    } else {
        previous
    }
}

async fn call(
    client: &CdpClient,
    session: &str,
    scroller: &str,
    function: &str,
    arguments: Value,
) -> Result<Value, String> {
    let read = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId": scroller, "functionDeclaration": function,
                "arguments": arguments, "returnByValue": true,
            })),
            Some(session),
        )
        .await?;
    if read.get("exceptionDetails").is_some() {
        return Err("The scroller could not be read.".into());
    }
    Ok(read["result"]["value"].clone())
}

async fn offsets(client: &CdpClient, session: &str, scroller: &str) -> Result<Offsets, String> {
    let value = call(client, session, scroller, OFFSETS, json!([])).await?;
    let number = |index: usize| value[index].as_f64().filter(|value| value.is_finite());
    match (number(0), number(1), number(2), number(3)) {
        (Some(x), Some(y), Some(most_x), Some(most_y)) => Ok(Offsets {
            at: [x, y],
            most: [most_x, most_y],
        }),
        _ => Err("The scroller could not be read.".into()),
    }
}

async fn wheel_point(
    client: &CdpClient,
    session: &str,
    scroller: &str,
    pointer: Option<(f64, f64)>,
    toward: Axes<f64>,
) -> Result<Option<(f64, f64, f64)>, String> {
    let (px, py) = pointer.map_or((Value::Null, Value::Null), |(x, y)| (json!(x), json!(y)));
    let value = call(
        client,
        session,
        scroller,
        WHEEL_POINT,
        json!([{"value":px},{"value":py},{"value":toward[0]},{"value":toward[1]}]),
    )
    .await?;
    let number = |index: usize| value[index].as_f64().filter(|value| value.is_finite());
    Ok(match (number(0), number(1), number(2)) {
        (Some(x), Some(y), Some(width)) => Some((x, y, width)),
        _ => None,
    })
}

/// The page's own scroller, held in the page's pointer realm.
async fn page_scroller(client: &CdpClient, mapping: &Mapping) -> Result<String, String> {
    let read = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.scrollingElement || document.documentElement",
                "contextId": mapping.pointer.context,
            })),
            Some(&mapping.session),
        )
        .await?;
    read["result"]["objectId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "The page has no scrolling element.".into())
}

/// Where one scroll stands while its wheel turns.
struct Scroll<'a> {
    client: &'a CdpClient,
    page_session: &'a str,
    session: &'a str,
    scroller: String,
    start: Axes<f64>,
    delta: Axes<f64>,
}

impl Scroll<'_> {
    async fn read(&self, pacing: &mut Pacing<'_>) -> Result<Option<Offsets>, CommandError> {
        pacing
            .observe(offsets(self.client, self.session, &self.scroller))
            .await
    }

    /// How many notches each axis still needs, at `per_notch` CSS pixels a
    /// notch.
    fn wanted(&self, at: &Offsets, per_notch: Axes<f64>) -> Axes<u32> {
        let left = at.remaining(self.start, self.delta);
        std::array::from_fn(|axis| {
            motion::notches_toward(left[axis], direction(self.delta[axis]), per_notch[axis])
        })
    }

    /// The rest of the way by script, published as `scrolling`.
    async fn by_script(&self, at: &Offsets) -> Result<(), CommandError> {
        let [delta_x, delta_y] = at.remaining(self.start, self.delta);
        if delta_x == 0.0 && delta_y == 0.0 {
            return Ok(());
        }
        crate::native::interaction::scroll_by(
            self.client,
            self.page_session,
            self.session,
            &self.scroller,
            delta_x,
            delta_y,
        )
        .await
        .map_err(Into::into)
    }
}

impl NativeMouse {
    /// Scrolls by `delta` CSS pixels, as far as the scroller can go, with the
    /// wheel: `scroller` is an object id in `session`, or `None` for that
    /// page's own scroller. `page_session` is the tab's page, whose dialogs
    /// end the scroll as well as `session`'s.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::native::browser_control) async fn scroll(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        page_session: &str,
        session: &str,
        scroller: Option<&str>,
        delta: (f64, f64),
        interrupts: &Interrupts,
    ) -> Result<(), CommandError> {
        self.require_known()?;
        let result = self
            .scroll_with_wheel(
                client,
                display,
                page_session,
                session,
                scroller,
                delta,
                interrupts,
            )
            .await
            .map(|()| false);
        self.finish(result, display).await.map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    async fn scroll_with_wheel(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        page_session: &str,
        session: &str,
        scroller: Option<&str>,
        delta: (f64, f64),
        interrupts: &Interrupts,
    ) -> Result<(), CommandError> {
        self.same_layout(display)?;
        if let Some(reason) = interrupts.pending() {
            return Err(stopped(reason, Acted::Nothing));
        }
        let mapping = self.prepare(client, session, display).await?;
        let scroller = match scroller {
            Some(scroller) => scroller.to_owned(),
            None => page_scroller(client, &mapping).await?,
        };
        let dialog_sessions = [session, page_session];
        let mut pacing = Pacing::start(client, interrupts, &dialog_sessions);
        let Some(start) = pacing.observe(offsets(client, session, &scroller)).await? else {
            return Ok(());
        };
        let scroll = Scroll {
            client,
            page_session,
            session,
            scroller,
            start: start.at,
            delta: [delta.0, delta.1],
        };
        let surface = display.surface();
        let scale = mapping.scale()?;
        let toward = scroll.delta.map(direction);
        let mut per_notch = [NOTCH_DIP * f64::from(surface.device_scale_factor) / scale; 2];
        if scroll.wanted(&start, per_notch) == [0, 0] {
            // Nothing to scroll, or less than half a notch: no wheel turns
            // that little.
            return scroll.by_script(&start).await;
        }
        let pointer = display
            .pointer()
            .and_then(|pointer| mapping.page(pointer, &surface).ok());
        let point = wheel_point(client, session, &scroll.scroller, pointer, toward);
        let Some(found) = pacing.observe(point).await? else {
            return Ok(());
        };
        let Some((to, (x, y), width)) = found.and_then(|(x, y, width)| {
            let to = mapping.point(x, y, &surface).ok()?;
            Some((to, (x, y), width))
        }) else {
            return scroll.by_script(&start).await;
        };
        let from = self.origin(&mapping, display, &surface)?;
        if let Some(glide) = Glide::new(from, to, width * scale, motion::REACHED_CSS_PX * scale) {
            if self
                .travel(
                    glide,
                    0,
                    &mapping,
                    client,
                    display,
                    &dialog_sessions,
                    interrupts,
                )
                .await?
            {
                return Ok(());
            }
        }
        let mut at = start;
        let mut acted = Acted::Nothing;
        for _ in 0..ROUNDS {
            let count = scroll.wanted(&at, per_notch);
            if count == [0, 0] {
                break;
            }
            let from = at;
            for index in 0..count[0].max(count[1]) {
                pacing.tick().await;
                if pacing.check(acted)? {
                    return Ok(());
                }
                let [delta_x, delta_y] = std::array::from_fn(|axis| {
                    if index < count[axis] {
                        toward[axis] * motion::NOTCH_DELTA
                    } else {
                        0.0
                    }
                });
                let params = json!({
                    "type": "mouseWheel", "x": x, "y": y, "deltaX": delta_x, "deltaY": delta_y,
                });
                let atomic = display.atomic_input().await;
                self.turning_layout(display, acted)?;
                self.send(&params, to, &mapping, client, display, atomic)
                    .await?;
                acted = Acted::Turned;
                if index > 0 {
                    continue;
                }
                // The wheel has to reach the scroller: wait for it to move.
                let turned = Instant::now();
                loop {
                    pacing.tick().await;
                    if pacing.check(acted)? {
                        return Ok(());
                    }
                    let Some(now) = scroll.read(&mut pacing).await? else {
                        return Ok(());
                    };
                    if now.at != from.at {
                        at = now;
                        break;
                    }
                    if turned.elapsed() >= motion::WHEEL_STALL {
                        return scroll.by_script(&now).await;
                    }
                }
            }
            let turned = Instant::now();
            let mut still = 0;
            while still < SETTLED_FRAMES && turned.elapsed() < SETTLE_LIMIT {
                pacing.tick().await;
                if pacing.check(acted)? {
                    return Ok(());
                }
                let Some(now) = scroll.read(&mut pacing).await? else {
                    return Ok(());
                };
                still = if now.at == at.at { still + 1 } else { 0 };
                at = now;
            }
            per_notch = std::array::from_fn(|axis| {
                let moved = at.at[axis] - from.at[axis];
                learned(
                    per_notch[axis],
                    moved,
                    count[axis],
                    at.at[axis],
                    at.most[axis],
                )
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scroll_goes_as_far_as_its_scroller_can_go_now() {
        let at = Offsets {
            at: [0.0, 120.0],
            most: [0.0, 2000.0],
        };
        assert_eq!(at.remaining([0.0, 0.0], [0.0, 300.0]), [0.0, 180.0]);
        // Past either end the goal is the end.
        assert_eq!(at.remaining([0.0, 0.0], [0.0, 5000.0]), [0.0, 1880.0]);
        assert_eq!(at.remaining([0.0, 0.0], [-50.0, -300.0]), [0.0, -120.0]);
        // Content that grew lets it go further than it could at the start.
        let grown = Offsets {
            most: [0.0, 9000.0],
            ..at
        };
        assert_eq!(grown.remaining([0.0, 0.0], [0.0, 5000.0]), [0.0, 4880.0]);
        // A scroller that cannot move has nowhere to go.
        let fixed = Offsets {
            at: [0.0, 0.0],
            most: [-15.0, 0.0],
        };
        assert_eq!(fixed.remaining([0.0, 0.0], [300.0, 300.0]), [0.0, 0.0]);
    }

    #[test]
    fn the_distance_per_notch_is_learned_only_away_from_the_ends() {
        assert_eq!(learned(120.0, 300.0, 3, 300.0, 2000.0), 100.0);
        assert_eq!(learned(120.0, -240.0, 2, 760.0, 2000.0), 120.0);
        // It stopped at an end, did not move, or turned nothing: keep the
        // estimate.
        assert_eq!(learned(120.0, 90.0, 3, 2000.0, 2000.0), 120.0);
        assert_eq!(learned(120.0, -90.0, 3, 0.0, 2000.0), 120.0);
        assert_eq!(learned(120.0, 0.0, 3, 400.0, 2000.0), 120.0);
        assert_eq!(learned(120.0, 300.0, 0, 400.0, 2000.0), 120.0);
    }

    #[test]
    fn directions_are_signs_with_none_for_no_movement() {
        assert_eq!(direction(300.0), 1.0);
        assert_eq!(direction(-0.5), -1.0);
        assert_eq!(direction(0.0), 0.0);
        assert_eq!(direction(-0.0), 0.0);
        assert_eq!(direction(f64::NAN), 0.0);
    }
}
