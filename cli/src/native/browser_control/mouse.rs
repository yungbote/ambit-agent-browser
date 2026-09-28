//! Agent mouse transport for the browser's existing owned display.
//!
//! The pointer travels. An event that lands more than
//! `motion::REACHED_CSS_PX` from the real X pointer is preceded by a glide
//! there (`motion::Glide`): one helper sample per presenter frame, each
//! published to viewers once the helper acknowledged it, so a person watching
//! sees the input the driver really sends. A wheel turns one notch per frame
//! the same way (`motion::notches`; the `scroll` command's closed loop is in
//! `mouse_scroll.rs`). A pending interruption (a person taking control) stops
//! either at its next frame.
//!
//! The page-to-display mapping is measured natively, from the renderer's
//! trusted event for a one-pixel move where the pointer already is, and kept
//! per page session while its page generation, window and page geometry
//! stand. It is proven when a gesture starts and again before a press, and a
//! hit test at the end of travel refuses a press the aimed element no longer
//! receives. Only the display helper sends buttons.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::interrupts::{InterruptReason, Interrupts};
use super::motion::{self, Glide};
use crate::native::actions::CommandError;
use crate::native::activity::{InputSource, NativePointer};
use crate::native::cdp::client::CdpClient;
use crate::native::cdp::types::CdpEvent;
use crate::native::display::{AtomicInput, DisplayClient, Surface};

const GEOMETRY: &str = "({scale:devicePixelRatio*(visualViewport?.scale??1),width:innerWidth,height:innerHeight,offsetX:visualViewport?.offsetLeft??0,offsetY:visualViewport?.offsetTop??0})";

fn outcome_unknown(message: impl AsRef<str>) -> String {
    let message = message.as_ref();
    if message.starts_with("browser_control_outcome_unknown: ") {
        message.into()
    } else {
        format!("browser_control_outcome_unknown: {message}")
    }
}

const UNMEASURED: &str =
    "The browser did not report the native mouse position. No native button was sent.";

/// A person's view resized the window after the agent's page was last
/// proven: whatever a command or program resolved on the page may have
/// moved, so its pointer input is refused until the next command proves it.
const RESIZED: &str = "The browser window was resized after this page was last observed, so this mouse input was not sent. Observe the page before choosing the next action.";

/// How long a probe of the pointer waits for the renderer to report it.
const PROBE_WAIT: Duration = Duration::from_millis(100);

/// The smaller side, in CSS pixels, of a travel's target when nothing under
/// it is aimed at: a wheel turned somewhere else, or a held drag.
const UNAIMED_WIDTH_CSS: f64 = 40.0;

/// A CDP move whose trusted renderer event, when this page's own document
/// receives it, measures where page coordinates are on the native display.
/// Used only when the pointer cannot be measured where it is.
async fn pre_hover(
    client: &CdpClient,
    session: &str,
    x: f64,
    y: f64,
) -> Result<Option<NativePointer>, String> {
    let mut command = client
        .enqueue_command(
            "Input.dispatchMouseEvent",
            Some(json!({
                "type": "mouseMoved", "x": x, "y": y, "buttons": 0,
            })),
            Some(session),
        )
        .await?;
    let response = tokio::time::timeout(Duration::from_secs(5), command.acknowledgment())
        .await
        .map_err(|_| {
            "The browser did not acknowledge the pre-hover; inspect it before retrying."
        })??;
    if let Some(error) = response.error {
        return Err(format!("Browser pre-hover failed: {error}"));
    }
    Ok(command.native_pointer())
}

/// Chooses the visible point of this page's own document nearest the
/// viewport's centre. A point over an iframe, object or embed delivers its
/// events to that document instead, so it cannot measure this page.
const CALIBRATION_PROBE: &str = r#"(() => {
    const width = innerWidth, height = innerHeight, x = width / 2, y = height / 2;
    const own = (px, py) => {
        const element = document.elementFromPoint(px, py);
        return !!element && !/^(IFRAME|FRAME|OBJECT|EMBED)$/.test(element.tagName);
    };
    let best = null;
    for (let i = 0; i < 9; i++) for (let j = 0; j < 9; j++) {
        const px = Math.min(width - 1, Math.round((i + 0.5) * width / 9));
        const py = Math.min(height - 1, Math.round((j + 0.5) * height / 9));
        const distance = (px - x) ** 2 + (py - y) ** 2;
        if ((!best || distance < best.distance) && own(px, py)) best = { x: px, y: py, distance };
    }
    return { calibration: best && { x: best.x, y: best.y } };
})()"#;

/// The page-to-native mapping is one affine relation for the whole page, so
/// a page the pointer cannot be measured over (it is over a child frame or
/// the browser's own controls) is measured at its own visible point nearest
/// the viewport's centre. A CDP hover shows there until the travel's first
/// sample moves the real pointer.
async fn calibrate(client: &CdpClient, session: &str) -> Result<NativePointer, String> {
    let read = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({ "expression": CALIBRATION_PROBE, "returnByValue": true })),
            Some(session),
        )
        .await?;
    let view = &read["result"]["value"];
    if read.get("exceptionDetails").is_some() || !view.is_object() {
        return Err(UNMEASURED.into());
    }
    let (Some(calibration_x), Some(calibration_y)) = (
        view["calibration"]["x"].as_f64(),
        view["calibration"]["y"].as_f64(),
    ) else {
        return Err(UNMEASURED.into());
    };
    pre_hover(client, session, calibration_x, calibration_y)
        .await?
        .ok_or_else(|| UNMEASURED.into())
}

/// Where the element a press aims at is recorded, in the page's pointer
/// realm, and how narrow it is (its smaller side in CSS pixels).
fn aim_script(x: f64, y: f64) -> String {
    format!(
        r#"((x, y) => {{
            let doc = document, lx = x, ly = y, hit = doc.elementFromPoint(lx, ly);
            while (hit && (hit.tagName === 'IFRAME' || hit.tagName === 'FRAME') && hit.contentDocument) {{
                const frame = hit.getBoundingClientRect();
                lx -= frame.x + hit.clientLeft;
                ly -= frame.y + hit.clientTop;
                doc = hit.contentDocument;
                const inner = doc.elementFromPoint(lx, ly);
                if (!inner) break;
                hit = inner;
            }}
            globalThis.__ambitAim = hit;
            if (!hit) return null;
            const box = hit.getBoundingClientRect();
            return Math.min(box.width, box.height);
        }})({x}, {y})"#
    )
}

/// Resolves once the page has begun a frame: Chrome dispatches continuous
/// native input (a move, a wheel) at the next frame, so by then the page has
/// taken the pointer's last move. Bounded for a page that draws no frames.
const NEXT_FRAME: &str = "new Promise(resolve => { requestAnimationFrame(() => resolve(true)); setTimeout(() => resolve(true), 100); })";

/// Whether the aimed element still receives a press at the point, once the
/// page has taken the travel's last move: null when it does, otherwise what
/// receives it instead ('' when it left the page).
fn hit_test_script(x: f64, y: f64) -> String {
    format!(
        r#"{NEXT_FRAME}.then(() => {{
            const x = {x}, y = {y}, el = globalThis.__ambitAim;
            const blocker = !el || !el.isConnected ? '' : ({blocker})(document, el, x, y);
            return blocker === null ? null : {{ blocker, scrollX, scrollY }};
        }})"#,
        blocker = crate::native::element::BLOCKER_AT_JS,
    )
}

#[derive(Clone)]
struct Mapping {
    session: String,
    pointer: NativePointer,
    surface: String,
    window: (u32, i32, i32, u32, u32),
}

impl Mapping {
    /// Display pixels per CSS pixel of the page.
    fn scale(&self) -> Result<f64, String> {
        self.pointer.geometry["scale"]
            .as_f64()
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .ok_or_else(|| "Missing native pointer scale".into())
    }

    /// The page's viewport in CSS pixels, when the measurement reported it.
    fn viewport(&self) -> Option<(f64, f64)> {
        let geometry = &self.pointer.geometry;
        geometry["width"].as_f64().zip(geometry["height"].as_f64())
    }

    /// The display point for page point `(x, y)`: inside the page's viewport,
    /// on the screen and inside the browser window this mapping measured. A
    /// size-class framebuffer is larger than the window, and a press outside
    /// the window reaches no page.
    fn point(&self, x: f64, y: f64, surface: &Surface) -> Result<(f64, f64), String> {
        if let Some((width, height)) = self.viewport() {
            if !(x >= 0.0 && y >= 0.0 && x < width && y < height) {
                return Err(format!(
                    "The point ({x}, {y}) is outside the visible page ({width}x{height} CSS pixels), so no input was sent to it; scroll it into view or choose a visible point."
                ));
            }
        }
        let scale = self.scale()?;
        let factor = f64::from(surface.device_scale_factor);
        let x = (self.pointer.screen_x * factor + (x - self.pointer.client_x) * scale).round();
        let y = (self.pointer.screen_y * factor + (y - self.pointer.client_y) * scale).round();
        let (_, left, top, width, height) = self.window;
        let (left, top) = (f64::from(left), f64::from(top));
        if !x.is_finite()
            || !y.is_finite()
            || x < left.max(0.0)
            || y < top.max(0.0)
            || x >= (left + f64::from(width)).min(f64::from(surface.width))
            || y >= (top + f64::from(height)).min(f64::from(surface.height))
        {
            return Err("The mouse position is outside the current browser window.".into());
        }
        Ok((x, y))
    }

    /// The page point shown at display point `point`.
    fn page(&self, point: (f64, f64), surface: &Surface) -> Result<(f64, f64), String> {
        let scale = self.scale()?;
        let factor = f64::from(surface.device_scale_factor);
        Ok((
            self.pointer.client_x + (point.0 - self.pointer.screen_x * factor) / scale,
            self.pointer.client_y + (point.1 - self.pointer.screen_y * factor) / scale,
        ))
    }

    /// The display point at the centre of the page's viewport: where a
    /// pointer with no known place starts its travel.
    fn center(&self, surface: &Surface) -> Result<(f64, f64), String> {
        let (width, height) = self.viewport().ok_or(UNMEASURED)?;
        self.point((width / 2.0).floor(), (height / 2.0).floor(), surface)
    }
}

/// What a press at `point` must still reach: the element under it when the
/// travel there began, held in the page's pointer realm.
struct Aim {
    session: String,
    generation: String,
    context: i64,
    point: (f64, f64),
    /// The aimed element's smaller side in CSS pixels.
    width: Option<f64>,
}

impl Aim {
    fn holds(&self, mapping: &Mapping, point: (f64, f64)) -> bool {
        self.session == mapping.session
            && self.generation == mapping.pointer.page_generation
            && (self.point.0 - point.0).hypot(self.point.1 - point.1) <= motion::REACHED_CSS_PX
    }
}

/// What an unfinished pointer action has done at its target when it stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acted {
    /// Nothing: at most its travel moved the pointer.
    Nothing,
    /// A button is held; stopping releases it.
    Held,
    /// The wheel turned, so the page may have scrolled part of the way.
    Turned,
}

impl Acted {
    fn of(held: bool) -> Self {
        if held {
            Self::Held
        } else {
            Self::Nothing
        }
    }
}

/// What a pending interruption makes of input that has not finished: a
/// travel stopped before its target did nothing there; one stopped with a
/// button held or a wheel turning is interrupted, and a held button is
/// released.
fn stopped(reason: InterruptReason, acted: Acted) -> CommandError {
    let human =
        json!({"interruptedBy":"human","executionStopped":true,"effectsMayHaveOccurred":true});
    match (reason, acted) {
        (InterruptReason::HumanControl, Acted::Nothing) => "browser_controlled_by_user: The user took control of this browser before this pointer action reached its target, so nothing was pressed or scrolled. Wait until they release control before continuing.".into(),
        (InterruptReason::HumanControl, Acted::Held) => CommandError::with_data(
            "browser_operation_interrupted: The user took control of this browser during this held pointer gesture; its button was released. Inspect the page after they release control and do not replay the gesture.",
            human,
        ),
        (InterruptReason::HumanControl, Acted::Turned) => CommandError::with_data(
            "browser_operation_interrupted: The user took control of this browser while the wheel was turning, so the page scrolled only part of the way. Inspect the page after they release control and do not replay the scroll.",
            human,
        ),
        (_, acted) => CommandError::with_data(
            "browser_operation_interrupted: The browser owner stopped this pointer action before it finished. Inspect the page before continuing; do not replay it.",
            json!({"executionStopped":true,"effectsMayHaveOccurred":acted != Acted::Nothing}),
        ),
    }
}

/// A layout that lands while the wheel turns stops it: the notches before
/// it were sent, and the rest would land on content that moved.
const RESIZED_TURNING: &str = "browser_operation_interrupted: The browser window was resized while the wheel was turning, so the page scrolled only part of the way. Observe the page before choosing the next action; do not replay the scroll.";

/// Whether a CDP event is a JavaScript dialog opening for one of the pages
/// an action watches: it blocks their scripts until someone answers it.
fn opens_dialog(event: &CdpEvent, sessions: &[&str]) -> bool {
    event.method == "Page.javascriptDialogOpening"
        && event
            .session_id
            .as_deref()
            .is_none_or(|id| sessions.contains(&id))
}

/// The clock paced input runs on: one helper event per presenter frame.
/// A glide samples on absolute deadlines from a clock started one frame
/// before its first sample (`until`), so the first goes at once and a late
/// one never delays the rest; counted events (wheel notches) go once a frame
/// (`tick`). Between events it stops for a pending interruption and for a
/// JavaScript dialog opening for the page.
struct Pacing<'a> {
    clock: Instant,
    /// When the next counted event is due: none yet, so the first goes at
    /// once.
    next_tick: Option<Instant>,
    raised: tokio::sync::watch::Receiver<u64>,
    events: tokio::sync::broadcast::Receiver<CdpEvent>,
    interrupts: &'a Interrupts,
    dialog_sessions: &'a [&'a str],
    _paced: super::paced::Span,
}

impl<'a> Pacing<'a> {
    fn start(
        client: &CdpClient,
        interrupts: &'a Interrupts,
        dialog_sessions: &'a [&'a str],
    ) -> Self {
        let now = Instant::now();
        Self {
            clock: now.checked_sub(motion::FRAME).unwrap_or(now),
            next_tick: None,
            raised: interrupts.subscribe(),
            events: client.subscribe(),
            interrupts,
            dialog_sessions,
            _paced: super::paced::Span::begin(),
        }
    }

    fn elapsed(&self) -> Duration {
        self.clock.elapsed()
    }

    /// Whether the next event may go: a pending interruption stops the
    /// action (what that means depends on what it has `acted`), and a
    /// dialog that opened ends it (true).
    fn check(&mut self, acted: Acted) -> Result<bool, CommandError> {
        if let Some(reason) = self.interrupts.pending() {
            return Err(stopped(reason, acted));
        }
        loop {
            match self.events.try_recv() {
                Ok(event) if opens_dialog(&event, self.dialog_sessions) => return Ok(true),
                Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(false),
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    return Err(outcome_unknown(
                        "The browser disconnected during this pointer action. Do not replay it.",
                    )
                    .into())
                }
            }
        }
    }

    /// Waits until `next` on the clock, or less when an interruption is
    /// raised meanwhile.
    async fn until(&mut self, next: Duration) {
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.clock + next)) => {}
            _ = self.raised.changed() => {}
        }
    }

    /// Waits for the next counted event's frame: the first at once, then one
    /// frame after the one before. A late event goes at once and the next is
    /// a whole frame after it, so events never bunch and never come faster
    /// than one a frame. A raised interruption wakes it early.
    async fn tick(&mut self) {
        let now = Instant::now();
        let due = self.next_tick.unwrap_or(now);
        if due > now {
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(due)) => {}
                _ = self.raised.changed() => {}
            }
        }
        self.next_tick = Some(due.max(now) + motion::FRAME);
    }

    /// A page read, unless a dialog opens for the page first (`None`): the
    /// dialog blocks the page's scripts, so the read would wait for it.
    async fn observe<T>(
        &mut self,
        read: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<Option<T>, CommandError> {
        tokio::pin!(read);
        loop {
            tokio::select! {
                result = &mut read => return Ok(Some(result?)),
                event = self.events.recv() => match event {
                    Ok(event) if opens_dialog(&event, self.dialog_sessions) => return Ok(None),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(outcome_unknown(
                            "The browser disconnected during this pointer action. Do not replay it.",
                        )
                        .into())
                    }
                    _ => {}
                },
            }
        }
    }
}

/// Page coordinates of a mouse event: both finite, or both absent where the
/// event lands wherever the pointer is (a button pressed in place, a wheel).
fn coordinates(params: &Value) -> Result<Option<(f64, f64)>, String> {
    let absent = |key: &str| params.get(key).is_none_or(Value::is_null);
    if absent("x") && absent("y") {
        return Ok(None);
    }
    let x = params["x"]
        .as_f64()
        .filter(|x| x.is_finite())
        .ok_or("Invalid mouse x")?;
    let y = params["y"]
        .as_f64()
        .filter(|y| y.is_finite())
        .ok_or("Invalid mouse y")?;
    Ok(Some((x, y)))
}

/// The modifier bit (Alt 1, Control 2, Meta 4, Shift 8) a key event's key
/// is, or 0 for any other key.
fn modifier_bit(event: &Value) -> i64 {
    let code = event["code"].as_str().unwrap_or_default();
    let key = event["key"].as_str().unwrap_or_default();
    let name = code
        .strip_suffix("Left")
        .or_else(|| code.strip_suffix("Right"))
        .unwrap_or(key);
    match name {
        "Alt" => 1,
        "Control" => 2,
        "Meta" => 4,
        "Shift" => 8,
        _ => 0,
    }
}

/// The helper's identity of a key: its physical code, or its key when it
/// names none.
fn key_identity(event: &Value) -> Option<String> {
    event["code"]
        .as_str()
        .filter(|code| !code.is_empty() && *code != "Unidentified")
        .or_else(|| event["key"].as_str())
        .map(str::to_owned)
}

/// The agent's native input held through the display helper: pointer
/// mappings and aim, the buttons and modifiers its mouse events hold, and
/// the keys it pressed without releasing (a `keydown`).
#[derive(Default)]
pub(super) struct NativeMouse {
    /// Measured page-to-display mappings by page session, kept while each
    /// page generation, window and page geometry stand (`current`).
    mappings: HashMap<String, Mapping>,
    aim: Option<Aim>,
    buttons: i64,
    modifiers: i64,
    /// Keys held down, by identity, with the modifier bit each is. Every
    /// native event carries the held modifiers, so the helper, which sets
    /// modifier keys to each event's mask, never lets one go early.
    keys: BTreeMap<String, i64>,
    /// Set before the helper can perform input. Cancellation cannot clear it.
    unknown: bool,
}

impl NativeMouse {
    /// Layouts land between atomic inputs, not between commands, and a
    /// layout is proven only by the agent's next command. Checked under
    /// atomic input custody, so no layout lands between this check and the
    /// native send.
    fn same_layout(&self, display: &DisplayClient) -> Result<(), String> {
        if display.layout_proven() {
            Ok(())
        } else {
            Err(RESIZED.into())
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn require_known(&self) -> Result<(), String> {
        if self.unknown {
            return Err("browser_control_outcome_unknown: Native input release is unconfirmed. Close the browser before sending more mouse input; do not replay the original action.".into());
        }
        Ok(())
    }

    pub(super) fn needs_release(&self) -> bool {
        self.buttons != 0 || self.modifiers != 0 || !self.keys.is_empty() || self.unknown
    }

    /// The modifiers the held keys are.
    pub(super) fn held_modifiers(&self) -> i64 {
        self.keys.values().fold(0, |held, bit| held | bit)
    }

    /// A native event's modifier mask: the one it asks for, and those held.
    fn holding(&self, requested: i64) -> i64 {
        requested | self.held_modifiers()
    }

    /// Keyboard events the helper acknowledged: a press holds its key until
    /// its release.
    pub(super) fn keys_acknowledged(&mut self, events: &[Value]) {
        for event in events {
            let Some(identity) = key_identity(event) else {
                continue;
            };
            match event["eventType"].as_str() {
                Some("keyDown" | "rawKeyDown") => {
                    self.keys.insert(identity, modifier_bit(event));
                }
                Some("keyUp") => {
                    self.keys.remove(&identity);
                }
                _ => {}
            }
        }
    }

    /// Cleanup has its own real acknowledgement. It never turns an earlier
    /// action with an unknown outcome into an acknowledged successful action.
    pub(super) async fn release(&mut self, display: &DisplayClient) -> Result<(), String> {
        self.unknown = true;
        display.reset().await.map_err(|error| {
            outcome_unknown(format!("Native input release is unconfirmed ({error}). Close the browser before sending more mouse input."))
        })?;
        self.reset();
        Ok(())
    }

    /// A failure that may have left input held releases it. Only a stop the
    /// driver chose knows what it sent; any other failure after input may
    /// have acted, so its outcome is unknown.
    async fn finish(
        &mut self,
        result: Result<bool, CommandError>,
        display: &DisplayClient,
    ) -> Result<bool, CommandError> {
        let Err(mut failure) = result else {
            return result;
        };
        if self.needs_release() {
            if !failure.error.starts_with("browser_operation_interrupted: ") {
                failure.error = outcome_unknown(&failure.error);
            }
            if let Err(cleanup) = self.release(display).await {
                failure.error = format!("{} {cleanup}", outcome_unknown(&failure.error));
            }
        }
        Err(failure)
    }

    async fn current(
        &self,
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
    ) -> Result<(), String> {
        let session = mapping.session.as_str();
        if !display.ready()
            || mapping.surface != display.surface().generation
            || mapping.pointer.page_generation != client.page_generation(session)
            || mapping
                .pointer
                .source_page
                .as_ref()
                .is_some_and(|source| client.page_generation(&source.session) != source.generation)
        {
            return Err(
                "The browser changed during this mouse gesture. Inspect it before continuing."
                    .into(),
            );
        }
        let info = display.info().await.map_err(|error| error.to_string())?;
        let window = info
            .active_window()
            .ok_or("The active browser window is unavailable")?;
        if mapping.window != (window.id, window.x, window.y, window.width, window.height) {
            return Err("The browser window moved during this mouse gesture.".into());
        }
        let read = client.send_command("Runtime.evaluate", Some(json!({
            "expression": GEOMETRY, "contextId": mapping.pointer.context, "returnByValue": true,
        })), Some(session)).await?;
        if read.get("exceptionDetails").is_some()
            || read["result"]["value"] != mapping.pointer.geometry
        {
            return Err("The page layout changed during this mouse gesture.".into());
        }
        if let Some(source) = mapping.pointer.source_page.as_ref() {
            let alive = client.send_command("Runtime.evaluate", Some(json!({"expression":"true","contextId":source.context,"returnByValue":true})), Some(&source.session)).await?;
            if alive["result"]["value"] != true {
                return Err("The pointer frame changed during this mouse gesture.".into());
            }
        }
        Ok(())
    }

    /// The mapping of `session`, proven current: the kept one while its page,
    /// window and geometry stand, or a new measurement. A held gesture keeps
    /// the mapping it started with or fails.
    async fn prepare(
        &mut self,
        client: &CdpClient,
        session: &str,
        display: &DisplayClient,
    ) -> Result<Mapping, String> {
        if !display.ready() {
            return Err("The browser window is applying its layout.".into());
        }
        if let Some(mapping) = self.mappings.get(session).cloned() {
            match self.current(&mapping, client, display).await {
                Ok(()) => return Ok(mapping),
                Err(error) if self.buttons != 0 => return Err(error),
                Err(_) => {
                    self.mappings.remove(session);
                }
            }
        }
        if self.buttons != 0 {
            return Err("The held mouse gesture has no observed position in this page. Release the mouse before starting a new gesture.".into());
        }
        let info = display.info().await.map_err(|error| error.to_string())?;
        let window = info
            .active_window()
            .ok_or("The active browser window is unavailable")?;
        let window = (window.id, window.x, window.y, window.width, window.height);
        let (measured, moved) = self.probe(client, session, display, window).await?;
        let pointer = match measured {
            Some(pointer) => pointer,
            None => calibrate(client, session).await?,
        };
        let mapping = Mapping {
            session: session.into(),
            pointer,
            surface: display.surface().generation,
            window,
        };
        self.current(&mapping, client, display).await?;
        // The measurement moved the real pointer: viewers see it arrive
        // there before anything travels from it.
        if let Some(point) = moved {
            self.published(point, &mapping, client, display)?;
        }
        self.mappings.insert(session.into(), mapping.clone());
        Ok(mapping)
    }

    /// Measures the page natively: a one-pixel move where the pointer
    /// already is, too small to see, which the renderer under it reports as
    /// a trusted event. A pointer with no known place first appears at the
    /// window's centre and is measured there. The measurement is `None` when
    /// this page's own document does not receive the move (the pointer is
    /// over the browser's controls, a child frame or another window); the
    /// point is where the move went, when one was sent.
    async fn probe(
        &mut self,
        client: &CdpClient,
        session: &str,
        display: &DisplayClient,
        window: (u32, i32, i32, u32, u32),
    ) -> Result<(Option<NativePointer>, Option<(f64, f64)>), String> {
        let (_, left, top, width, height) = window;
        let (left, top) = (f64::from(left), f64::from(top));
        let (right, bottom) = (left + f64::from(width), top + f64::from(height));
        let center = (
            ((left + right) / 2.0).floor(),
            ((top + bottom) / 2.0).floor(),
        );
        let at = match display.pointer() {
            Some((x, y)) if x >= left + 1.0 && y >= top && x < right - 1.0 && y < bottom => {
                (if x < center.0 { x + 1.0 } else { x - 1.0 }, y)
            }
            _ => center,
        };
        let Ok(probe) = client.arm_pointer_probe(session).await else {
            return Ok((None, None));
        };
        let atomic = display.atomic_input().await;
        self.same_layout(display)?;
        self.send_move(at, self.modifiers, display).await?;
        drop(atomic);
        Ok((probe.measured(PROBE_WAIT).await, Some(at)))
    }

    /// Records what a press at `point` must reach, unless an aim there
    /// stands and this event does not travel: a press after its own move, or
    /// a second click in place, keeps the aim its travel began with. Returns
    /// the aimed element's smaller side in CSS pixels.
    async fn aim(
        &mut self,
        mapping: &Mapping,
        point: (f64, f64),
        travels: bool,
        client: &CdpClient,
    ) -> Result<Option<f64>, String> {
        if let Some(aim) = self
            .aim
            .as_ref()
            .filter(|aim| !travels && aim.holds(mapping, point))
        {
            return Ok(aim.width);
        }
        self.aim = None;
        let read = client
            .send_command(
                "Runtime.evaluate",
                Some(json!({
                    "expression": aim_script(point.0, point.1),
                    "contextId": mapping.pointer.context, "returnByValue": true,
                })),
                Some(&mapping.session),
            )
            .await?;
        if read.get("exceptionDetails").is_some() {
            return Err("The page could not be read at the pointer's target.".into());
        }
        let width = read["result"]["value"]
            .as_f64()
            .filter(|width| width.is_finite());
        self.aim = Some(Aim {
            session: mapping.session.clone(),
            generation: mapping.pointer.page_generation.clone(),
            context: mapping.pointer.context,
            point,
            width,
        });
        Ok(width)
    }

    /// The hit test at the end of travel: a press is sent only where the
    /// element its travel aimed at still receives it. A layer that opened
    /// during the travel (a hover menu, a banner) or a target that moved
    /// refuses the press; nothing is pressed.
    async fn hit_test(
        &self,
        mapping: &Mapping,
        point: (f64, f64),
        client: &CdpClient,
    ) -> Result<(), CommandError> {
        let Some(aim) = self.aim.as_ref().filter(|aim| aim.holds(mapping, point)) else {
            return Ok(());
        };
        let read = client
            .send_command(
                "Runtime.evaluate",
                Some(json!({
                    "expression": hit_test_script(point.0, point.1),
                    "contextId": aim.context, "returnByValue": true, "awaitPromise": true,
                })),
                Some(&aim.session),
            )
            .await?;
        let found = &read["result"]["value"];
        if read.get("exceptionDetails").is_none() && found.is_null() {
            return Ok(());
        }
        // The topmost node for the refusal's record. Hit testing takes the
        // document coordinates of the session's local root.
        let observed = match (found["scrollX"].as_f64(), found["scrollY"].as_f64()) {
            (Some(scroll_x), Some(scroll_y)) => client
                .send_command(
                    "DOM.getNodeForLocation",
                    Some(json!({
                        "x": (point.0 + scroll_x).round() as i64,
                        "y": (point.1 + scroll_y).round() as i64,
                        "includeUserAgentShadowDOM": true,
                    })),
                    Some(&aim.session),
                )
                .await
                .ok()
                .and_then(|node| node["backendNodeId"].as_i64()),
            _ => None,
        };
        let message = match found["blocker"].as_str() {
            Some("") | None => "browser_observation_stale: The target left the page while the pointer travelled to it, so no button was pressed. Observe the page before choosing the next action.".to_string(),
            Some(blocker) => format!("browser_observation_stale: Another element (<{blocker}>) now covers the target at the press point, so no button was pressed. Observe the page before choosing the next action."),
        };
        Err(CommandError::with_data(
            message,
            json!({ "precondition": "hitTest", "observed": observed }),
        ))
    }

    /// One pointer move to `point` (display pixels) with the buttons this
    /// gesture holds and `modifiers`, which the helper holds from then on.
    /// Its outcome is unknown until the helper acknowledges it.
    async fn send_move(
        &mut self,
        point: (f64, f64),
        modifiers: i64,
        display: &DisplayClient,
    ) -> Result<(), String> {
        let modifiers = self.holding(modifiers);
        let event = json!({
            "type": "input_mouse", "eventType": "mouseMoved", "x": point.0, "y": point.1,
            "buttons": self.buttons, "modifiers": modifiers,
        });
        self.unknown = true;
        if let Err(error) = display.input(&[event]).await {
            if error.operation_performed == Some(json!(false)) {
                self.unknown = false;
                return Err(error.to_string());
            }
            return Err(format!("browser_control_outcome_unknown: Native input may have executed ({error}). Do not replay the original action."));
        }
        self.modifiers = modifiers;
        self.unknown = false;
        Ok(())
    }

    /// One sample of a travel: sent under atomic input custody at a proven
    /// layout, then published to viewers as the agent's pointer.
    async fn sample(
        &mut self,
        point: (f64, f64),
        modifiers: i64,
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
    ) -> Result<(), String> {
        let atomic = display.atomic_input().await;
        self.same_layout(display)?;
        self.send_move(point, modifiers, display).await?;
        drop(atomic);
        self.published(point, mapping, client, display)
    }

    /// Publishes an acknowledged move of the real pointer to `point`
    /// (display pixels) to viewers, as the agent's pointer.
    fn published(
        &self,
        point: (f64, f64),
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
    ) -> Result<(), String> {
        let surface = display.surface();
        let (x, y) = mapping.page(point, &surface)?;
        let factor = f64::from(surface.device_scale_factor);
        client
            .observe_activity(
                json!({
                    "type": "pointer", "eventType": "move", "x": x, "y": y,
                    "buttons": self.buttons, "modifiers": self.modifiers,
                    "screenX": point.0 / factor, "screenY": point.1 / factor,
                }),
                &mapping.session,
                mapping.pointer.page_generation.clone(),
                InputSource::Agent,
            )
            .acknowledged();
        Ok(())
    }

    /// Travels along `glide`, one sample per frame on the travel's clock,
    /// the last on its destination. Stops at the first sample a pending
    /// interruption finds, and when a JavaScript dialog opens for this page
    /// (returns true). Intermediate samples prove nothing but the layout.
    #[allow(clippy::too_many_arguments)]
    async fn travel(
        &mut self,
        glide: Glide,
        modifiers: i64,
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
        dialog_sessions: &[&str],
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        let mut pacing = Pacing::start(client, interrupts, dialog_sessions);
        let mut last = None;
        loop {
            if pacing.check(Acted::of(self.buttons != 0))? {
                return Ok(true);
            }
            let sample = glide.sample(pacing.elapsed());
            let point = (sample.point.0.round(), sample.point.1.round());
            if last != Some(point) {
                self.sample(point, modifiers, mapping, client, display)
                    .await?;
                last = Some(point);
            }
            let Some(next) = sample.next else {
                return Ok(false);
            };
            pacing.until(next).await;
        }
    }

    /// Turns the wheel at `point` (display pixels; the page point is in
    /// `params`) one of `notches` per frame. The last goes through
    /// `dispatch_at`, so the page has taken the wheel when this returns;
    /// the others read nothing back. Returns true when a dialog opened.
    #[allow(clippy::too_many_arguments)]
    async fn turn(
        &mut self,
        params: Value,
        notches: Vec<(f64, f64)>,
        point: (f64, f64),
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
        dialog_sessions: &[&str],
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        let mut pacing = Pacing::start(client, interrupts, dialog_sessions);
        let last = notches.len().saturating_sub(1);
        for (index, (delta_x, delta_y)) in notches.into_iter().enumerate() {
            pacing.tick().await;
            let acted = if index == 0 {
                Acted::of(self.buttons != 0)
            } else {
                Acted::Turned
            };
            if pacing.check(acted)? {
                return Ok(true);
            }
            let mut notch = params.clone();
            notch["deltaX"] = json!(delta_x);
            notch["deltaY"] = json!(delta_y);
            let atomic = display.atomic_input().await;
            self.turning_layout(display, acted)?;
            if index == last {
                return Ok(self
                    .dispatch_at(
                        notch,
                        point,
                        mapping,
                        client,
                        display,
                        dialog_sessions,
                        atomic,
                    )
                    .await?);
            }
            self.send(&notch, point, mapping, client, display, atomic)
                .await?;
        }
        Ok(false)
    }

    /// `same_layout` for a wheel's next notch: once the wheel has turned, a
    /// layout that landed stops it part of the way.
    fn turning_layout(&self, display: &DisplayClient, acted: Acted) -> Result<(), CommandError> {
        self.same_layout(display).map_err(|refusal| {
            if acted == Acted::Turned {
                CommandError::with_data(
                    RESIZED_TURNING,
                    json!({"executionStopped":true,"effectsMayHaveOccurred":true}),
                )
            } else {
                refusal.into()
            }
        })
    }

    /// Where the pointer's travel starts: the real X pointer, or the page's
    /// centre while the pointer is off the screen or has no known place.
    fn origin(
        &self,
        mapping: &Mapping,
        display: &DisplayClient,
        surface: &Surface,
    ) -> Result<(f64, f64), String> {
        display
            .pointer()
            .filter(|(x, y)| {
                *x >= 0.0
                    && *y >= 0.0
                    && *x < f64::from(surface.width)
                    && *y < f64::from(surface.height)
            })
            .map_or_else(|| mapping.center(surface), Ok)
    }

    /// Where an event without coordinates lands: the page point under the X
    /// pointer. A wheel turns at the viewport's centre while the pointer's
    /// place is unknown or off the page; a button is pressed only in place.
    fn pointer_point(
        &self,
        event_type: &str,
        mapping: &Mapping,
        display: &DisplayClient,
        surface: &Surface,
    ) -> Result<(f64, f64), String> {
        let (width, height) = mapping.viewport().ok_or(UNMEASURED)?;
        let under = display
            .pointer()
            .map(|pointer| mapping.page(pointer, surface))
            .transpose()?
            .filter(|(x, y)| *x >= 0.0 && *y >= 0.0 && *x < width && *y < height);
        match (under, event_type) {
            (Some(point), _) => Ok(point),
            (None, "mouseWheel") => Ok(((width / 2.0).floor(), (height / 2.0).floor())),
            (None, _) => Err("The pointer is not over the page, so no mouse input was sent. Move the mouse onto the page first.".into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch(
        &mut self,
        params: Value,
        client: &CdpClient,
        session: &str,
        display: &DisplayClient,
        dialog_sessions: &[&str],
        allow_release_cleanup: bool,
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        // Do not turn another command into a retry of an unconfirmed reset.
        self.require_known()?;
        let result = self
            .gesture(
                params,
                client,
                session,
                display,
                dialog_sessions,
                allow_release_cleanup,
                interrupts,
            )
            .await;
        self.finish(result, display).await
    }

    /// One mouse event and the travel that brings the pointer to it: proven
    /// before it starts, stopped by a pending interruption, and for a press
    /// proven again and hit tested at the end of travel.
    #[allow(clippy::too_many_arguments)]
    async fn gesture(
        &mut self,
        mut params: Value,
        client: &CdpClient,
        session: &str,
        display: &DisplayClient,
        dialog_sessions: &[&str],
        allow_release_cleanup: bool,
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        let event_type = params["type"]
            .as_str()
            .filter(|kind| {
                matches!(
                    *kind,
                    "mouseMoved" | "mousePressed" | "mouseReleased" | "mouseWheel"
                )
            })
            .ok_or("Unsupported mouse event type")?
            .to_owned();
        let point = coordinates(&params)?;
        let notches = match event_type.as_str() {
            "mouseWheel" => Some(
                motion::notches((
                    params["deltaX"].as_f64().unwrap_or(0.0),
                    params["deltaY"].as_f64().unwrap_or(0.0),
                ))
                .ok_or("Invalid wheel delta: each axis must be finite and at most 32768")?,
            ),
            _ => None,
        };
        // A resize since the page was proven refuses before anything is sent.
        self.same_layout(display)?;
        if let Some(reason) = interrupts.pending() {
            return Err(stopped(reason, Acted::of(self.buttons != 0)));
        }
        let mapping = match self.prepare(client, session, display).await {
            Ok(mapping) => mapping,
            Err(error) => {
                // An explicit release needs no stale page coordinates.
                // Nothing was sent for this event yet.
                if allow_release_cleanup && event_type == "mouseReleased" && self.buttons != 0 {
                    self.release(display).await?;
                    return Ok(false);
                }
                return Err(error.into());
            }
        };
        let surface = display.surface();
        let (x, y) = match point {
            Some(point) => point,
            None => self.pointer_point(&event_type, &mapping, display, &surface)?,
        };
        params["x"] = json!(x);
        params["y"] = json!(y);
        let to = mapping.point(x, y, &surface)?;
        let scale = mapping.scale()?;
        let from = self.origin(&mapping, display, &surface)?;
        let reach = motion::REACHED_CSS_PX * scale;
        let travels = (to.0 - from.0).hypot(to.1 - from.1) > reach;
        let presses = event_type == "mousePressed" && self.buttons == 0;
        let width = if self.buttons == 0 && (presses || event_type == "mouseMoved") {
            self.aim(&mapping, (x, y), travels, client).await?
        } else {
            None
        };
        // The travel's samples carry the modifiers of the event they lead to.
        let modifiers = params["modifiers"].as_i64().unwrap_or(0);
        if let Some(glide) = Glide::new(from, to, width.unwrap_or(UNAIMED_WIDTH_CSS) * scale, reach)
        {
            if self
                .travel(
                    glide,
                    modifiers,
                    &mapping,
                    client,
                    display,
                    dialog_sessions,
                    interrupts,
                )
                .await?
            {
                return Ok(true);
            }
        }
        if presses {
            self.current(&mapping, client, display).await?;
            self.hit_test(&mapping, (x, y), client).await?;
            if let Some(reason) = interrupts.pending() {
                return Err(stopped(reason, Acted::Nothing));
            }
        }
        if let Some(notches) = notches {
            return self
                .turn(
                    params,
                    notches,
                    to,
                    &mapping,
                    client,
                    display,
                    dialog_sessions,
                    interrupts,
                )
                .await;
        }
        let atomic = display.atomic_input().await;
        self.same_layout(display)?;
        Ok(self
            .dispatch_at(
                params,
                to,
                &mapping,
                client,
                display,
                dialog_sessions,
                atomic,
            )
            .await?)
    }

    /// Sends one native mouse event at `point` (display pixels; the page
    /// point is in `params`) and publishes it to viewers once the helper
    /// acknowledged it. `atomic` is the input custody its geometry proof ran
    /// under: it ends at the helper's receipt.
    async fn send(
        &mut self,
        params: &Value,
        point: (f64, f64),
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
        atomic: AtomicInput<'_>,
    ) -> Result<(), String> {
        let event_type = params["type"].as_str().unwrap_or_default();
        // The helper changes one physical button per press/release. A caller's
        // claimed bitmask cannot clear the owner's actual acknowledged hold.
        let button = i64::from(crate::native::input::mouse_button_mask(
            params["button"].as_str().unwrap_or("left"),
        ));
        let buttons = match event_type {
            "mousePressed" => self.buttons | button,
            "mouseReleased" => self.buttons & !button,
            _ => self.buttons,
        };
        let modifiers = self.holding(params["modifiers"].as_i64().unwrap_or(0));
        let surface = display.surface();
        let mut event = crate::native::input::stream_event("input_mouse", params);
        event["x"] = json!(point.0);
        event["y"] = json!(point.1);
        event["buttons"] = json!(buttons);
        event["modifiers"] = json!(modifiers);
        let mut activity =
            crate::native::activity::from_command("Input.dispatchMouseEvent", params)
                .ok_or("Invalid mouse activity")?;
        activity["buttons"] = json!(buttons);
        activity["modifiers"] = json!(modifiers);
        activity["screenX"] = json!(point.0 / f64::from(surface.device_scale_factor));
        activity["screenY"] = json!(point.1 / f64::from(surface.device_scale_factor));
        let observation = client.observe_activity(
            activity,
            &mapping.session,
            mapping.pointer.page_generation.clone(),
            InputSource::Agent,
        );
        self.unknown = true;
        if let Err(error) = display.input(&[event]).await {
            if error.operation_performed == Some(json!(false)) {
                self.unknown = false;
                return Err(error.to_string());
            }
            return Err(format!("browser_control_outcome_unknown: Native input may have executed ({error}). Do not replay the original action."));
        }
        self.buttons = buttons;
        self.modifiers = modifiers;
        self.unknown = false;
        // A held button keeps layouts back until it is released.
        display.set_gesture(buttons != 0);
        drop(atomic);
        observation.acknowledged();
        Ok(())
    }

    /// Sends one native mouse event (`send`), then joins the page: the
    /// helper's receipt proves native dispatch, and the readback joins
    /// dialog observation. Returns true when a dialog opened.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_at(
        &mut self,
        params: Value,
        point: (f64, f64),
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
        dialog_sessions: &[&str],
        atomic: AtomicInput<'_>,
    ) -> Result<bool, String> {
        let session = mapping.session.as_str();
        let event_type = params["type"].as_str().ok_or("Missing mouse event type")?;
        if !matches!(
            event_type,
            "mouseMoved" | "mousePressed" | "mouseReleased" | "mouseWheel"
        ) {
            return Err("Unsupported mouse event type".into());
        }
        // Held moves may cross an iframe. Their real helper acknowledgement
        // proves movement; only button boundaries need a renderer/dialog join.
        let observe = !matches!(
            (event_type, self.buttons),
            ("mouseMoved", 1..) | ("mouseReleased", 0)
        );
        let mut events = client.subscribe();
        self.send(&params, point, mapping, client, display, atomic)
            .await?;
        if !observe {
            return Ok(false);
        }
        // A move or a wheel is taken at the page's next frame: a hover
        // promises the page has seen the pointer arrive when it returns.
        let expression = if matches!(event_type, "mouseMoved" | "mouseWheel") {
            NEXT_FRAME
        } else {
            "true"
        };
        let observed = client.send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression, "contextId": mapping.pointer.context,
                "awaitPromise": true, "returnByValue": true,
            })),
            Some(session),
        );
        tokio::pin!(observed);
        // The helper receipt proves native dispatch. This page readback joins
        // dialog observation; it does not claim DOM handling or a click effect.
        // Never cancel an in-flight helper RPC when a renderer dialog opens.
        loop {
            tokio::select! {
                result = &mut observed => {
                    let result = result.map_err(outcome_unknown)?;
                    if result["result"]["value"] != true { return Err(outcome_unknown("The current page could not be observed after native input. Inspect the page before retrying the action.")); }
                    return Ok(false);
                }
                event = events.recv() => match event {
                    Ok(event) if opens_dialog(&event, dialog_sessions) => return Ok(true),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err(outcome_unknown("The browser disconnected after native input. Do not replay the action.")),
                    _ => {},
                },
            }
        }
    }

    /// A drag is a press at the source after a travel there, a travel with
    /// the button held to the target, and the release there. Both pages are
    /// measured before the button goes down: a held gesture keeps the
    /// mapping it started with.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn drag(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        page_session: &str,
        source: (&str, f64, f64),
        target: (&str, f64, f64),
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        self.require_known()?;
        let result = self
            .drag_inner(client, display, page_session, source, target, interrupts)
            .await;
        self.finish(result, display).await
    }

    async fn drag_inner(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        page_session: &str,
        source: (&str, f64, f64),
        target: (&str, f64, f64),
        interrupts: &Interrupts,
    ) -> Result<bool, CommandError> {
        self.same_layout(display)?;
        if let Some(reason) = interrupts.pending() {
            return Err(stopped(reason, Acted::Nothing));
        }
        self.prepare(client, target.0, display).await?;
        let steps = [
            (
                source.0,
                json!({"type":"mousePressed","x":source.1,"y":source.2,"button":"left","buttons":1,"clickCount":1}),
            ),
            (
                target.0,
                json!({"type":"mouseMoved","x":target.1,"y":target.2,"button":"left","buttons":1}),
            ),
            (
                target.0,
                json!({"type":"mouseReleased","x":target.1,"y":target.2,"button":"left","buttons":0,"clickCount":1}),
            ),
        ];
        for (session, params) in steps {
            let dialog_sessions = [session, page_session];
            if self
                .gesture(
                    params,
                    client,
                    session,
                    display,
                    &dialog_sessions,
                    false,
                    interrupts,
                )
                .await?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[path = "mouse_scroll.rs"]
mod scroll;

#[cfg(test)]
#[path = "mouse_tests.rs"]
mod tests;
