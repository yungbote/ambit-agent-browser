//! The private X11 display of one locally owned Chrome process.
//!
//! The helper implements platform operations. Browser custody, command order,
//! frame pacing and observation identity remain in the native driver.
//!
//! Two channels reach the helper: the control channel (stdio) carries input,
//! geometry and clipboard operations in strict order; the frame channel
//! (an inherited descriptor) carries only captures. A capture in flight never
//! delays acknowledged native input.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub(crate) const MAX_DISPLAY_SIZE: u32 = 4096;
pub(crate) const DEVICE_SCALE_FACTOR: u32 = 2;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// The descriptor the helper serves captures on; see `DisplayProcess::spawn`.
const FRAME_CHANNEL_FD: i32 = 3;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Surface {
    pub kind: String,
    pub coordinate_space: String,
    pub generation: String,
    pub width: u32,
    pub height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub device_scale_factor: u32,
    /// The surface's raster space includes the native pointer. A frame captured
    /// for a controlling client omits the composited pointer (that client
    /// renders its own); the coordinate contract does not change with it.
    pub cursor_included: bool,
}

impl Surface {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            kind: "browser-window".into(),
            coordinate_space: "display-pixels".into(),
            generation: uuid::Uuid::new_v4().to_string(),
            width,
            height,
            origin_x: 0,
            origin_y: 0,
            device_scale_factor: DEVICE_SCALE_FACTOR,
            cursor_included: true,
        }
    }
}

/// A rectangle in display pixels.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub windows: Vec<WindowInfo>,
    pub focus_window: Option<u32>,
    /// The protocol extensions this helper serves (`info` answers them):
    /// `captureWait`, `cursorIdentity`, `layoutGate`, `sizeClass`. An older
    /// helper lists none and rejects their fields, so each is sent only when
    /// advertised.
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowInfo {
    pub id: u32,
    pub pid: Option<u32>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub mapped: bool,
    pub focused: bool,
    pub override_redirect: bool,
    pub window_type: String,
}

impl DisplayInfo {
    pub(crate) fn active_window(&self) -> Option<&WindowInfo> {
        let mut candidates = self.windows.iter().filter(|window| {
            window.pid.is_some_and(|pid| pid > 0)
                && window.mapped
                && !window.override_redirect
                && window.window_type == "normal"
        });
        let first = candidates.next()?;
        if candidates.next().is_none() {
            return Some(first);
        }
        let mut focused = self.windows.iter().filter(|window| {
            window.pid.is_some_and(|pid| pid > 0)
                && window.mapped
                && !window.override_redirect
                && window.window_type == "normal"
                && (window.focused || self.focus_window == Some(window.id))
        });
        let result = focused.next()?;
        focused.next().is_none().then_some(result)
    }
}

/// What one capture asks of the helper. `cursor` composites the native pointer
/// (and counts its movement as change); `budget_bytes` bounds the encoded frame
/// through the helper's adaptive quality (0 = no bound); `force` returns a
/// frame even when nothing changed, so a rotated surface generation can be
/// published on the same pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CaptureRequest {
    pub cursor: bool,
    pub budget_bytes: u32,
    pub force: bool,
    /// Every viewer composites damaged rectangles onto its last full frame,
    /// so the helper may answer with patches instead of a whole frame.
    pub patches: bool,
    /// How long an unchanged capture waits in the helper for damage, a new
    /// cursor identity or a finished layout before answering unchanged
    /// (`captureWait`); 0 answers at once.
    pub wait_ms: u32,
    /// Report the displayed cursor's identity when it changed since the
    /// helper last reported it (`cursorIdentity`).
    pub cursor_identity: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capture {
    pub width: u32,
    pub height: u32,
    pub encoding: String,
    /// A whole frame. Absent when the frame is carried as `patches`.
    #[serde(default)]
    pub data: Option<String>,
    /// Damaged rectangles, aligned to the encoder's block grid, that replace
    /// the same rectangles of the viewer's last whole frame.
    #[serde(default)]
    pub patches: Vec<Patch>,
    pub cursor_included: bool,
    /// The browser window within the frame, when the helper's framebuffer is
    /// a size class larger than the window. Absent: the whole frame.
    #[serde(default)]
    pub visible: Option<Rect>,
    #[serde(default)]
    #[cfg_attr(not(test), allow(dead_code))] // Retained for capture qualification.
    pub quality: u32,
    /// The helper's per-stage wall times for this capture, for measurement;
    /// `waitUs` is how long the helper held an unchanged answer before this
    /// frame's damage arrived.
    #[serde(default)]
    pub timings: Option<Value>,
}

impl Capture {
    /// Microseconds the helper waited before taking this frame.
    pub(crate) fn wait_us(&self) -> u64 {
        self.timings
            .as_ref()
            .and_then(|timings| timings["waitUs"].as_u64())
            .unwrap_or(0)
    }
}

/// One capture's answer: a frame when the display changed, and the displayed
/// cursor's identity when it changed and was asked for. Either may be absent.
#[derive(Debug, Default)]
pub(crate) struct Captured {
    pub frame: Option<(Capture, Surface)>,
    /// The helper's identity record: `serial`, `css` (a keyword or null) and,
    /// for a page's own cursor, `image` with its hotspot, scale and PNG.
    pub cursor: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Patch {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub data: String,
    /// Decoder padding keeps 4:2:0 chroma interpolation identical to a whole
    /// frame. The viewer draws this source crop at x/y, never the padding.
    #[serde(default)]
    pub source_x: u32,
    #[serde(default)]
    pub source_y: u32,
}

impl Capture {
    /// Whether this frame describes `surface`'s raster. A frame of another
    /// size was taken across a layout: stale, never a helper failure.
    fn fits(&self, surface: &Surface) -> bool {
        self.width == surface.width && self.height == surface.height
    }

    fn coherent(&self, request: CaptureRequest, surface: &Surface) -> bool {
        let whole = self.data.is_some();
        let patched = !self.patches.is_empty();
        // A forced frame republishes the whole surface; patches are only
        // ever answers to a request that allowed them.
        whole != patched
            && (request.patches && !request.force || !patched)
            && self.fits(surface)
            && self.visible.is_none_or(|visible| {
                visible.x >= 0
                    && visible.y >= 0
                    && visible.width > 0
                    && visible.height > 0
                    && (visible.x as u32)
                        .checked_add(visible.width)
                        .is_some_and(|edge| edge <= surface.width)
                    && (visible.y as u32)
                        .checked_add(visible.height)
                        .is_some_and(|edge| edge <= surface.height)
            })
            && self.encoding == "jpeg"
            && self.cursor_included == request.cursor
            && self.patches.len() <= 64
            && self.patches.iter().all(|patch| {
                !patch.data.is_empty()
                    && patch.source_x <= 16
                    && patch.source_y <= 16
                    && patch.x % 16 == 0
                    && patch.y % 16 == 0
                    && patch.width > 0
                    && patch.width <= 512
                    && patch.height > 0
                    && patch.height <= 512
                    && patch
                        .x
                        .checked_add(patch.width)
                        .is_some_and(|edge| edge <= surface.width)
                    && patch
                        .y
                        .checked_add(patch.height)
                        .is_some_and(|edge| edge <= surface.height)
            })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DisplayError {
    pub code: String,
    pub message: String,
    pub operation_performed: Option<Value>,
}

impl DisplayError {
    fn unavailable() -> Self {
        Self {
            code: "display_unavailable".into(),
            message: "The browser display is unavailable.".into(),
            operation_performed: Some(json!("unknown")),
        }
    }

    fn retired() -> Self {
        Self {
            code: "display_retired".into(),
            message: "The browser window is being replaced.".into(),
            operation_performed: Some(json!(false)),
        }
    }

    /// The frame loop skips these; they are not helper failures.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(
            self.code.as_str(),
            "display_layout_pending" | "display_frame_stale" | "display_retired"
        )
    }
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DisplayError {}

pub(crate) fn enabled() -> bool {
    std::env::var("AGENT_BROWSER_WINDOW_STREAM").is_ok_and(|value| value == "1")
}

/// The display pixels of an owned window of `width` × `height` CSS pixels.
pub(crate) fn window_pixels(width: u32, height: u32) -> Result<(u32, u32), String> {
    let maximum = MAX_DISPLAY_SIZE / DEVICE_SCALE_FACTOR;
    if !(1..=maximum).contains(&width) || !(1..=maximum).contains(&height) {
        return Err(format!(
            "Window dimensions must be between 1 and {maximum} CSS pixels"
        ));
    }
    Ok((width * DEVICE_SCALE_FACTOR, height * DEVICE_SCALE_FACTOR))
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    type Wire = BufReader<tokio::net::UnixStream>;

    /// How long a layout waits for the agent's held button to be released.
    const GESTURE_BOUND: std::time::Duration = std::time::Duration::from_secs(1);

    pub(crate) struct DisplayClient {
        identity: String,
        /// Ordered control operations: info, resize, input, reset, copy.
        control: tokio::sync::Mutex<Wire>,
        /// Captures only. Independent of the control channel so a frame in
        /// flight never delays native input.
        frames: tokio::sync::Mutex<Wire>,
        abort_sockets: [UnixStream; 2],
        failed: AtomicBool,
        /// Its owner ended this display on purpose: it stops producing frames
        /// without failing the view that showed it.
        retired: AtomicBool,
        next_id: AtomicU64,
        surface: RwLock<SurfaceState>,
        /// What the helper's `info` last advertised.
        features: RwLock<Vec<String>>,
        /// Agent pointer input and window layouts exclude each other: one
        /// atomic agent input (its geometry proof and its native send) holds
        /// this shared, a layout holds it exclusively.
        input_lease: tokio::sync::RwLock<()>,
        /// The agent's held button, as a nonzero hold id: a layout waits for
        /// it to end (a click between press and release, a drag), bounded by
        /// `GESTURE_BOUND`, so a resize does not land mid-gesture.
        gesture: tokio::sync::watch::Sender<u64>,
        next_gesture: AtomicU64,
        /// A hold that outlived the bound once: layouts no longer wait for
        /// it. Its next input finds the window changed and releases.
        overridden_gesture: AtomicU64,
    }

    /// Exclusive custody of the window geometry for one layout.
    pub(crate) struct LayoutGuard<'a>(#[allow(dead_code)] tokio::sync::RwLockWriteGuard<'a, ()>);

    /// Shared custody of the window geometry for one atomic agent input.
    pub(crate) type AtomicInput<'a> = tokio::sync::RwLockReadGuard<'a, ()>;

    struct SurfaceState {
        value: Surface,
        changed_at: std::time::Instant,
        ready: bool,
        /// The browser window's size in display pixels: the whole surface,
        /// or the top-left part of a size-class framebuffer.
        window: (u32, u32),
        /// Counts applied layouts, so the agent's next command can prove the
        /// page follows the newest one exactly once.
        layout_epoch: u64,
        /// The layout epoch the agent's page proof last ran at, and whether
        /// a JavaScript dialog kept the proof from the page.
        proof: Option<(u64, bool)>,
    }

    struct InFlight<'a> {
        client: &'a DisplayClient,
        complete: bool,
    }

    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            if !self.complete {
                self.client.abort();
            }
        }
    }

    struct Channel {
        client: UnixStream,
        helper: UnixStream,
    }

    fn channel() -> Result<Channel, String> {
        let (client, helper) = UnixStream::pair().map_err(|e| e.to_string())?;
        Ok(Channel { client, helper })
    }

    impl DisplayClient {
        fn new(
            control: UnixStream,
            frames: UnixStream,
            surface: Surface,
            ready: bool,
        ) -> Result<Arc<Self>, String> {
            let abort_sockets = [
                control.try_clone().map_err(|e| e.to_string())?,
                frames.try_clone().map_err(|e| e.to_string())?,
            ];
            let wire = |socket: UnixStream| -> Result<Wire, String> {
                socket.set_nonblocking(true).map_err(|e| e.to_string())?;
                Ok(BufReader::new(
                    tokio::net::UnixStream::from_std(socket).map_err(|e| e.to_string())?,
                ))
            };
            Ok(Arc::new(Self {
                identity: uuid::Uuid::new_v4().to_string(),
                control: tokio::sync::Mutex::new(wire(control)?),
                frames: tokio::sync::Mutex::new(wire(frames)?),
                abort_sockets,
                failed: AtomicBool::new(false),
                retired: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
                surface: RwLock::new(SurfaceState {
                    window: (surface.width, surface.height),
                    value: surface,
                    changed_at: std::time::Instant::now(),
                    ready,
                    layout_epoch: 0,
                    proof: None,
                }),
                features: RwLock::new(Vec::new()),
                input_lease: tokio::sync::RwLock::new(()),
                gesture: tokio::sync::watch::channel(0).0,
                next_gesture: AtomicU64::new(1),
                overridden_gesture: AtomicU64::new(0),
            }))
        }

        /// As if the helper's `info` had listed these protocol extensions.
        #[cfg(test)]
        pub(crate) fn advertise(&self, features: &[&str]) {
            *self.features.write().unwrap() = features.iter().map(|f| f.to_string()).collect();
        }

        /// A client whose control and frame peers the test drives directly.
        #[cfg(test)]
        pub(crate) fn test_channel() -> (Arc<Self>, tokio::net::UnixStream, tokio::net::UnixStream)
        {
            let control = channel().unwrap();
            let frames = channel().unwrap();
            let client = Self::new(
                control.client,
                frames.client,
                Surface::new(2560, 1440),
                true,
            )
            .unwrap();
            let peer = |socket: UnixStream| {
                socket.set_nonblocking(true).unwrap();
                tokio::net::UnixStream::from_std(socket).unwrap()
            };
            (client, peer(control.helper), peer(frames.helper))
        }

        pub(crate) fn identity(&self) -> &str {
            &self.identity
        }
        pub(crate) fn surface(&self) -> Surface {
            self.surface.read().unwrap().value.clone()
        }

        /// The browser window's size in display pixels.
        pub(crate) fn window(&self) -> (u32, u32) {
            self.surface.read().unwrap().window
        }

        /// Moves on every applied layout.
        pub(crate) fn layout_epoch(&self) -> u64 {
            self.surface.read().unwrap().layout_epoch
        }

        /// Records the agent's page proof of the layout at `epoch`: the page
        /// follows the window, or a JavaScript dialog kept the proof from it.
        pub(crate) fn record_proof(&self, epoch: u64, page_blocked: bool) {
            self.surface.write().unwrap().proof = Some((epoch, page_blocked));
        }

        /// The newest page proof: its layout epoch and whether a dialog
        /// blocked it.
        pub(crate) fn proof(&self) -> Option<(u64, bool)> {
            self.surface.read().unwrap().proof
        }

        /// Whether the page was proven to follow the window's current
        /// layout. Agent pointer input is sent only then: coordinates the
        /// agent resolved before a newer layout (a person's resize landing
        /// during a command or a Playwright program) may name another
        /// element now.
        pub(crate) fn layout_proven(&self) -> bool {
            let state = self.surface.read().unwrap();
            state.proof == Some((state.layout_epoch, false))
        }

        /// Whether the helper advertised a protocol extension.
        pub(crate) fn has(&self, feature: &str) -> bool {
            self.features
                .read()
                .unwrap()
                .iter()
                .any(|advertised| advertised == feature)
        }

        /// Shared custody for one atomic agent pointer input: no layout
        /// lands between its geometry proof and its native send.
        pub(crate) async fn atomic_input(&self) -> AtomicInput<'_> {
            self.input_lease.read().await
        }

        /// Whether the agent holds a button. Layouts wait for a held
        /// gesture to end, within `GESTURE_BOUND`.
        pub(crate) fn set_gesture(&self, held: bool) {
            self.gesture.send_if_modified(|hold| match (held, *hold) {
                (true, 0) => {
                    *hold = self.next_gesture.fetch_add(1, Ordering::Relaxed);
                    true
                }
                (false, 1..) => {
                    *hold = 0;
                    true
                }
                _ => false,
            });
        }

        /// Exclusive custody for a layout: after the atomic agent input in
        /// flight, and after a held gesture ends or outlives its bound.
        pub(crate) async fn layout(&self) -> LayoutGuard<'_> {
            let deadline = tokio::time::Instant::now() + GESTURE_BOUND;
            let mut hold = self.gesture.subscribe();
            loop {
                // While the exclusive lease is held no new gesture can start,
                // so a hold observed as ended here stays ended.
                let guard = self.input_lease.write().await;
                let held = *hold.borrow_and_update();
                if held == 0 || held == self.overridden_gesture.load(Ordering::Acquire) {
                    return LayoutGuard(guard);
                }
                if tokio::time::Instant::now() >= deadline {
                    self.overridden_gesture.store(held, Ordering::Release);
                    return LayoutGuard(guard);
                }
                drop(guard);
                let _ = tokio::time::timeout_at(deadline, hold.wait_for(|now| *now != held)).await;
            }
        }

        pub(crate) fn changed_since(&self, received_at: std::time::Instant) -> bool {
            self.surface.read().unwrap().changed_at > received_at
        }

        pub(crate) fn available(&self) -> bool {
            !self.failed.load(Ordering::Acquire)
        }

        pub(crate) fn ready(&self) -> bool {
            self.available() && self.surface.read().unwrap().ready
        }

        /// Called before the browser behind this display closes or is
        /// replaced. Its captures end as transient, never as a failure.
        pub(crate) fn retire(&self) {
            self.retired.store(true, Ordering::Release);
        }

        fn abort(&self) {
            self.failed.store(true, Ordering::Release);
            for socket in &self.abort_sockets {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }

        async fn command(
            &self,
            wire: &mut Wire,
            mut request: Value,
        ) -> Result<Value, DisplayError> {
            if !self.available() {
                return Err(DisplayError::unavailable());
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            request["id"] = json!(id);
            let mut encoded =
                serde_json::to_vec(&request).map_err(|_| DisplayError::unavailable())?;
            encoded.push(b'\n');
            let mut in_flight = InFlight {
                client: self,
                complete: false,
            };
            let result = tokio::time::timeout(RPC_TIMEOUT, async {
                wire.get_mut().write_all(&encoded).await?;
                let mut response = Vec::new();
                wire.take((MAX_RESPONSE_BYTES + 1) as u64)
                    .read_until(b'\n', &mut response)
                    .await?;
                if response.len() > MAX_RESPONSE_BYTES || response.last() != Some(&b'\n') {
                    return Err(std::io::Error::other("Invalid display response boundary"));
                }
                let response: Value = serde_json::from_slice(&response)?;
                if response["id"].as_u64() != Some(id) || !response["success"].is_boolean() {
                    return Err(std::io::Error::other("Invalid display response identity"));
                }
                Ok::<_, std::io::Error>(response)
            })
            .await;
            let response = match result {
                Ok(Ok(response)) => response,
                _ => {
                    // A partial response cannot become the next request's
                    // acknowledgment. EOF also asks the helper to release its
                    // injected native input; the command is never replayed.
                    self.abort();
                    return Err(DisplayError::unavailable());
                }
            };
            in_flight.complete = true;
            if response["success"] == true {
                return Ok(response["data"].clone());
            }
            serde_json::from_value(response["error"].clone())
                .map_err(|_| DisplayError::unavailable())
                .and_then(Err)
        }

        pub(crate) async fn request(&self, request: Value) -> Result<Value, DisplayError> {
            let mut wire = self.control.lock().await;
            self.command(&mut wire, request).await
        }

        pub(crate) async fn info(&self) -> Result<DisplayInfo, DisplayError> {
            let info: DisplayInfo =
                serde_json::from_value(self.request(json!({ "op": "info" })).await?)
                    .map_err(|_| DisplayError::unavailable())?;
            *self.features.write().unwrap() = info.features.clone();
            Ok(info)
        }

        /// Lays the window out at `width` × `height` display pixels under
        /// `layout` custody. With `size_class` (only when the helper
        /// advertises it) the framebuffer may stay larger than the window, so
        /// a drag never reallocates it; the surface is then the framebuffer
        /// and `window()` the window. The surface generation names the
        /// window, not its size: it does not move here, so a person's input
        /// and the frames continue across a resize. Every answered attempt
        /// moves the layout epoch, which the agent's next command proves.
        pub(crate) async fn resize(
            &self,
            _layout: &LayoutGuard<'_>,
            width: u32,
            height: u32,
            window_id: Option<u32>,
            size_class: bool,
        ) -> Result<DisplayInfo, DisplayError> {
            let size_class = size_class && self.has("sizeClass");
            let mut wire = self.control.lock().await;
            // Geometry can change before a failed or cancelled reply. Fence
            // queued agent coordinates on that possibility; captures wait
            // only when the helper cannot gate them on the paint itself.
            {
                let mut surface = self.surface.write().unwrap();
                surface.changed_at = std::time::Instant::now();
                surface.ready = false;
            }
            let mut request = json!({ "op": "resize", "width": width, "height": height, "windowId": window_id.unwrap_or(0) });
            if size_class {
                request["sizeClass"] = json!(true);
            }
            let answered = |surface: &mut SurfaceState| {
                surface.layout_epoch += 1;
                surface.changed_at = std::time::Instant::now();
                surface.ready = true;
            };
            let reply = match self.command(&mut wire, request).await {
                Ok(reply) => reply,
                Err(error) => {
                    // A refusal leaves the helper usable; its geometry is
                    // still proven again by the agent's next command.
                    if self.available() {
                        answered(&mut self.surface.write().unwrap());
                    }
                    return Err(error);
                }
            };
            let Ok(info) = serde_json::from_value::<DisplayInfo>(reply) else {
                self.abort();
                return Err(DisplayError::unavailable());
            };
            if !(1..=MAX_DISPLAY_SIZE).contains(&info.width)
                || !(1..=MAX_DISPLAY_SIZE).contains(&info.height)
                || info.width < width
                || info.height < height
                || !size_class && (info.width, info.height) != (width, height)
            {
                self.abort();
                return Err(DisplayError::unavailable());
            }
            let mut surface = self.surface.write().unwrap();
            surface.value.width = info.width;
            surface.value.height = info.height;
            surface.window = (width, height);
            answered(&mut surface);
            Ok(info)
        }

        pub(crate) async fn invalidate(&self) {
            let _wire = self.control.lock().await;
            let mut surface = self.surface.write().unwrap();
            surface.value.generation = uuid::Uuid::new_v4().to_string();
            surface.changed_at = std::time::Instant::now();
        }

        /// One capture: a frame when the display changed since the previous
        /// capture, and the cursor's identity when asked and changed. A
        /// generation rotated or a layout applied while the frame was in
        /// flight makes the frame stale (transient), never a helper failure,
        /// and so does retirement: a closing window is never published.
        pub(crate) async fn capture(
            &self,
            request: CaptureRequest,
        ) -> Result<Captured, DisplayError> {
            let captured = self.capture_current(request).await;
            if self.retired.load(Ordering::Acquire) {
                return Err(DisplayError::retired());
            }
            captured
        }

        fn stale() -> DisplayError {
            DisplayError {
                code: "display_frame_stale".into(),
                message: "The browser window changed while this frame was captured.".into(),
                operation_performed: Some(json!(false)),
            }
        }

        async fn capture_current(&self, request: CaptureRequest) -> Result<Captured, DisplayError> {
            if self.retired.load(Ordering::Acquire) {
                return Err(DisplayError::retired());
            }
            let mut wire = self.frames.lock().await;
            let before = {
                let state = self.surface.read().unwrap();
                // A helper that gates frames on the browser's own paint
                // answers unchanged through a layout; an older one must not
                // be asked while a resize is in flight.
                if !state.ready && !self.has("layoutGate") {
                    return Err(DisplayError {
                        code: "display_layout_pending".into(),
                        message: "The browser is applying its window layout.".into(),
                        operation_performed: Some(json!(false)),
                    });
                }
                state.value.clone()
            };
            let mut command = json!({
                "op": "capture", "cursor": request.cursor,
                "budgetBytes": request.budget_bytes, "force": request.force,
                "patches": request.patches,
            });
            if request.wait_ms > 0 {
                command["waitMs"] = json!(request.wait_ms);
            }
            if request.cursor_identity {
                command["cursorIdentity"] = json!(true);
            }
            let mut reply = self.command(&mut wire, command).await?;
            let cursor = reply
                .as_object_mut()
                .and_then(|reply| reply.remove("cursor"))
                .filter(|cursor| cursor.is_object());
            if reply["changed"] == false {
                return Ok(Captured {
                    frame: None,
                    cursor,
                });
            }
            let capture: Capture =
                serde_json::from_value(reply).map_err(|_| DisplayError::unavailable())?;
            let after = self.surface();
            if after.generation != before.generation
                || (after.width, after.height) != (before.width, before.height)
                || !capture.fits(&after)
            {
                return Err(Self::stale());
            }
            if !capture.coherent(request, &after) {
                self.abort();
                return Err(DisplayError::unavailable());
            }
            Ok(Captured {
                frame: Some((capture, after)),
                cursor,
            })
        }

        pub(crate) async fn input(&self, events: &[Value]) -> Result<(), DisplayError> {
            self.request(json!({ "op": "input", "events": events }))
                .await?;
            Ok(())
        }

        /// Releases every button and key the helper holds. A confirmed
        /// reset also ends the agent's held gesture, whoever asked for it
        /// (the agent's cleanup, a person taking control), so layouts stop
        /// waiting for a button that is no longer down.
        pub(crate) async fn reset(&self) -> Result<(), DisplayError> {
            self.request(json!({ "op": "reset" })).await?;
            self.set_gesture(false);
            Ok(())
        }
    }

    pub(crate) struct DisplayProcess {
        child: Child,
        pub(crate) client: Arc<DisplayClient>,
    }

    impl DisplayProcess {
        pub(crate) fn spawn(
            display: &str,
            authority: &Path,
            chrome_pid: u32,
        ) -> Result<Self, String> {
            let executable = std::env::var_os("AGENT_BROWSER_DISPLAY_HELPER")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::current_exe()
                        .ok()?
                        .parent()
                        .map(|dir| dir.join("browser-display"))
                })
                .ok_or("Browser display helper path is unavailable")?;
            let control = channel()?;
            let frames = channel()?;
            let helper_out: OwnedFd = control
                .helper
                .try_clone()
                .map_err(|e| e.to_string())?
                .into();
            let helper_in: OwnedFd = control.helper.into();
            let frame_channel: OwnedFd = frames.helper.into();
            let mut command = Command::new(executable);
            command
                .args([
                    "--chrome-pid",
                    &chrome_pid.to_string(),
                    "--capture-fd",
                    &FRAME_CHANNEL_FD.to_string(),
                ])
                .env("DISPLAY", display)
                .env("XAUTHORITY", authority)
                .stdin(Stdio::from(helper_in))
                .stdout(Stdio::from(helper_out))
                .stderr(Stdio::null());
            let frame_fd = frame_channel.as_raw_fd();
            // SAFETY: dup2 is async-signal-safe and the only work done between
            // fork and exec. The duplicate has no close-on-exec flag, so the
            // helper inherits exactly this descriptor as its frame channel.
            unsafe {
                use std::os::unix::process::CommandExt;
                command.pre_exec(move || {
                    if libc::dup2(frame_fd, FRAME_CHANNEL_FD) == FRAME_CHANNEL_FD {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            let child = command
                .spawn()
                .map_err(|e| format!("Browser display helper could not start: {e}"))?;
            drop(frame_channel);
            let client = DisplayClient::new(
                control.client,
                frames.client,
                Surface::new(MAX_DISPLAY_SIZE, MAX_DISPLAY_SIZE),
                false,
            )?;
            Ok(Self { child, client })
        }
    }

    impl Drop for DisplayProcess {
        fn drop(&mut self) {
            self.client.retire();
            self.client.abort();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => break,
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::io::AsyncWriteExt;
        use tokio::sync::oneshot;

        async fn reply(peer: &mut BufReader<tokio::net::UnixStream>, value: Value) {
            let body = format!("{value}\n");
            peer.get_mut().write_all(body.as_bytes()).await.unwrap();
        }

        async fn read_request(peer: &mut BufReader<tokio::net::UnixStream>) -> Value {
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            serde_json::from_str(&line).unwrap()
        }

        fn frame(width: u32, height: u32, cursor: bool) -> Value {
            json!({"changed":true,"width":width,"height":height,"encoding":"jpeg","data":"AA==","cursorIncluded":cursor,"quality":85})
        }

        const CAPTURE: CaptureRequest = CaptureRequest {
            cursor: true,
            budget_bytes: 0,
            force: false,
            patches: false,
            wait_ms: 0,
            cursor_identity: false,
        };

        /// Answers one `resize` on the control peer with a framebuffer.
        async fn answer_resize(
            peer: &mut BufReader<tokio::net::UnixStream>,
            framebuffer: (u32, u32),
        ) -> Value {
            let request = read_request(peer).await;
            assert_eq!(request["op"], "resize");
            reply(
                peer,
                json!({"id":request["id"],"success":true,
                    "data":{"width":framebuffer.0,"height":framebuffer.1,"windows":[]}}),
            )
            .await;
            request
        }

        /// A layout lands between atomic agent inputs, never inside one, and
        /// after a held button is released; a hold that outlives the bound
        /// stops holding layouts back, once.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_layout_waits_for_the_atomic_input_and_a_held_button_within_its_bound() {
            let (display, _peer, _frames) = DisplayClient::test_channel();
            let atomic = display.atomic_input().await;
            let owner = display.clone();
            let layout = tokio::spawn(async move {
                let _guard = owner.layout().await;
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                !layout.is_finished(),
                "a layout waits for the input in flight"
            );
            drop(atomic);
            tokio::time::timeout(Duration::from_secs(1), layout)
                .await
                .expect("the layout follows the input")
                .unwrap();

            display.set_gesture(true);
            let owner = display.clone();
            let layout = tokio::spawn(async move {
                let _guard = owner.layout().await;
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!layout.is_finished(), "a layout waits for a held button");
            // Input continues while the layout waits for the release.
            drop(display.atomic_input().await);
            display.set_gesture(false);
            tokio::time::timeout(Duration::from_millis(500), layout)
                .await
                .expect("the layout follows the release")
                .unwrap();

            display.set_gesture(true);
            let started = std::time::Instant::now();
            drop(display.layout().await);
            let waited = started.elapsed();
            assert!(
                waited >= GESTURE_BOUND && waited < GESTURE_BOUND * 2,
                "a stuck hold is waited out once: {waited:?}"
            );
            let started = std::time::Instant::now();
            drop(display.layout().await);
            assert!(started.elapsed() < Duration::from_millis(100));
        }

        /// A resize keeps the window's surface generation, so a person's
        /// input and the frames continue across it, and moves the layout
        /// epoch the agent's next command proves. The framebuffer may be a
        /// size class larger than the window only when the helper serves
        /// one; a refused resize still moves the epoch.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_resize_keeps_the_generation_and_moves_the_layout_epoch() {
            let (display, peer, _frames) = DisplayClient::test_channel();
            let generation = display.surface().generation;
            let mut peer = BufReader::new(peer);
            let owner = display.clone();
            let resized = tokio::spawn(async move {
                let layout = owner.layout().await;
                owner
                    .resize(&layout, 1560, 1200, Some(7), true)
                    .await
                    .map(|_| ())
            });
            let request = answer_resize(&mut peer, (1560, 1200)).await;
            assert!(
                request.get("sizeClass").is_none(),
                "not advertised: {request}"
            );
            resized.await.unwrap().unwrap();
            assert_eq!(display.surface().generation, generation);
            assert_eq!(display.layout_epoch(), 1);
            assert_eq!(display.window(), (1560, 1200));

            display.advertise(&["sizeClass"]);
            let owner = display.clone();
            let resized = tokio::spawn(async move {
                let layout = owner.layout().await;
                owner
                    .resize(&layout, 1400, 1100, Some(7), true)
                    .await
                    .map(|_| ())
            });
            let request = answer_resize(&mut peer, (1536, 1280)).await;
            assert_eq!(request["sizeClass"], true);
            resized.await.unwrap().unwrap();
            let surface = display.surface();
            assert_eq!((surface.width, surface.height), (1536, 1280));
            assert_eq!(surface.generation, generation);
            assert_eq!(display.window(), (1400, 1100));
            assert_eq!(display.layout_epoch(), 2);

            let owner = display.clone();
            let refused = tokio::spawn(async move {
                let layout = owner.layout().await;
                owner
                    .resize(&layout, 1400, 1000, Some(7), false)
                    .await
                    .map(|_| ())
            });
            let request = read_request(&mut peer).await;
            reply(&mut peer, json!({"id":request["id"],"success":false,"error":{"code":"display_window_missing","message":"missing","operationPerformed":false}})).await;
            assert!(refused.await.unwrap().is_err());
            assert!(display.available() && display.ready());
            assert_eq!(display.layout_epoch(), 3, "its geometry is proven again");

            // An exact resize answered with another size is a broken helper.
            let owner = display.clone();
            let broken = tokio::spawn(async move {
                let layout = owner.layout().await;
                owner
                    .resize(&layout, 1400, 1000, Some(7), false)
                    .await
                    .map(|_| ())
            });
            answer_resize(&mut peer, (1536, 1280)).await;
            assert!(broken.await.unwrap().is_err());
            assert!(!display.available());
        }

        /// A helper that gates frames on the browser's paint is captured
        /// through a layout; an older one is not asked while it lays out.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn captures_continue_through_a_layout_only_with_a_paint_gated_helper() {
            for gated in [false, true] {
                let (display, peer, frames) = DisplayClient::test_channel();
                if gated {
                    display.advertise(&["layoutGate"]);
                }
                let mut peer = BufReader::new(peer);
                let owner = display.clone();
                let resized = tokio::spawn(async move {
                    let layout = owner.layout().await;
                    owner
                        .resize(&layout, 1560, 1200, Some(7), false)
                        .await
                        .map(|_| ())
                });
                let request = read_request(&mut peer).await;
                assert_eq!(request["op"], "resize");
                assert!(!display.ready());
                let owner = display.clone();
                let capture = tokio::spawn(async move { owner.capture(CAPTURE).await });
                let mut frames = BufReader::new(frames);
                if gated {
                    let capture_request = read_request(&mut frames).await;
                    assert_eq!(capture_request["op"], "capture");
                    reply(
                        &mut frames,
                        json!({"id":capture_request["id"],"success":true,"data":{"changed":false}}),
                    )
                    .await;
                    assert!(capture.await.unwrap().unwrap().frame.is_none());
                } else {
                    let error = capture.await.unwrap().unwrap_err();
                    assert_eq!(error.code, "display_layout_pending");
                }
                reply(&mut peer, json!({"id":request["id"],"success":true,"data":{"width":1560,"height":1200,"windows":[]}})).await;
                resized.await.unwrap().unwrap();
            }
        }

        #[tokio::test]
        async fn reset_waits_for_the_prior_input_response_on_the_same_channel() {
            let (display, peer, _frames) = DisplayClient::test_channel();
            let (seen, received) = oneshot::channel();
            let (inspect, inspect_receiver) = oneshot::channel();
            let server = tokio::spawn(async move {
                let mut peer = BufReader::new(peer);
                let input = read_request(&mut peer).await;
                assert_eq!(input["op"], "input");
                seen.send(()).unwrap();
                inspect_receiver.await.unwrap();
                let mut byte = [0];
                assert_eq!(
                    peer.get_mut().try_read(&mut byte).unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                reply(&mut peer, json!({"id":input["id"],"success":false,"error":{"code":"test_input_unknown","message":"Input outcome unknown","operationPerformed":"unknown"}})).await;
                let reset = read_request(&mut peer).await;
                assert_eq!(reset["op"], "reset");
                reply(
                    &mut peer,
                    json!({"id":reset["id"],"success":true,"data":{}}),
                )
                .await;
            });
            let owner = display.clone();
            let input = tokio::spawn(async move { owner.input(&[]).await });
            received.await.unwrap();
            let owner = display.clone();
            let (started, start_received) = oneshot::channel();
            let reset = tokio::spawn(async move {
                started.send(()).unwrap();
                owner.reset().await
            });
            start_received.await.unwrap();
            inspect.send(()).unwrap();
            assert_eq!(input.await.unwrap().unwrap_err().code, "test_input_unknown");
            reset.await.unwrap().unwrap();
            server.await.unwrap();
        }

        #[tokio::test]
        async fn cancelled_input_aborts_both_channels_before_any_reset_can_be_acknowledged() {
            let (display, peer, frames) = DisplayClient::test_channel();
            let (seen, received) = oneshot::channel();
            let server = tokio::spawn(async move {
                let mut peer = BufReader::new(peer);
                let input = read_request(&mut peer).await;
                assert_eq!(input["op"], "input");
                seen.send(()).unwrap();
                let mut line = String::new();
                assert_eq!(
                    peer.read_line(&mut line).await.unwrap(),
                    0,
                    "cancelled input must close this channel, not queue reset or later input"
                );
                let mut frames = BufReader::new(frames);
                assert_eq!(
                    frames.read_line(&mut line).await.unwrap(),
                    0,
                    "the frame channel closes with the control channel"
                );
            });
            let owner = display.clone();
            let input = tokio::spawn(async move { owner.input(&[]).await });
            received.await.unwrap();
            input.abort();
            assert!(input.await.unwrap_err().is_cancelled());
            assert!(!display.available());
            assert!(display.reset().await.is_err());
            assert!(display.input(&[]).await.is_err());
            assert!(display.capture(CAPTURE).await.is_err());
            server.await.unwrap();
        }

        /// The point of the second channel: a capture the helper is still
        /// encoding cannot delay an input acknowledgment.
        #[tokio::test]
        async fn input_is_acknowledged_while_a_capture_is_in_flight() {
            let (display, peer, frames) = DisplayClient::test_channel();
            let (capture_seen, capture_received) = oneshot::channel();
            let (input_done, input_finished) = oneshot::channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let mut peer = BufReader::new(peer);
                let capture = read_request(&mut frames).await;
                assert_eq!(capture["op"], "capture");
                assert_eq!(capture["cursor"], false);
                assert_eq!(capture["budgetBytes"], 120000);
                capture_seen.send(()).unwrap();
                let input = read_request(&mut peer).await;
                assert_eq!(input["op"], "input");
                reply(
                    &mut peer,
                    json!({"id":input["id"],"success":true,"data":{}}),
                )
                .await;
                input_finished.await.unwrap();
                reply(
                    &mut frames,
                    json!({"id":capture["id"],"success":true,"data":frame(2560, 1440, false)}),
                )
                .await;
            });
            let owner = display.clone();
            let capture = tokio::spawn(async move {
                owner
                    .capture(CaptureRequest {
                        cursor: false,
                        budget_bytes: 120000,
                        force: false,
                        patches: false,
                        ..Default::default()
                    })
                    .await
            });
            capture_received.await.unwrap();
            display.input(&[]).await.unwrap();
            input_done.send(()).unwrap();
            let (captured, surface) = capture.await.unwrap().unwrap().frame.unwrap();
            assert!(!captured.cursor_included);
            assert_eq!(captured.quality, 85);
            assert_eq!(surface.width, 2560);
            helper.await.unwrap();
        }

        #[tokio::test]
        async fn unchanged_capture_yields_no_frame_and_a_rotated_generation_is_transient() {
            let (display, _peer, frames) = DisplayClient::test_channel();
            let (rotate, rotated) = oneshot::channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let first = read_request(&mut frames).await;
                reply(
                    &mut frames,
                    json!({"id":first["id"],"success":true,"data":{"changed":false}}),
                )
                .await;
                let second = read_request(&mut frames).await;
                assert_eq!(second["force"], true);
                rotate.send(()).unwrap();
                reply(
                    &mut frames,
                    json!({"id":second["id"],"success":true,"data":frame(2560, 1440, true)}),
                )
                .await;
                let third = read_request(&mut frames).await;
                reply(
                    &mut frames,
                    json!({"id":third["id"],"success":true,"data":frame(2560, 1440, true)}),
                )
                .await;
            });
            assert!(display.capture(CAPTURE).await.unwrap().frame.is_none());
            let owner = display.clone();
            let forced = tokio::spawn(async move {
                owner
                    .capture(CaptureRequest {
                        force: true,
                        ..CAPTURE
                    })
                    .await
            });
            rotated.await.unwrap();
            display.invalidate().await;
            let error = forced.await.unwrap().unwrap_err();
            assert_eq!(error.code, "display_frame_stale");
            assert!(error.is_transient());
            assert!(display.available());
            assert!(display.capture(CAPTURE).await.unwrap().frame.is_some());
            helper.await.unwrap();
        }

        #[tokio::test]
        async fn patches_are_accepted_only_when_requested_and_inside_the_surface() {
            let patch = |x: u32, y: u32, width: u32, height: u32| {
                json!({"changed":true,"width":2560,"height":1440,"encoding":"jpeg","cursorIncluded":true,"quality":75,
                    "patches":[{"x":x,"y":y,"width":width,"height":height,"data":"AA=="}]})
            };
            let (display, _peer, frames) = DisplayClient::test_channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let request = read_request(&mut frames).await;
                assert_eq!(request["patches"], true);
                reply(
                    &mut frames,
                    json!({"id":request["id"],"success":true,"data":patch(16, 32, 160, 48)}),
                )
                .await;
                let request = read_request(&mut frames).await;
                assert_eq!(request["patches"], false);
                reply(
                    &mut frames,
                    json!({"id":request["id"],"success":true,"data":patch(16, 32, 160, 48)}),
                )
                .await;
            });
            let patched = CaptureRequest {
                patches: true,
                ..CAPTURE
            };
            let (captured, _) = display.capture(patched).await.unwrap().frame.unwrap();
            assert!(captured.data.is_none());
            assert_eq!(captured.patches.len(), 1);
            assert_eq!(
                (captured.patches[0].x, captured.patches[0].width),
                (16, 160)
            );
            // Patches answered to a whole-frame request contradict it.
            assert_eq!(
                display.capture(CAPTURE).await.unwrap_err().code,
                "display_unavailable"
            );
            assert!(!display.available());
            helper.await.unwrap();

            // So do patches answered to a forced (republishing) request.
            let (display, _peer, frames) = DisplayClient::test_channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let request = read_request(&mut frames).await;
                assert_eq!(
                    (&request["patches"], &request["force"]),
                    (&json!(true), &json!(true))
                );
                reply(
                    &mut frames,
                    json!({"id":request["id"],"success":true,"data":patch(16, 32, 160, 48)}),
                )
                .await;
            });
            assert_eq!(
                display
                    .capture(CaptureRequest {
                        force: true,
                        ..patched
                    })
                    .await
                    .unwrap_err()
                    .code,
                "display_unavailable"
            );
            helper.await.unwrap();

            let (display, _peer, frames) = DisplayClient::test_channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let request = read_request(&mut frames).await;
                reply(
                    &mut frames,
                    json!({"id":request["id"],"success":true,"data":patch(2560, 0, 16, 16)}),
                )
                .await;
            });
            assert_eq!(
                display.capture(patched).await.unwrap_err().code,
                "display_unavailable"
            );
            helper.await.unwrap();
        }

        /// A display its owner is replacing stops publishing without failing
        /// the view: before, during and after the helper goes away.
        #[tokio::test]
        async fn a_retired_display_ends_its_captures_as_transient() {
            let (display, _peer, frames) = DisplayClient::test_channel();
            let (seen, received) = oneshot::channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let request = read_request(&mut frames).await;
                seen.send(()).unwrap();
                reply(
                    &mut frames,
                    json!({"id":request["id"],"success":true,"data":frame(2560, 1440, true)}),
                )
                .await;
                frames
            });
            let owner = display.clone();
            let in_flight = tokio::spawn(async move { owner.capture(CAPTURE).await });
            received.await.unwrap();
            display.retire();
            let error = in_flight.await.unwrap().unwrap_err();
            assert_eq!(error.code, "display_retired");
            assert!(error.is_transient());
            drop(helper.await.unwrap());
            display.abort();
            let error = display.capture(CAPTURE).await.unwrap_err();
            assert_eq!(error.code, "display_retired");
            assert!(error.is_transient());
        }

        #[tokio::test]
        async fn a_frame_that_contradicts_the_request_is_fatal() {
            let (display, _peer, frames) = DisplayClient::test_channel();
            let helper = tokio::spawn(async move {
                let mut frames = BufReader::new(frames);
                let request = read_request(&mut frames).await;
                reply(
                    &mut frames,
                    json!({"id":request["id"],"success":true,"data":frame(2560, 1440, false)}),
                )
                .await;
            });
            assert_eq!(
                display.capture(CAPTURE).await.unwrap_err().code,
                "display_unavailable"
            );
            assert!(!display.available());
            helper.await.unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use platform::{AtomicInput, DisplayClient, DisplayProcess};

/// There is no private X11 backend on other platforms. The uninhabited type
/// keeps the optional browser capability honest without a fake implementation.
#[cfg(not(target_os = "linux"))]
pub(crate) enum DisplayClient {}

#[cfg(not(target_os = "linux"))]
pub(crate) struct LayoutGuard<'a>(std::marker::PhantomData<&'a ()>);

#[cfg(not(target_os = "linux"))]
pub(crate) struct AtomicInput<'a>(std::marker::PhantomData<&'a ()>);

#[cfg(not(target_os = "linux"))]
impl DisplayClient {
    pub(crate) fn identity(&self) -> &str {
        match *self {}
    }
    pub(crate) fn surface(&self) -> Surface {
        match *self {}
    }
    pub(crate) fn available(&self) -> bool {
        match *self {}
    }
    pub(crate) fn ready(&self) -> bool {
        match *self {}
    }
    pub(crate) fn changed_since(&self, _: std::time::Instant) -> bool {
        match *self {}
    }
    pub(crate) fn window(&self) -> (u32, u32) {
        match *self {}
    }
    pub(crate) fn layout_epoch(&self) -> u64 {
        match *self {}
    }
    pub(crate) fn record_proof(&self, _: u64, _: bool) {
        match *self {}
    }
    pub(crate) fn proof(&self) -> Option<(u64, bool)> {
        match *self {}
    }
    pub(crate) fn layout_proven(&self) -> bool {
        match *self {}
    }
    pub(crate) fn has(&self, _: &str) -> bool {
        match *self {}
    }
    pub(crate) async fn atomic_input(&self) -> AtomicInput<'_> {
        match *self {}
    }

    pub(crate) fn set_gesture(&self, _: bool) {
        match *self {}
    }
    pub(crate) async fn layout(&self) -> LayoutGuard<'_> {
        match *self {}
    }
    pub(crate) fn retire(&self) {
        match *self {}
    }
    pub(crate) async fn request(&self, _: Value) -> Result<Value, DisplayError> {
        match *self {}
    }
    pub(crate) async fn info(&self) -> Result<DisplayInfo, DisplayError> {
        match *self {}
    }
    pub(crate) async fn resize(
        &self,
        _: &LayoutGuard<'_>,
        _: u32,
        _: u32,
        _: Option<u32>,
        _: bool,
    ) -> Result<DisplayInfo, DisplayError> {
        match *self {}
    }

    pub(crate) async fn invalidate(&self) {
        match *self {}
    }
    pub(crate) async fn capture(&self, _: CaptureRequest) -> Result<Captured, DisplayError> {
        match *self {}
    }
    pub(crate) async fn input(&self, _: &[Value]) -> Result<(), DisplayError> {
        match *self {}
    }
    pub(crate) async fn reset(&self) -> Result<(), DisplayError> {
        match *self {}
    }
}
