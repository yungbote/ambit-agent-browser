//! The agent's scrolling in the owned window through trusted input: the
//! pointer goes where the wheel reaches the scroller, and the wheel turns
//! until readback proves the result. A held gesture's native notches remain
//! paced per frame. `scroll` asks for a distance;
//! bringing an element into view (`scrollintoview`, and every native pointer
//! command before it reads its target) asks for the element's centre in the
//! middle of what each scroller hiding it shows, outermost first.
//!
//! The existing native mouse owner positions the visible pointer, then Chrome
//! receives precision wheel input in CSS pixels. The loop reads actual
//! displacement and visibility through settlement; it never changes DOM
//! scroll offsets or reports an input acknowledgement as a painted result.
//! Explicit wheel gestures and the human sign-in browser retain paced native
//! display input. Sign-in never uses this attached automation CDP path.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{stopped, Acted, Mapping, NativeMouse, Pacing};
use crate::native::actions::CommandError;
use crate::native::browser_control::interrupts::Interrupts;
use crate::native::browser_control::motion::{self, Glide};
use crate::native::cdp::client::CdpClient;
use crate::native::display::DisplayClient;

/// A scroller that has not moved for this many frames in a row has settled.
const SETTLED_FRAMES: u32 = 3;

/// The longest an unheld precision wheel may settle before refusing.
const SETTLE_LIMIT: Duration = Duration::from_secs(1);

// Selection of a hiding scroller and the distance that scroller moves must
// read the same clipped point. Compile the one projection into both read-only
// scripts so it cannot drift and adds no per-frame script allocation.
macro_rules! projected_target {
    () => {
        r#"function(element, stop) {
    const parent = el => el.assignedSlot || el.parentElement || (el.parentNode && el.parentNode.host) || null;
    const scrolls = el => {
        const style = getComputedStyle(el);
        return /^(auto|scroll|overlay)$/.test(style.overflowX) || /^(auto|scroll|overlay)$/.test(style.overflowY);
    };
    const box = element.getBoundingClientRect();
    let x = box.left + box.width / 2, y = box.top + box.height / 2;
    let hider = null, doc = element.ownerDocument, node = parent(element);
    const projects = (el, left, top) => {
        const outsideX = x < left || x >= left + el.clientWidth;
        const outsideY = y < top || y >= top + el.clientHeight;
        if (outsideX) x = left + el.clientWidth / 2;
        if (outsideY) y = top + el.clientHeight / 2;
        return outsideX || outsideY;
    };
    for (;;) {
        const root = doc.scrollingElement || doc.documentElement;
        for (; node && node !== root; node = parent(node)) {
            if (node === stop) return [x, y, hider];
            if (!scrolls(node)) continue;
            const rect = node.getBoundingClientRect();
            if (projects(node, rect.left + node.clientLeft, rect.top + node.clientTop)) hider = node;
        }
        if (root === stop) return [x, y, hider];
        if (projects(root, 0, 0)) hider = root;
        const frame = doc.defaultView.frameElement;
        if (!frame) return [x, y, hider];
        const rect = frame.getBoundingClientRect();
        x += rect.left + frame.clientLeft;
        y += rect.top + frame.clientTop;
        doc = frame.ownerDocument;
        node = parent(frame);
    }
}"#
    };
}

/// A scroller's offsets and limits, then its target's projected distance
/// and visibility per axis, all in that scroller's document's CSS pixels.
pub(super) const READ: &str = concat!(
    r#"function(element) {
    const offsets = [this.scrollLeft, this.scrollTop, this.scrollWidth - this.clientWidth, this.scrollHeight - this.clientHeight];
    if (!element) return offsets;
    const [x, y] = ("#,
    projected_target!(),
    r#")(element, this);
    const root = this.ownerDocument.scrollingElement || this.ownerDocument.documentElement;
    let left = 0, top = 0;
    if (this !== root) {
        const rect = this.getBoundingClientRect();
        left = rect.left + this.clientLeft;
        top = rect.top + this.clientTop;
    }
    const width = this.clientWidth, height = this.clientHeight;
    return offsets.concat([x - (left + width / 2), y - (top + height / 2),
        x < left || x >= left + width, y < top || y >= top + height]);
}"#
);

/// The outermost scroller hiding the target's visible projection, walking
/// through shadow trees and same-origin frames, or null when it shows.
pub(super) const HIDER: &str = concat!(
    "function() { return (",
    projected_target!(),
    ")(this, null)[2]; }"
);

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

/// What a scroll turns its scroller toward.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Goal<'a> {
    /// This far, in CSS pixels, from where the scroller started.
    By(Axes<f64>),
    /// Until this element's centre is in the middle of what it shows.
    Into(&'a str),
}

/// One read of a scroller, in CSS pixels: where it is, how far it can go,
/// and for an element, how far its centre is from the middle of what the
/// scroller shows and whether it lies outside it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    at: Axes<f64>,
    most: Axes<f64>,
    aim: Axes<f64>,
    outside: Axes<bool>,
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

async fn read(
    client: &CdpClient,
    session: &str,
    scroller: &str,
    goal: Goal<'_>,
) -> Result<Reading, String> {
    let arguments = match goal {
        Goal::By(_) => json!([]),
        Goal::Into(element) => json!([{ "objectId": element }]),
    };
    let value = call(client, session, scroller, READ, arguments).await?;
    let number = |index: usize| value[index].as_f64().filter(|value| value.is_finite());
    let (Some(x), Some(y), Some(most_x), Some(most_y)) =
        (number(0), number(1), number(2), number(3))
    else {
        return Err("The scroller could not be read.".into());
    };
    let aim = match goal {
        Goal::By(_) => [0.0, 0.0],
        Goal::Into(_) => match (number(4), number(5)) {
            (Some(aim_x), Some(aim_y)) => [aim_x, aim_y],
            _ => return Err("The scroller could not be read.".into()),
        },
    };
    Ok(Reading {
        at: [x, y],
        most: [most_x, most_y],
        aim,
        outside: [value[6] == true, value[7] == true],
    })
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

/// The outermost scroller hiding `element`'s centre (`HIDER`), if any.
async fn hider(client: &CdpClient, session: &str, element: &str) -> Result<Option<String>, String> {
    let found = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({ "objectId": element, "functionDeclaration": HIDER })),
            Some(session),
        )
        .await?;
    if found.get("exceptionDetails").is_some() {
        return Err("The element to bring into view could not be read.".into());
    }
    Ok(found["result"]["objectId"].as_str().map(str::to_owned))
}

/// The `<iframe>` element in `page_session` that shows the out-of-process
/// frame `session` renders, when it can be found.
async fn frame_owner(client: &CdpClient, page_session: &str, session: &str) -> Option<String> {
    let frame = client.target_for_session(session)?;
    let owner = client
        .send_command(
            "DOM.getFrameOwner",
            Some(json!({ "frameId": frame })),
            Some(page_session),
        )
        .await
        .ok()?;
    let node = client
        .send_command(
            "DOM.resolveNode",
            Some(json!({ "backendNodeId": owner["backendNodeId"] })),
            Some(page_session),
        )
        .await
        .ok()?;
    node["object"]["objectId"].as_str().map(str::to_owned)
}

/// Where a scroll is going, from its first reading.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Course<'a> {
    goal: Goal<'a>,
    /// Where the scroller started.
    start: Axes<f64>,
    /// The direction each axis moves: +1, -1, or 0 for an axis left alone.
    /// Into view, only an axis on which the centre lies outside moves.
    toward: Axes<f64>,
}

impl<'a> Course<'a> {
    fn new(goal: Goal<'a>, first: &Reading) -> Self {
        let toward = match goal {
            Goal::By(delta) => delta.map(direction),
            Goal::Into(_) => std::array::from_fn(|axis| {
                if first.outside[axis] {
                    direction(first.aim[axis])
                } else {
                    0.0
                }
            }),
        };
        Self {
            goal,
            start: first.at,
            toward,
        }
    }

    /// How far the scroller still has to go on each axis it moves, as far
    /// as it can go now: content that grows lets it go further.
    fn remaining(&self, now: &Reading) -> Axes<f64> {
        std::array::from_fn(|axis| {
            if self.toward[axis] == 0.0 {
                return 0.0;
            }
            let goal = match self.goal {
                Goal::By(delta) => self.start[axis] + delta[axis],
                Goal::Into(_) => now.at[axis] + now.aim[axis],
            };
            goal.clamp(0.0, now.most[axis].max(0.0)) - now.at[axis]
        })
    }
}

/// One scroll's scroller, and the course it takes.
struct Scroll<'a> {
    client: &'a CdpClient,
    session: &'a str,
    scroller: String,
    course: Course<'a>,
}

impl Scroll<'_> {
    async fn read(&self, pacing: &mut Pacing<'_>) -> Result<Option<Reading>, CommandError> {
        pacing
            .observe(read(
                self.client,
                self.session,
                &self.scroller,
                self.course.goal,
            ))
            .await
    }
}

impl NativeMouse {
    /// Scrolls by `delta` CSS pixels, as far as the scroller can go, with the
    /// wheel: `scroller` is an object id in `session`, or `None` for that
    /// page's own scroller. `page_session` is the tab's page, whose dialogs
    /// end the scroll as well as `session`'s. Returns true when a dialog
    /// opened.
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
    ) -> Result<bool, CommandError> {
        self.require_known()?;
        let goal = Goal::By([delta.0, delta.1]);
        let result = self
            .scroll_with_wheel(
                client,
                display,
                (page_session, session),
                scroller,
                goal,
                interrupts,
            )
            .await;
        self.finish(result, display).await
    }

    /// Brings `element` (an object id in `session`) into view as a person
    /// does: each scroller hiding its centre, outermost first, is turned by
    /// the wheel until the centre is in the middle of what it shows. An
    /// element of an out-of-process frame first has its frame brought into
    /// view in the page. Nothing moves while the centre shows. A target still
    /// hidden after wheel input fails; it is never revealed by a DOM mutation.
    /// Returns true when a dialog opened.
    pub(in crate::native::browser_control) async fn scroll_into_view(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        (page_session, session): (&str, &str),
        element: &str,
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        self.require_known()?;
        let result = self
            .bring_into_view(
                client,
                display,
                (page_session, session),
                element,
                interrupts,
            )
            .await;
        self.finish(result, display).await
    }

    async fn bring_into_view(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        (page_session, session): (&str, &str),
        element: &str,
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        let mut targets = Vec::with_capacity(2);
        if session != page_session {
            if let Some(owner) = frame_owner(client, page_session, session).await {
                targets.push((page_session, owner));
            }
        }
        targets.push((session, element.to_owned()));
        for (session, element) in &targets {
            let mut hiding = hider(client, session, element).await?;
            while let Some(scroller) = hiding {
                if self
                    .scroll_with_wheel(
                        client,
                        display,
                        (page_session, session),
                        Some(&scroller),
                        Goal::Into(element),
                        interrupts,
                    )
                    .await?
                {
                    return Ok(true);
                }
                hiding = hider(client, session, element).await?;
                if let Some(next) = hiding.as_ref() {
                    // Remote object ids are handles, not node identities.
                    // Reject a non-advancing hiding container, without an
                    // arbitrary nesting limit or another input attempt.
                    let unchanged = call(
                        client,
                        session,
                        &scroller,
                        "function(next) { return this === next; }",
                        json!([{"objectId":next}]),
                    )
                    .await?;
                    if unchanged.as_bool().ok_or("The browser could not confirm progress through the hiding scroll container.")? {
                        return Err("The wheel could not reveal the target. Inspect the page and choose a visible control; do not replay the action.".into());
                    }
                }
            }
        }
        Ok(false)
    }

    async fn scroll_with_wheel(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        (page_session, session): (&str, &str),
        scroller: Option<&str>,
        goal: Goal<'_>,
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
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
        let Some(first) = pacing
            .observe(read(client, session, &scroller, goal))
            .await?
        else {
            return Ok(true);
        };
        let scroll = Scroll {
            client,
            session,
            scroller,
            course: Course::new(goal, &first),
        };
        let surface = display.surface();
        let scale = mapping.scale()?;
        if scroll.course.remaining(&first) == [0.0, 0.0] {
            if let Goal::Into(_) = goal {
                if first.outside.iter().any(|outside| *outside) {
                    // Preserve the canonical visible-point refusal for a
                    // fixed offscreen target the scroller cannot reach.
                    return Err("The target is outside the visible page or clipped by a container the wheel could not reveal, so no input was sent to it. Inspect the page before continuing.".into());
                }
            }
            return Ok(false);
        }
        let pointer = display
            .pointer()
            .and_then(|pointer| mapping.page(pointer, &surface).ok());
        let point = wheel_point(
            client,
            session,
            &scroll.scroller,
            pointer,
            scroll.course.toward,
        );
        let Some(found) = pacing.observe(point).await? else {
            return Ok(true);
        };
        let Some((to, (x, y), width)) = found.and_then(|(x, y, width)| {
            let to = mapping.point(x, y, &surface).ok()?;
            Some((to, (x, y), width))
        }) else {
            return Err("No visible point can deliver wheel input to this scroller. Choose a visible scroll target.".into());
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
                return Ok(true);
            }
        }
        self.precision_scroll(
            &scroll,
            &first,
            (x, y),
            &mapping,
            display,
            &mut pacing,
            Acted::Nothing,
        )
        .await
    }

    /// Chrome receives a real precision wheel event at the pointer the native
    /// owner already positioned. Custody and the original document mapping
    /// remain held through its acknowledgement; the existing CDP observer
    /// publishes that acknowledgement, rather than inventing a helper receipt.
    #[allow(clippy::too_many_arguments)]
    async fn precision_scroll(
        &mut self,
        scroll: &Scroll<'_>,
        first: &Reading,
        (x, y): (f64, f64),
        mapping: &Mapping,
        display: &DisplayClient,
        pacing: &mut Pacing<'_>,
        acted: Acted,
    ) -> Result<bool, CommandError> {
        let operation_started = Instant::now();
        let [delta_x, delta_y] = scroll.course.remaining(first);
        // Readback tolerance can finish an already executed wheel gesture;
        // it must not swallow a valid initial pixel/fractional request.
        if (delta_x == 0.0 && delta_y == 0.0)
            || (acted != Acted::Nothing && delta_x.abs() <= 1.0 && delta_y.abs() <= 1.0)
        {
            return Ok(false);
        }
        if pacing.check(acted)? {
            return Ok(true);
        }
        let atomic = display.atomic_input().await;
        self.current(mapping, scroll.client, display).await?;
        if pacing.check(acted)? {
            return Ok(true);
        }
        let params = json!({"type":"mouseWheel", "x":x, "y":y,
            "deltaX":delta_x, "deltaY":delta_y, "buttons":self.buttons,
            "modifiers":self.holding(0)});
        if self.buttons != 0 {
            // A held physical gesture stays on its input device until release.
            // Switching a drag to CDP wheel input loses Chrome's text anchor.
            // There is no script/selection repair and no learned retry loop.
            let surface = display.surface();
            let css_per_notch = 120.0 * f64::from(surface.device_scale_factor) / mapping.scale()?;
            let frames = ((motion::WHEEL_BUDGET - Duration::from_millis(250)).as_nanos()
                / motion::FRAME.as_nanos()) as u32;
            let events = [delta_x, delta_y].map(|delta| {
                motion::wheel_events((delta.abs() / css_per_notch).ceil() as u32, frames)
            });
            drop(atomic);
            let began = Instant::now();
            let point = mapping.point(x, y, &surface)?;
            for index in 0..events[0].len().max(events[1].len()) {
                pacing.tick().await;
                if pacing.check(Acted::Held)? {
                    return Ok(true);
                };
                if began.elapsed() >= motion::WHEEL_BUDGET - Duration::from_millis(250) {
                    break;
                }
                let mut event = params.clone();
                event["deltaX"] = json!(
                    delta_x.signum()
                        * f64::from(events[0].get(index).copied().unwrap_or(0))
                        * motion::NOTCH_DELTA
                );
                event["deltaY"] = json!(
                    delta_y.signum()
                        * f64::from(events[1].get(index).copied().unwrap_or(0))
                        * motion::NOTCH_DELTA
                );
                let atomic = display.atomic_input().await;
                self.current(mapping, scroll.client, display).await?;
                self.send(&event, point, mapping, scroll.client, display, atomic)
                    .await?;
                pacing.sent();
            }
        } else {
            self.unknown = true;
            let mut command = scroll
                .client
                .enqueue_command(
                    "Input.dispatchMouseEvent",
                    Some(params),
                    Some(scroll.session),
                )
                .await?;
            let response = tokio::time::timeout(Duration::from_secs(5), command.acknowledgment())
            .await.map_err(|_| "browser_control_outcome_unknown: Precision wheel acknowledgement was lost. Inspect the page before continuing; do not replay the scroll.")??;
            if let Some(error) = response.error {
                return Err(format!("browser_control_outcome_unknown: Precision wheel input failed ({error}). Inspect the page before continuing.").into());
            }
            self.unknown = false;
            drop(atomic);
            pacing.sent();
        }
        let started = if self.buttons != 0 {
            operation_started
        } else {
            Instant::now()
        };
        let settle_limit = if self.buttons != 0 {
            motion::WHEEL_BUDGET
        } else {
            SETTLE_LIMIT
        };
        let mut now = *first;
        let mut still = 0;
        while started.elapsed() < settle_limit {
            pacing.tick().await;
            if pacing.check(Acted::Turned)? {
                return Ok(true);
            }
            self.current(mapping, scroll.client, display).await?;
            let Some(read) = scroll.read(pacing).await? else {
                return Ok(true);
            };
            still = if read.at == now.at { still + 1 } else { 0 };
            now = read;
            let left = scroll.course.remaining(&now);
            let reached = match scroll.course.goal {
                Goal::By(_) => {
                    left.iter().all(|value| value.abs() <= 1.0)
                        && (acted != Acted::Nothing
                            || [delta_x, delta_y].iter().enumerate().all(|(axis, delta)| {
                                *delta == 0.0
                                    || delta.signum() * (now.at[axis] - first.at[axis]) > 0.0
                            }))
                }
                Goal::Into(_) => !now.outside.iter().any(|value| *value),
            };
            if still >= SETTLED_FRAMES {
                if reached {
                    return Ok(false);
                }
                break;
            }
        }
        Err("The page did not reach the requested scroll position after trusted wheel input. It may block scrolling or have changed. Inspect the page before continuing; do not replay the scroll.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(at: Axes<f64>, most: Axes<f64>, aim: Axes<f64>, outside: Axes<bool>) -> Reading {
        Reading {
            at,
            most,
            aim,
            outside,
        }
    }

    #[test]
    fn a_scroll_by_a_distance_goes_as_far_as_its_scroller_can_go_now() {
        let first = reading([0.0, 120.0], [0.0, 2000.0], [0.0; 2], [false; 2]);
        let by = Course::new(Goal::By([0.0, 300.0]), &first);
        assert_eq!(by.toward, [0.0, 1.0]);
        assert_eq!(by.remaining(&first), [0.0, 300.0]);
        // Past either end the goal is the end.
        let far = Course::new(Goal::By([0.0, 5000.0]), &first);
        assert_eq!(far.remaining(&first), [0.0, 1880.0]);
        let back = Course::new(Goal::By([-50.0, -300.0]), &first);
        assert_eq!(back.remaining(&first), [0.0, -120.0]);
        // Content that grew lets it go further than it could at the start.
        let grown = reading([0.0, 120.0], [0.0, 9000.0], [0.0; 2], [false; 2]);
        assert_eq!(far.remaining(&grown), [0.0, 5000.0]);
        // A scroller that cannot move has nowhere to go.
        let fixed = reading([0.0, 0.0], [-15.0, 0.0], [0.0; 2], [false; 2]);
        let stuck = Course::new(Goal::By([300.0, 300.0]), &fixed);
        assert_eq!(stuck.remaining(&fixed), [0.0, 0.0]);
    }

    /// Into view: only an axis on which the centre lies outside moves, and it
    /// moves until the centre is in the middle, as far as the scroller goes.
    #[test]
    fn an_element_is_brought_to_the_middle_of_what_hides_it() {
        // The centre is 9000 px below the middle and outside; 40 px right of
        // the middle but inside.
        let first = reading([0.0, 0.0], [300.0, 12000.0], [40.0, 9000.0], [false, true]);
        let into = Course::new(Goal::Into("element"), &first);
        assert_eq!(into.toward, [0.0, 1.0]);
        assert_eq!(into.remaining(&first), [0.0, 9000.0]);
        // Part of the way there the goal is where the element is now.
        let later = reading(
            [0.0, 6000.0],
            [300.0, 12000.0],
            [40.0, 3100.0],
            [false, false],
        );
        assert_eq!(into.remaining(&later), [0.0, 3100.0]);
        // An element near the end is brought as far as the scroller goes.
        let end = reading(
            [0.0, 11900.0],
            [300.0, 12000.0],
            [0.0, 400.0],
            [false, false],
        );
        assert_eq!(into.remaining(&end), [0.0, 100.0]);
        // Above: upward.
        let above = reading([0.0, 5000.0], [0.0, 12000.0], [0.0, -2000.0], [false, true]);
        let up = Course::new(Goal::Into("element"), &above);
        assert_eq!(up.toward, [0.0, -1.0]);
        assert_eq!(up.remaining(&above), [0.0, -2000.0]);
    }
}
