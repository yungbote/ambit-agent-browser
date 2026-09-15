//! The private X11 display of one locally owned Chrome process.
//!
//! The helper implements platform operations. Browser custody, command order,
//! frame pacing and observation identity remain in the native driver.

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capture {
    pub width: u32,
    pub height: u32,
    pub encoding: String,
    pub data: String,
    pub cursor_included: bool,
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

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    pub(crate) struct DisplayClient {
        identity: String,
        wire: tokio::sync::Mutex<BufReader<tokio::net::UnixStream>>,
        abort_socket: UnixStream,
        failed: AtomicBool,
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

    impl DisplayClient {
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

        fn abort(&self) {
            self.failed.store(true, Ordering::Release);
            let _ = self.abort_socket.shutdown(std::net::Shutdown::Both);
        }

        async fn command(
            &self,
            wire: &mut BufReader<tokio::net::UnixStream>,
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
            let mut wire = self.wire.lock().await;
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
            let mut wire = self.wire.lock().await;
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
            let _wire = self.wire.lock().await;
            let mut surface = self.surface.write().unwrap();
            surface.value.generation = uuid::Uuid::new_v4().to_string();
            surface.changed_at = std::time::Instant::now();
        }

        pub(crate) async fn finish_layout(&self) {
            let _wire = self.wire.lock().await;
            let mut surface = self.surface.write().unwrap();
            surface.ready = true;
            surface.changed_at = std::time::Instant::now();
        }

        pub(crate) async fn capture(&self) -> Result<(Capture, Surface), DisplayError> {
            let mut wire = self.wire.lock().await;
            if !self.surface.read().unwrap().ready {
                return Err(DisplayError {
                    code: "display_layout_pending".into(),
                    message: "The browser is applying its window layout.".into(),
                    operation_performed: Some(json!(false)),
                });
            }
            let capture: Capture =
                serde_json::from_value(self.command(&mut wire, json!({ "op": "capture" })).await?)
                    .map_err(|_| DisplayError::unavailable())?;
            let surface = self.surface();
            if capture.width != surface.width
                || capture.height != surface.height
                || capture.encoding != "jpeg"
                || !capture.cursor_included
            {
                self.abort();
                return Err(DisplayError::unavailable());
            }
            Ok((capture, surface))
        }

        pub(crate) async fn input(&self, event: &Value) -> Result<(), DisplayError> {
            self.request(json!({ "op": "input", "events": [event] }))
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
            let (client, helper) = UnixStream::pair().map_err(|e| e.to_string())?;
            let abort_socket = client.try_clone().map_err(|e| e.to_string())?;
            client.set_nonblocking(true).map_err(|e| e.to_string())?;
            let wire = tokio::net::UnixStream::from_std(client).map_err(|e| e.to_string())?;
            let helper_out: OwnedFd = helper.try_clone().map_err(|e| e.to_string())?.into();
            let helper_in: OwnedFd = helper.into();
            let child = Command::new(executable)
                .args(["--chrome-pid", &chrome_pid.to_string()])
                .env("DISPLAY", display)
                .env("XAUTHORITY", authority)
                .stdin(Stdio::from(helper_in))
                .stdout(Stdio::from(helper_out))
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Browser display helper could not start: {e}"))?;
            Ok(Self {
                child,
                client: Arc::new(DisplayClient {
                    identity: uuid::Uuid::new_v4().to_string(),
                    wire: tokio::sync::Mutex::new(BufReader::new(wire)),
                    abort_socket,
                    failed: AtomicBool::new(false),
                    next_id: AtomicU64::new(1),
                    surface: RwLock::new(SurfaceState {
                        value: Surface::new(MAX_DISPLAY_SIZE, MAX_DISPLAY_SIZE),
                        changed_at: std::time::Instant::now(),
                        ready: false,
                    }),
                }),
            })
        }
    }

    impl Drop for DisplayProcess {
        fn drop(&mut self) {
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
    pub(crate) async fn capture(&self) -> Result<(Capture, Surface), DisplayError> {
        match *self {}
    }
    pub(crate) async fn input(&self, _: &Value) -> Result<(), DisplayError> {
        match *self {}
    }
    pub(crate) async fn reset(&self) -> Result<(), DisplayError> {
        match *self {}
    }
}
