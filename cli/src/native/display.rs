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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureRequest {
    pub cursor: bool,
    pub budget_bytes: u32,
    pub force: bool,
    /// Every viewer composites damaged rectangles onto its last full frame,
    /// so the helper may answer with patches instead of a whole frame.
    pub patches: bool,
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
    /// The helper's per-stage wall times for this capture, for measurement.
    #[serde(default)]
    #[cfg_attr(not(test), allow(dead_code))] // Measured in native capture qualification.
    pub timings: Option<Value>,
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
    fn coherent(&self, request: CaptureRequest, surface: &Surface) -> bool {
        let whole = self.data.is_some();
        let patched = !self.patches.is_empty();
        // A forced frame republishes the whole surface; patches are only
        // ever answers to a request that allowed them.
        whole != patched
            && (request.patches && !request.force || !patched)
            && self.width == surface.width
            && self.height == surface.height
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
    }

    struct SurfaceState {
        value: Surface,
        changed_at: std::time::Instant,
        ready: bool,
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
                    value: surface,
                    changed_at: std::time::Instant::now(),
                    ready,
                }),
            }))
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
            serde_json::from_value(self.request(json!({ "op": "info" })).await?)
                .map_err(|_| DisplayError::unavailable())
        }

        pub(crate) async fn resize(
            &self,
            width: u32,
            height: u32,
            window_id: Option<u32>,
        ) -> Result<DisplayInfo, DisplayError> {
            let mut wire = self.control.lock().await;
            // Geometry can change before a failed or cancelled reply. Fence
            // queued coordinates and frame publication before that effect.
            {
                let mut surface = self.surface.write().unwrap();
                surface.value.generation = uuid::Uuid::new_v4().to_string();
                surface.changed_at = std::time::Instant::now();
                surface.ready = false;
            }
            let info: DisplayInfo = serde_json::from_value(
                self.command(&mut wire, json!({ "op": "resize", "width": width, "height": height, "windowId": window_id.unwrap_or(0) }))
                    .await?,
            )
            .map_err(|_| DisplayError::unavailable())?;
            if !(1..=MAX_DISPLAY_SIZE).contains(&info.width)
                || !(1..=MAX_DISPLAY_SIZE).contains(&info.height)
            {
                self.abort();
                return Err(DisplayError::unavailable());
            }
            let mut surface = self.surface.write().unwrap();
            surface.value.width = info.width;
            surface.value.height = info.height;
            Ok(info)
        }

        pub(crate) async fn invalidate(&self) {
            let _wire = self.control.lock().await;
            let mut surface = self.surface.write().unwrap();
            surface.value.generation = uuid::Uuid::new_v4().to_string();
            surface.changed_at = std::time::Instant::now();
        }

        pub(crate) async fn finish_layout(&self) {
            let _wire = self.control.lock().await;
            let mut surface = self.surface.write().unwrap();
            surface.ready = true;
            surface.changed_at = std::time::Instant::now();
        }

        /// One frame, or `None` when the display has not changed since the
        /// previous capture. A generation rotated while the frame was in
        /// flight makes the frame stale (transient), never a helper failure,
        /// and so does retirement: a closing window is never published.
        pub(crate) async fn capture(
            &self,
            request: CaptureRequest,
        ) -> Result<Option<(Capture, Surface)>, DisplayError> {
            let captured = self.capture_current(request).await;
            if self.retired.load(Ordering::Acquire) {
                return Err(DisplayError::retired());
            }
            captured
        }

        async fn capture_current(
            &self,
            request: CaptureRequest,
        ) -> Result<Option<(Capture, Surface)>, DisplayError> {
            if self.retired.load(Ordering::Acquire) {
                return Err(DisplayError::retired());
            }
            let mut wire = self.frames.lock().await;
            let before = {
                let state = self.surface.read().unwrap();
                if !state.ready {
                    return Err(DisplayError {
                        code: "display_layout_pending".into(),
                        message: "The browser is applying its window layout.".into(),
                        operation_performed: Some(json!(false)),
                    });
                }
                state.value.clone()
            };
            let reply = self
                .command(
                    &mut wire,
                    json!({
                        "op": "capture", "cursor": request.cursor,
                        "budgetBytes": request.budget_bytes, "force": request.force,
                        "patches": request.patches,
                    }),
                )
                .await?;
            if reply["changed"] == false {
                return Ok(None);
            }
            let capture: Capture =
                serde_json::from_value(reply).map_err(|_| DisplayError::unavailable())?;
            let after = self.surface();
            if after.generation != before.generation {
                return Err(DisplayError {
                    code: "display_frame_stale".into(),
                    message: "The browser window changed while this frame was captured.".into(),
                    operation_performed: Some(json!(false)),
                });
            }
            if !capture.coherent(request, &after) {
                self.abort();
                return Err(DisplayError::unavailable());
            }
            Ok(Some((capture, after)))
        }

        pub(crate) async fn input(&self, events: &[Value]) -> Result<(), DisplayError> {
            self.request(json!({ "op": "input", "events": events }))
                .await?;
            Ok(())
        }

        pub(crate) async fn reset(&self) -> Result<(), DisplayError> {
            self.request(json!({ "op": "reset" })).await?;
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
        };

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
                    })
                    .await
            });
            capture_received.await.unwrap();
            display.input(&[]).await.unwrap();
            input_done.send(()).unwrap();
            let (captured, surface) = capture.await.unwrap().unwrap().unwrap();
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
            assert!(display.capture(CAPTURE).await.unwrap().is_none());
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
            assert!(display.capture(CAPTURE).await.unwrap().is_some());
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
            let (captured, _) = display.capture(patched).await.unwrap().unwrap();
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
pub(crate) use platform::{DisplayClient, DisplayProcess};

/// There is no private X11 backend on other platforms. The uninhabited type
/// keeps the optional browser capability honest without a fake implementation.
#[cfg(not(target_os = "linux"))]
pub(crate) enum DisplayClient {}

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
        _: u32,
        _: u32,
        _: Option<u32>,
    ) -> Result<DisplayInfo, DisplayError> {
        match *self {}
    }
    pub(crate) async fn invalidate(&self) {
        match *self {}
    }
    pub(crate) async fn finish_layout(&self) {
        match *self {}
    }
    pub(crate) async fn capture(
        &self,
        _: CaptureRequest,
    ) -> Result<Option<(Capture, Surface)>, DisplayError> {
        match *self {}
    }
    pub(crate) async fn input(&self, _: &[Value]) -> Result<(), DisplayError> {
        match *self {}
    }
    pub(crate) async fn reset(&self) -> Result<(), DisplayError> {
        match *self {}
    }
}
