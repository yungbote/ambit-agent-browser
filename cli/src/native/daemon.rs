use serde_json::Value;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::signal;
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinSet;

use super::actions::{
    auto_save_restore_state, close_all_browser_backends, close_current_browser,
    execute_command_received, maybe_autosave_restore_state, DaemonState,
};
use super::cdp::client::CdpClient;
use super::playwright::{InterruptReason, Operations};
use super::state;
use super::stream::{IdleActivity, StreamServer};
use crate::connection::{DaemonSession, INTERNAL_DAEMON_SHUTDOWN_ACTION};

/// Foreground daemons retain the invoking PID and stderr for a process supervisor.
/// Detached daemons preserve the ordinary CLI's background logging behavior.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DaemonMode {
    Detached,
    Foreground,
}

pub async fn run_daemon(session: &str, mode: DaemonMode) -> Result<(), String> {
    // Claim ownership before changing metadata, sockets, or log files. The guard
    // cleans up only this daemon's files and releases its lock after shutdown.
    let _session = DaemonSession::acquire(session, mode == DaemonMode::Foreground)?;
    let socket_dir = get_daemon_socket_dir();
    let socket_path = socket_dir.join(format!("{}.sock", session));
    let stream_path = socket_dir.join(format!("{}.stream", session));

    #[cfg(unix)]
    if mode == DaemonMode::Detached {
        use std::os::unix::io::AsRawFd;
        let log_path = if env::var("AGENT_BROWSER_DEBUG").is_ok() {
            socket_dir.join(format!("{}.log", session))
        } else {
            PathBuf::from("/dev/null")
        };
        if let Ok(file) = fs::File::create(log_path) {
            // Keep detached logging independent of the spawning CLI's pipe.
            unsafe { libc::dup2(file.as_raw_fd(), 2) };
        }
    }
    if env::var("AGENT_BROWSER_DEBUG").is_ok() {
        let _ = writeln!(
            std::io::stderr(),
            "[daemon] Started for session: {}",
            session
        );
    }

    if let Ok(days_str) = env::var("AGENT_BROWSER_STATE_EXPIRE_DAYS") {
        if let Ok(days) = days_str.parse::<u64>() {
            if days > 0 {
                let _ = state::state_clean(days);
            }
        }
    }

    let mut stream_client: Option<Arc<RwLock<Option<Arc<CdpClient>>>>> = None;
    let mut stream_server_instance: Option<Arc<StreamServer>> = None;
    let idle_activity = Arc::new(IdleActivity::new());
    let preferred_port = env::var("AGENT_BROWSER_STREAM_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    match StreamServer::start_without_client(
        preferred_port,
        session.to_string(),
        true,
        idle_activity.clone(),
    )
    .await
    {
        Ok((stream_server, client_slot)) => {
            stream_client = Some(client_slot.clone());
            if let Err(e) = fs::write(&stream_path, stream_server.port().to_string()) {
                let _ = writeln!(std::io::stderr(), "Failed to write .stream file: {}", e);
            }
            stream_server_instance = Some(Arc::new(stream_server));
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "Stream server failed to start: {}", e);
        }
    }

    // Auto-shutdown the daemon after this many ms of inactivity (no commands
    // or dashboard input received). Applies a default when
    // AGENT_BROWSER_IDLE_TIMEOUT_MS is unset; an explicit 0 disables idle
    // shutdown entirely.
    let idle_timeout = resolve_idle_timeout(env::var("AGENT_BROWSER_IDLE_TIMEOUT_MS").ok());

    let autosave_interval_ms = autosave_interval_ms_from_env();

    run_socket_server(
        &socket_path,
        session,
        stream_client,
        stream_server_instance,
        idle_activity,
        idle_timeout,
        autosave_interval_ms,
    )
    .await
}

/// Idle timeout applied when AGENT_BROWSER_IDLE_TIMEOUT_MS is unset, so an
/// integration that dies without calling `close` cannot leak the daemon and
/// its Chrome tree indefinitely (issue: leaked daemons observed running for
/// days). Socket commands and dashboard input reset the timer. Unlike an
/// explicit timeout, the default never closes a headed browser (including
/// Safari and iOS WebDriver sessions) or a user-attached browser because those
/// may be in direct human use that the daemon cannot observe. Provider-owned
/// CDP browsers remain eligible for cleanup.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 60 * 60 * 1000;

#[derive(Clone, Copy)]
struct IdleTimeout {
    ms: u64,
    /// True when the value came from DEFAULT_IDLE_TIMEOUT_MS rather than an
    /// explicit AGENT_BROWSER_IDLE_TIMEOUT_MS. Only the default exempts
    /// headed and user-attached browsers from shutdown.
    is_default: bool,
}

/// Resolve AGENT_BROWSER_IDLE_TIMEOUT_MS into an effective idle timeout:
/// unset or unparseable → the default; explicit 0 → disabled (None);
/// any other value → that many milliseconds.
fn resolve_idle_timeout(raw: Option<String>) -> Option<IdleTimeout> {
    match raw.as_deref().map(str::trim).map(str::parse::<u64>) {
        Some(Ok(0)) => None,
        Some(Ok(ms)) => Some(IdleTimeout {
            ms,
            is_default: false,
        }),
        // Unparseable values are validated (with a warning) at the flags
        // layer; falling back to the default here keeps the leak backstop
        // in place rather than silently disabling it.
        Some(Err(_)) | None => Some(IdleTimeout {
            ms: DEFAULT_IDLE_TIMEOUT_MS,
            is_default: true,
        }),
    }
}

fn remaining_idle_timeout(activity: &IdleActivity, timeout_ms: u64) -> Option<Duration> {
    Duration::from_millis(timeout_ms).checked_sub(activity.elapsed())
}

/// Minimum ms between periodic session autosaves while the browser is open.
/// Defaults to 30s; 0 disables periodic autosave (save-on-close still runs).
fn autosave_interval_ms_from_env() -> u64 {
    env::var("AGENT_BROWSER_AUTOSAVE_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30_000)
}

async fn run_socket_server(
    socket_path: &PathBuf,
    session: &str,
    stream_client: Option<Arc<RwLock<Option<Arc<CdpClient>>>>>,
    stream_server: Option<Arc<StreamServer>>,
    idle_activity: Arc<IdleActivity>,
    idle_timeout: Option<IdleTimeout>,
    autosave_interval_ms: u64,
) -> Result<(), String> {
    let shutdown = shutdown_signal()?;
    tokio::pin!(shutdown);
    let idle_timeout_ms = idle_timeout.map(|t| t.ms);

    #[cfg(unix)]
    let listener = tokio::net::UnixListener::bind(socket_path)
        .map_err(|e| format!("Failed to bind socket: {}", e))?;
    #[cfg(windows)]
    let listener = {
        use tokio::net::TcpListener;
        let preferred_port = get_port_for_session(session);
        let listener = match TcpListener::bind(format!("127.0.0.1:{}", preferred_port)).await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("Failed to bind TCP: {}", e))?,
        };
        let actual_port = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?
            .port();
        let socket_dir = socket_path.parent().unwrap_or(std::path::Path::new("."));
        fs::write(
            socket_dir.join(format!("{}.port", session)),
            actual_port.to_string(),
        )
        .map_err(|e| format!("Failed to write daemon port: {}", e))?;
        listener
    };

    let stream_file: Option<PathBuf> = if stream_server.is_some() {
        let dir = socket_path.parent().unwrap_or(std::path::Path::new("."));
        Some(dir.join(format!("{}.stream", session)))
    } else {
        None
    };
    let state: std::sync::Arc<tokio::sync::Mutex<DaemonState>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(DaemonState::new_with_stream(
            stream_client,
            stream_server,
            idle_activity.clone(),
        )));
    let playwright_operations = state.lock().await.playwright_operations.clone();

    // Notifier used by handle_connection to signal the daemon loop to exit
    // after a "close" command, instead of calling process::exit() which skips
    // destructors and can leave Chrome processes orphaned (issue #1113).
    let close_notify = Arc::new(Notify::new());

    // Every task that can hold daemon state belongs to this session. On
    // shutdown, cancellation releases those locks before browser teardown.
    let mut tasks = JoinSet::new();
    tasks.spawn(maintain_browser(state.clone(), autosave_interval_ms));

    let idle_sleep = idle_timeout_ms.map(|ms| tokio::time::sleep(Duration::from_millis(ms)));
    let mut idle_sleep_pin = idle_sleep.map(Box::pin);

    let interrupted = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        let idle_activity = idle_activity.clone();
                        let sf = stream_file.clone();
                        let cn = close_notify.clone();
                        let operations = playwright_operations.clone();
                        tasks.spawn(async move {
                            handle_connection(stream, state, idle_activity, sf, cn, operations).await;
                        });
                    }
                    Err(e) => {
                        let _ = writeln!(std::io::stderr(), "Accept error: {}", e);
                    }
                }
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    let _ = writeln!(std::io::stderr(), "Daemon task failed: {}", error);
                }
            }
            s = async {
                match idle_sleep_pin {
                    Some(ref mut s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
                state.lock().await
            }, if idle_timeout_ms.is_some() => {
                // The timer may have expired while a command held the state
                // lock. Command completion refreshes the shared activity
                // clock before releasing that lock, so re-check it here.
                if let Some(remaining) =
                    remaining_idle_timeout(&idle_activity, idle_timeout_ms.unwrap_or_default())
                {
                    idle_sleep_pin = Some(Box::pin(tokio::time::sleep(remaining)));
                    continue;
                }
                // The default timeout is a leak backstop, not a lifecycle
                // policy: never pull a headed, WebDriver, or attached browser
                // out from under a human. Re-arm and keep waiting instead.
                if idle_timeout.is_some_and(|t| t.is_default)
                    && s.blocks_default_idle_shutdown()
                {
                    idle_sleep_pin = idle_timeout_ms
                        .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                    continue;
                }
                if idle_timeout.is_some_and(|t| t.is_default) {
                    let _ = writeln!(
                        std::io::stderr(),
                        "Idle for {}m with no commands or dashboard input; saving configured restore state and shutting down (AGENT_BROWSER_IDLE_TIMEOUT_MS=0 disables)",
                        DEFAULT_IDLE_TIMEOUT_MS / 60_000
                    );
                }
                break false;
            }
            _ = idle_activity.notified(), if idle_timeout_ms.is_some() => {
                idle_sleep_pin = idle_timeout_ms
                    .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                continue;
            }
            _ = close_notify.notified() => {
                break false;
            }
            _ = &mut shutdown => break true,
        }
    };

    // Stop accepting work before cancelling existing connections. A blocked
    // navigation or maintenance tick must not hold the state lock through the
    // supervisor's termination grace period.
    drop(listener);
    let _stop_programs = playwright_operations.interrupt(InterruptReason::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(7), playwright_operations.settled()).await;
    tasks.shutdown().await;
    let mut state = state.lock().await;
    // Normal close/idle saves keep their existing behavior. A termination
    // signal grants a short best-effort save window, including if it arrives
    // after idle shutdown began. A blocked renderer must not prevent owned
    // browser cleanup before a supervisor escalates to killing the daemon.
    let save_result = {
        let save = auto_save_restore_state(&mut state);
        tokio::pin!(save);
        let grace = Duration::from_secs(1);
        if interrupted {
            tokio::time::timeout(grace, &mut save).await
        } else {
            tokio::select! {
                result = &mut save => Ok(result),
                _ = &mut shutdown => tokio::time::timeout(grace, &mut save).await,
            }
        }
    };
    match save_result {
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "State save exceeded the 1s termination grace period; closing browser. Latest changes may not be saved.");
        }
        Ok(Err(error)) => {
            let _ = writeln!(
                std::io::stderr(),
                "Failed to save browser state during shutdown: {}",
                error
            );
        }
        Ok(Ok(_)) => {}
    }
    let _ = close_all_browser_backends(&mut state).await;
    // The process can remain alive while its browser finishes closing. End
    // the visual stream explicitly before runtime teardown drops its tasks.
    if let Some(server) = state.stream_server.take() {
        server.shutdown().await;
        state.stream_client = None;
    }
    Ok(())
}

/// Periodic CDP maintenance shares command custody and is cancelled on stop.
/// A viewer's layout request also wakes it at once: the window follows the
/// dock without waiting for the next tick or command.
async fn maintain_browser(state: Arc<tokio::sync::Mutex<DaemonState>>, autosave_interval_ms: u64) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut layout = LayoutWakeup::default();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = layout.changed() => {}
        }
        let mut state = state.lock().await;
        layout.follow(state.stream_server.as_ref());
        if let Err(error) = state.expire_browser_control().await {
            let _ = writeln!(std::io::stderr(), "{}: {}", error.code, error.message);
        }
        let process_exited = state
            .browser
            .as_mut()
            .map(|manager| manager.has_process_exited())
            .unwrap_or(false);
        if process_exited {
            let _ = close_current_browser(&mut state).await;
        } else if state.browser.is_some() {
            if let Err(error) = state.drain_cdp_events_background().await {
                let _ = writeln!(
                    std::io::stderr(),
                    "Failed to apply browser network controls: {}",
                    error
                );
            } else {
                state.apply_pending_window_layout().await;
                maybe_autosave_restore_state(&mut state, autosave_interval_ms).await;
            }
        }
    }
}

/// The presentation cell of the current stream server, re-subscribed whenever
/// the server is replaced. Without a server there is nothing to wake for.
#[derive(Default)]
struct LayoutWakeup {
    server: Option<std::sync::Weak<StreamServer>>,
    changes: Option<tokio::sync::watch::Receiver<super::stream::presentation::PresentationState>>,
}

impl LayoutWakeup {
    fn follow(&mut self, server: Option<&Arc<StreamServer>>) {
        let same = match (self.server.as_ref(), server) {
            (Some(current), Some(server)) => std::ptr::eq(current.as_ptr(), Arc::as_ptr(server)),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.server = server.map(Arc::downgrade);
        self.changes = server.map(|server| server.presentation.subscribe());
    }

    async fn changed(&mut self) {
        match self.changes.as_mut() {
            Some(changes) => {
                if changes.changed().await.is_err() {
                    // The server is gone; the next tick re-subscribes.
                    self.changes = None;
                    self.server = None;
                }
            }
            None => std::future::pending().await,
        }
    }
}

async fn handle_connection<S>(
    stream: S,
    state: std::sync::Arc<tokio::sync::Mutex<DaemonState>>,
    idle_activity: Arc<IdleActivity>,
    stream_file_cleanup: Option<PathBuf>,
    close_notify: Arc<Notify>,
    playwright_operations: Operations,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    let mut queued = VecDeque::new();
    let mut partial = Vec::new();

    loop {
        line.clear();
        let read = if let Some(next) = queued.pop_front() {
            line = next;
            Ok(line.len())
        } else {
            match buf_reader.read_until(b'\n', &mut partial).await {
                Ok(count) => match String::from_utf8(std::mem::take(&mut partial)) {
                    Ok(text) => {
                        line = text;
                        Ok(count)
                    }
                    Err(_) => break,
                },
                Err(error) => Err(error),
            }
        };
        match read {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if looks_like_http(trimmed) {
                    break;
                }

                let cmd: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = serde_json::json!({
                            "success": false,
                            "error": format!("Invalid JSON: {}", e),
                        });
                        let mut resp = serde_json::to_string(&err).unwrap_or_default();
                        resp.push('\n');
                        let _ = writer.write_all(resp.as_bytes()).await;
                        continue;
                    }
                };

                idle_activity.mark();

                let received_at = std::time::Instant::now();

                let action = cmd
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let mut disconnected = false;
                let admitted = if action == "run_playwright" {
                    tokio::select! {
                        biased;
                        _ = read_until_disconnect(&mut buf_reader, &mut queued, &mut partial) => break,
                        admitted = command_state(&state, &cmd, &playwright_operations) => admitted,
                    }
                } else {
                    command_state(&state, &cmd, &playwright_operations).await
                };
                let response = match admitted {
                    Ok(mut s) => {
                        let response = if action == "run_playwright" {
                            let execution = execute_command_received(&cmd, &mut s, received_at);
                            tokio::pin!(execution);
                            tokio::select! {
                                biased;
                                _ = read_until_disconnect(&mut buf_reader, &mut queued, &mut partial) => {
                                    disconnected = true;
                                    let _stop = playwright_operations.interrupt(InterruptReason::CallerDisconnected);
                                    execution.await
                                }
                                response = &mut execution => response,
                            }
                        } else {
                            execute_command_received(&cmd, &mut s, received_at).await
                        };
                        // Refresh while command custody is still held.
                        idle_activity.mark();
                        response
                    }
                    Err(response) => response,
                };
                if disconnected {
                    break;
                }

                let mut resp = serde_json::to_string(&response).unwrap_or_default();
                resp.push('\n');
                if writer.write_all(resp.as_bytes()).await.is_err() {
                    break;
                }

                if close_completed_response(&action, &response) {
                    if let Some(ref path) = stream_file_cleanup {
                        let _ = fs::remove_file(path);
                    }
                    // Signal the daemon loop to exit gracefully instead of
                    // calling process::exit(), which skips destructors and
                    // can leave Chrome processes orphaned (issue #1113).
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    close_notify.notify_one();
                    return;
                }
            }
            Err(_) => break,
        }
    }
}

/// Preserve pipelined requests while observing EOF during a long operation.
/// Input is bounded so a peer cannot turn cancellation detection into a queue.
async fn read_until_disconnect<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    queued: &mut VecDeque<String>,
    partial: &mut Vec<u8>,
) {
    let mut bytes = queued.iter().map(String::len).sum::<usize>() + partial.len();
    loop {
        let available = match reader.fill_buf().await {
            Ok([]) | Err(_) => return,
            Ok(bytes) => bytes,
        };
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        bytes += count;
        if bytes > 2 * 1024 * 1024 {
            return;
        }
        partial.extend_from_slice(&available[..count]);
        let complete = partial.last() == Some(&b'\n');
        reader.consume(count);
        if complete {
            match String::from_utf8(std::mem::take(partial)) {
                Ok(line) => queued.push_back(line),
                Err(_) => return,
            }
        }
    }
}

fn looks_like_http(line: &str) -> bool {
    let prefixes = [
        "GET ", "POST ", "PUT ", "DELETE ", "PATCH ", "HEAD ", "OPTIONS ", "CONNECT ", "TRACE ",
    ];
    prefixes.iter().any(|p| line.starts_with(p))
}

/// A user takeover must settle inside the existing ten-second relay request.
/// Waiting two seconds for current command custody leaves five seconds for
/// already-admitted stream input to drain. A busy refusal never claims control.
async fn command_state<'a>(
    state: &'a tokio::sync::Mutex<DaemonState>,
    command: &Value,
    operations: &Operations,
) -> Result<tokio::sync::MutexGuard<'a, DaemonState>, Value> {
    if command["action"] == super::browser_control::ACTION && command["op"] == "acquire" {
        let _stop = super::browser_control::ControlRequest::parse(command)
            .ok()
            .map(|_| operations.interrupt(InterruptReason::HumanControl));
        tokio::time::timeout(Duration::from_secs(2), state.lock()).await.map_err(|_| serde_json::json!({
            "id": command["id"], "success": false, "code": "browser_control_unavailable",
            "error": "The browser is finishing its current operation. Try taking control again when it finishes.",
        }))
    } else {
        Ok(state.lock().await)
    }
}

fn close_completed_response(action: &str, response: &Value) -> bool {
    if !matches!(
        action,
        "close" | "confirm" | INTERNAL_DAEMON_SHUTDOWN_ACTION
    ) {
        return false;
    }

    fn data_closed(data: &Value) -> bool {
        data.get("closed").and_then(|v| v.as_bool()) == Some(true)
    }

    if response.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }

    let Some(data) = response.get("data") else {
        return false;
    };
    if data_closed(data) {
        return true;
    }

    data.get("result").is_some_and(|result| {
        result.get("success").and_then(|v| v.as_bool()) == Some(true)
            && result.get("data").is_some_and(data_closed)
    })
}

/// Register Unix handlers before publishing a listener, then keep the same
/// future alive across loop iterations so shutdown signals cannot be discarded.
fn shutdown_signal() -> Result<impl std::future::Future<Output = ()>, String> {
    #[cfg(unix)]
    {
        let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
            .map_err(|e| format!("Failed to install SIGINT handler: {}", e))?;
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .map_err(|e| format!("Failed to install SIGTERM handler: {}", e))?;
        let mut sighup = signal::unix::signal(signal::unix::SignalKind::hangup())
            .map_err(|e| format!("Failed to install SIGHUP handler: {}", e))?;
        Ok(async move {
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
                _ = sighup.recv() => {}
            }
        })
    }

    #[cfg(windows)]
    {
        Ok(async {
            if let Err(e) = signal::ctrl_c().await {
                let _ = writeln!(std::io::stderr(), "Failed to install Ctrl+C handler: {}", e);
            }
        })
    }
}

fn get_daemon_socket_dir() -> PathBuf {
    crate::connection::get_socket_dir()
}

#[cfg(windows)]
fn get_port_for_session(session: &str) -> u16 {
    crate::connection::get_port_for_session(session)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_resolve_idle_timeout_unset_applies_default() {
        let t = resolve_idle_timeout(None).expect("default should apply when unset");
        assert_eq!(t.ms, DEFAULT_IDLE_TIMEOUT_MS);
        assert!(t.is_default);
    }

    #[test]
    fn test_resolve_idle_timeout_explicit_zero_disables() {
        assert!(resolve_idle_timeout(Some("0".to_string())).is_none());
        assert!(resolve_idle_timeout(Some(" 0 ".to_string())).is_none());
    }

    #[test]
    fn test_resolve_idle_timeout_explicit_value_is_not_default() {
        let t = resolve_idle_timeout(Some("5000".to_string())).expect("explicit value");
        assert_eq!(t.ms, 5000);
        assert!(!t.is_default);
    }

    #[test]
    fn test_resolve_idle_timeout_unparseable_falls_back_to_default() {
        for raw in ["banana", "", "-1", "30s"] {
            let t = resolve_idle_timeout(Some(raw.to_string()))
                .unwrap_or_else(|| panic!("{:?} should fall back to default", raw));
            assert_eq!(t.ms, DEFAULT_IDLE_TIMEOUT_MS);
            assert!(t.is_default);
        }
    }

    #[test]
    fn test_default_idle_timeout_does_not_close_webdriver_sessions() {
        let mut state = DaemonState::new();
        assert!(!state.blocks_default_idle_shutdown());

        state.backend_type = crate::native::actions::BackendType::WebDriver;
        assert!(state.blocks_default_idle_shutdown());
    }

    #[tokio::test]
    async fn acquire_busy_refuses_without_late_custody() {
        let state = tokio::sync::Mutex::new(DaemonState::new());
        let held = state.lock().await;
        let command =
            serde_json::json!({ "action": super::super::browser_control::ACTION, "op": "acquire" });
        let operations = Operations::default();
        let response = match command_state(&state, &command, &operations).await {
            Ok(_) => panic!("acquisition passed an active command"),
            Err(response) => response,
        };
        assert_eq!(response["code"], "browser_control_unavailable");
        drop(held);
        assert!(command_state(&state, &command, &operations).await.is_ok());
        assert!(state
            .lock()
            .await
            .browser_control
            .lock()
            .await
            .agent_error()
            .is_none());
    }

    #[tokio::test]
    async fn disconnect_observer_preserves_partial_pipelined_input_when_canceled() {
        let (mut client, server) = tokio::io::duplex(256);
        let mut reader = BufReader::new(server);
        let mut queued = VecDeque::new();
        let mut partial = Vec::new();
        client
            .write_all(b"{\"action\":\"title\"}\n{\"action\":")
            .await
            .unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            read_until_disconnect(&mut reader, &mut queued, &mut partial)
        )
        .await
        .is_err());
        assert_eq!(queued.pop_front().unwrap(), "{\"action\":\"title\"}\n");
        assert_eq!(partial, b"{\"action\":");
        client.write_all(b"\"url\"}\n").await.unwrap();
        drop(client);
        read_until_disconnect(&mut reader, &mut queued, &mut partial).await;
        assert_eq!(queued.pop_front().unwrap(), "{\"action\":\"url\"}\n");
        assert!(partial.is_empty());
    }

    #[tokio::test]
    async fn disconnected_queued_program_never_waits_for_or_interrupts_command_custody() {
        let state = Arc::new(tokio::sync::Mutex::new(DaemonState::new()));
        let held = state.lock().await;
        let operations = held.playwright_operations.clone();
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(handle_connection(
            server,
            state.clone(),
            Arc::new(IdleActivity::new()),
            None,
            Arc::new(Notify::new()),
            operations,
        ));
        client
            .write_all(
                b"{\"action\":\"run_playwright\",\"code\":\"return 1\",\"timeoutMs\":1000}\n",
            )
            .await
            .unwrap();
        drop(client);
        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .unwrap()
            .unwrap();
        assert!(held.browser.is_none());
    }

    /// One daemon connection carrying one request, as a host helper would.
    async fn connect(
        state: Arc<tokio::sync::Mutex<DaemonState>>,
        operations: Operations,
        command: Value,
    ) -> (
        BufReader<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<()>,
    ) {
        let (mut client, server) = tokio::io::duplex(64 << 10);
        let activity = Arc::new(IdleActivity::new());
        let task = tokio::spawn(handle_connection(
            server,
            state,
            activity,
            None,
            Arc::new(Notify::new()),
            operations,
        ));
        client
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        (BufReader::new(client), task)
    }

    async fn reply(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn value(client: &CdpClient, session: &str, expression: &str) -> Value {
        client
            .send_command(
                "Runtime.evaluate",
                Some(serde_json::json!({"expression":expression,"returnByValue":true})),
                Some(session),
            )
            .await
            .unwrap()["result"]["value"]
            .clone()
    }

    /// A stop that lands while the runner is still loading Playwright and
    /// attaching proves that no program code ran: human control reports the
    /// ordinary refusal and a deadline reports rejection, never an
    /// interrupted or unknown outcome. The page shows no program effect.
    #[tokio::test]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_stop_before_program_code_reports_that_nothing_ran() {
        use crate::test_utils::EnvGuard;
        use serde_json::json;
        let real = std::env::var("AGENT_BROWSER_PLAYWRIGHT_MODULE")
            .expect("AGENT_BROWSER_PLAYWRIGHT_MODULE selects playwright-core");
        let directory = tempfile::tempdir().unwrap();
        let slow = directory.path().join("slow-playwright.mjs");
        std::fs::write(
            &slow,
            format!(
                "await new Promise(resolve => setTimeout(resolve, 3000));\nexport * from {};\n",
                serde_json::to_string(url::Url::from_file_path(&real).unwrap().as_str()).unwrap()
            ),
        )
        .unwrap();
        let env = EnvGuard::new(&["AGENT_BROWSER_PLAYWRIGHT_MODULE"]);
        env.set("AGENT_BROWSER_PLAYWRIGHT_MODULE", slow.to_str().unwrap());

        let mut initial = DaemonState::new();
        let opened = Box::pin(execute_command_received(
            &json!({"action":"navigate","url":"data:text/html,<title>Untouched</title>"}),
            &mut initial,
            std::time::Instant::now(),
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let browser = initial.browser.as_ref().unwrap();
        let client = browser.client.clone();
        let session = browser.active_session_id().unwrap().to_owned();
        let operations = initial.playwright_operations.clone();
        let state = Arc::new(tokio::sync::Mutex::new(initial));
        let program = "await page.evaluate(() => document.title = 'Program ran');";

        let (mut late, task) = connect(
            state.clone(),
            operations.clone(),
            json!({"action":"run_playwright","timeoutMs":1000,"code":program}),
        )
        .await;
        let refused = reply(&mut late).await;
        assert_eq!(refused["code"], "browser_operation_rejected", "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("did not start before its deadline"),
            "{refused}"
        );
        drop(late);
        task.await.unwrap();

        let (mut waiting, program_task) = connect(
            state.clone(),
            operations.clone(),
            json!({"action":"run_playwright","timeoutMs":30000,"code":program}),
        )
        .await;
        // The runner is spawned and still loading its client.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let controller = uuid::Uuid::new_v4().to_string();
        let expires = crate::native::stream::timestamp_ms() + 30_000;
        let (mut human, human_task) = connect(state.clone(), operations.clone(), json!({"action":super::super::browser_control::ACTION,"op":"acquire","controllerId":controller,"expiresAt":expires})).await;
        let acquired = reply(&mut human).await;
        assert_eq!(acquired["success"], true, "{acquired}");
        let stopped = reply(&mut waiting).await;
        assert_eq!(stopped["code"], "browser_controlled_by_user", "{stopped}");
        assert!(stopped.get("data").is_none_or(Value::is_null), "{stopped}");
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(
            value(&client, &session, "document.title").await,
            "Untouched"
        );
        drop(human);
        human_task.await.unwrap();
        drop(waiting);
        program_task.await.unwrap();
        let (mut release, task) = connect(state.clone(), operations.clone(), json!({"action":super::super::browser_control::ACTION,"op":"release","controllerId":controller})).await;
        assert_eq!(reply(&mut release).await["success"], true);
        drop(release);
        task.await.unwrap();
        close_current_browser(&mut *state.lock().await)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_takeover_and_caller_disconnect_settle_before_releasing_custody() {
        use serde_json::json;

        let mut initial = DaemonState::new();
        let opened = Box::pin(execute_command_received(
            &json!({"action":"navigate","url":"data:text/html,<title>Custody</title>"}),
            &mut initial,
            std::time::Instant::now(),
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let browser = initial.browser.as_ref().unwrap();
        let client = browser.client.clone();
        let session = browser.active_session_id().unwrap().to_owned();
        let target = browser.active_target_id().unwrap().to_owned();
        let operations = initial.playwright_operations.clone();
        let state = Arc::new(tokio::sync::Mutex::new(initial));
        let (mut program, program_task) = connect(state.clone(), operations.clone(), json!({"action":"run_playwright","timeoutMs":60000,"code":"await page.evaluate(() => {globalThis.holds={buttons:0,shift:false};addEventListener('pointerdown',e=>holds.buttons=e.buttons);addEventListener('pointerup',e=>holds.buttons=e.buttons);addEventListener('keydown',e=>holds.shift=e.shiftKey);addEventListener('keyup',e=>holds.shift=e.shiftKey)}); await page.mouse.move(40,40); await page.mouse.down(); await page.keyboard.down('Shift'); await page.evaluate(() => globalThis.programStarted = true); await page.waitForTimeout(60000);"})).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while value(&client, &session, "globalThis.programStarted").await != true {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let controller = uuid::Uuid::new_v4().to_string();
        let expires = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 30000;
        let (mut human, human_task) = connect(state.clone(), operations.clone(), json!({"action":super::super::browser_control::ACTION,"op":"acquire","controllerId":controller,"expiresAt":expires})).await;
        let acquired = reply(&mut human).await;
        assert_eq!(acquired["success"], true, "{acquired}");
        assert_eq!(
            value(&client, &session, "globalThis.holds").await,
            json!({"buttons":0,"shift":false})
        );
        assert_eq!(
            reply(&mut program).await["code"],
            "browser_operation_interrupted"
        );
        assert_eq!(
            state
                .lock()
                .await
                .browser
                .as_ref()
                .unwrap()
                .active_target_id()
                .unwrap(),
            target
        );
        drop(human);
        human_task.await.unwrap();
        drop(program);
        program_task.await.unwrap();
        let (mut release, task) = connect(state.clone(), operations.clone(), json!({"action":super::super::browser_control::ACTION,"op":"release","controllerId":controller})).await;
        assert_eq!(reply(&mut release).await["success"], true);
        drop(release);
        task.await.unwrap();

        let code = "const {spawn} = await import('node:child_process'); const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {stdio:'ignore'}); await page.evaluate(pid => {globalThis.childPid=pid;globalThis.ticks=0}, child.pid); for (;;) { await page.evaluate(() => globalThis.ticks++); await page.waitForTimeout(20); }";
        let (mut observation, task) = connect(
            state.clone(),
            operations.clone(),
            json!({"action":"snapshot"}),
        )
        .await;
        assert_eq!(reply(&mut observation).await["success"], true);
        drop(observation);
        task.await.unwrap();
        let (program, task) = connect(
            state.clone(),
            operations.clone(),
            json!({"action":"run_playwright","timeoutMs":60000,"code":code}),
        )
        .await;
        let child = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(pid) = value(&client, &session, "globalThis.childPid")
                    .await
                    .as_u64()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        drop(program);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let ticks = value(&client, &session, "globalThis.ticks").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(value(&client, &session, "globalThis.ticks").await, ticks);
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string(format!("/proc/{child}/stat"));
            assert!(
                status.is_err()
                    || status
                        .unwrap()
                        .rsplit_once(')')
                        .unwrap()
                        .1
                        .split_whitespace()
                        .next()
                        == Some("Z"),
                "The program's child remained executable after disconnect"
            );
        }
        assert_eq!(
            state
                .lock()
                .await
                .browser
                .as_ref()
                .unwrap()
                .active_target_id()
                .unwrap(),
            target
        );

        // Takeover during a mutating loop: the loop is stopped and settled
        // before control is granted, and no program input lands afterwards.
        let (mut observation, task) = connect(
            state.clone(),
            operations.clone(),
            json!({"action":"snapshot"}),
        )
        .await;
        assert_eq!(reply(&mut observation).await["success"], true);
        drop(observation);
        task.await.unwrap();
        let (mut program, program_task) = connect(state.clone(), operations.clone(), json!({"action":"run_playwright","timeoutMs":60000,"code":"await page.evaluate(() => {globalThis.ticks=0;globalThis.typed='';addEventListener('keydown',e=>typed+=e.key)}); for (;;) { await page.keyboard.press('x'); await page.evaluate(() => globalThis.ticks++); await page.waitForTimeout(20); }"})).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while value(&client, &session, "globalThis.ticks").await.as_u64() < Some(3) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let controller = uuid::Uuid::new_v4().to_string();
        let (mut human, human_task) = connect(state.clone(), operations.clone(), json!({"action":super::super::browser_control::ACTION,"op":"acquire","controllerId":controller,"expiresAt":expires})).await;
        let acquired = reply(&mut human).await;
        assert_eq!(acquired["success"], true, "{acquired}");
        let interrupted = reply(&mut program).await;
        assert_eq!(
            interrupted["code"], "browser_operation_interrupted",
            "{interrupted}"
        );
        assert_eq!(interrupted["data"]["executionStopped"], true);
        assert_eq!(interrupted["data"]["effectsMayHaveOccurred"], true);
        let settled = value(&client, &session, "({ticks, typed})").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(value(&client, &session, "({ticks, typed})").await, settled);
        assert!(
            settled["typed"].as_str().unwrap().contains('x'),
            "{settled}"
        );
        drop(human);
        human_task.await.unwrap();
        drop(program);
        program_task.await.unwrap();
        let (mut release, task) = connect(state.clone(), operations.clone(), json!({"action":super::super::browser_control::ACTION,"op":"release","controllerId":controller})).await;
        assert_eq!(reply(&mut release).await["success"], true);
        drop(release);
        task.await.unwrap();
        assert_eq!(
            state
                .lock()
                .await
                .browser
                .as_ref()
                .unwrap()
                .active_target_id()
                .unwrap(),
            target
        );
        close_current_browser(&mut *state.lock().await)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_idle_activity_receives_dashboard_activity() {
        let activity = Arc::new(IdleActivity::new());
        activity.mark();

        tokio::time::timeout(Duration::from_millis(100), activity.notified())
            .await
            .expect("dashboard input notification should wake the idle loop");
    }

    #[tokio::test]
    async fn test_command_completion_rearms_expired_idle_timeout() {
        let activity = IdleActivity::new();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            remaining_idle_timeout(&activity, 1).is_none(),
            "the original idle deadline should have expired"
        );

        // A command that held the daemon state lock past the deadline marks
        // completion before releasing the lock. The timeout path must then
        // wait for a new full idle period instead of closing immediately.
        activity.mark();
        assert!(remaining_idle_timeout(&activity, 100).is_some());
    }

    #[test]
    fn test_daemon_socket_dir_matches_client_namespace() {
        let guard = crate::test_utils::EnvGuard::new(&[
            "AGENT_BROWSER_SOCKET_DIR",
            "XDG_RUNTIME_DIR",
            "AGENT_BROWSER_NAMESPACE",
        ]);
        let dir = tempfile::tempdir().unwrap();
        guard.set("AGENT_BROWSER_SOCKET_DIR", dir.path().to_str().unwrap());
        guard.remove("XDG_RUNTIME_DIR");
        guard.set("AGENT_BROWSER_NAMESPACE", "Worktree: One");

        let socket_dir = get_daemon_socket_dir();

        assert_eq!(socket_dir, crate::connection::get_socket_dir());
        assert!(socket_dir.ends_with(
            std::path::PathBuf::from("namespaces")
                .join("worktree-one")
                .join("run")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn test_port_matches_client_algorithm() {
        let guard = crate::test_utils::EnvGuard::new(&["AGENT_BROWSER_NAMESPACE"]);
        guard.remove("AGENT_BROWSER_NAMESPACE");

        assert_eq!(get_port_for_session("default"), 50838);
        assert_eq!(get_port_for_session("my-session"), 63105);
        assert_eq!(get_port_for_session("work"), 51184);
        assert_eq!(get_port_for_session(""), 49152);
    }

    #[test]
    fn test_close_completed_response_requires_actual_close_result() {
        let confirmation_response = serde_json::json!({
            "success": true,
            "data": {
                "confirmation_required": true,
                "confirmation_id": "close-1",
                "action": "close"
            }
        });

        assert!(!close_completed_response("close", &confirmation_response));
    }

    #[test]
    fn test_close_completed_response_accepts_direct_and_confirmed_close() {
        let direct = serde_json::json!({
            "success": true,
            "data": { "closed": true }
        });
        let confirmed = serde_json::json!({
            "success": true,
            "data": {
                "confirmed": true,
                "action": "close",
                "result": {
                    "success": true,
                    "data": { "closed": true }
                }
            }
        });

        assert!(close_completed_response("close", &direct));
        assert!(close_completed_response(
            crate::connection::INTERNAL_DAEMON_SHUTDOWN_ACTION,
            &direct
        ));
        assert!(close_completed_response("confirm", &confirmed));
    }

    /// Guard against re-introducing `waitpid(-1)` in daemon code.
    ///
    /// Issue #1035: a SIGCHLD handler that called `waitpid(-1, WNOHANG)` was
    /// added in v0.22.3 to reap zombie Chrome processes. This races with
    /// Rust's `Child::try_wait()` / `Child::wait()` because `waitpid(-1)`
    /// reaps *any* child, stealing the exit status before Rust can collect
    /// it. The result is ECHILD errors in `BrowserManager::has_process_exited()`
    /// and `ChromeProcess::kill()`, which can leave the daemon in a broken
    /// state or cause hangs on certain Linux configurations.
    ///
    /// The fix uses the existing 500ms drain interval to call
    /// `has_process_exited()` (which delegates to `Child::try_wait()`)
    /// for targeted, race-free zombie detection.
    #[test]
    fn test_no_waitpid_minus_one_in_daemon() {
        let source = include_str!("daemon.rs");
        // Only check production code (everything before `#[cfg(test)]`)
        let production_code = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !production_code.contains("waitpid(-1"),
            "daemon.rs production code must not call waitpid(-1, ...). \
             Use Child::try_wait() via has_process_exited() instead. \
             See issue #1035."
        );
    }

    /// Verify that `Child::try_wait()` correctly detects a crashed child
    /// without needing a global SIGCHLD handler or `waitpid(-1)`.
    /// This is what `has_process_exited()` uses in the fixed code.
    #[cfg(unix)]
    #[test]
    fn test_child_try_wait_detects_exit_without_sigchld_handler() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 42"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn child");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child did not exit before the deadline");
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(e) => panic!("try_wait() should succeed without waitpid(-1): {}", e),
            }
        };

        assert_eq!(status.code(), Some(42));
    }

    /// Regression test for #1101: idle timeout must fire even while the
    /// drain interval ticks every 500 ms.  The bug was that `sleep_future`
    /// was created **inside** the loop, so each drain tick dropped the
    /// in-progress sleep and replaced it with a fresh one – the timer
    /// could never reach its deadline.
    #[tokio::test]
    async fn test_idle_timeout_fires_despite_drain_interval() {
        let idle_timeout_ms: u64 = 1000;
        let mut drain_interval = tokio::time::interval(Duration::from_millis(500));
        drain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let activity = IdleActivity::new();

        let start = tokio::time::Instant::now();

        let exited = tokio::time::timeout(Duration::from_secs(5), async {
            let mut idle_sleep_pin = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                idle_timeout_ms,
            ))));

            loop {
                tokio::select! {
                    _ = drain_interval.tick() => {}
                    _ = async {
                        match idle_sleep_pin {
                            Some(ref mut s) => s.as_mut().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        break;
                    }
                    _ = activity.notified() => {
                        idle_sleep_pin = Some(Box::pin(
                            tokio::time::sleep(Duration::from_millis(idle_timeout_ms)),
                        ));
                        continue;
                    }
                }
            }
        })
        .await;

        let elapsed = start.elapsed();

        assert!(
            exited.is_ok(),
            "idle timeout never fired – loop ran for >5 s (bug #1101)"
        );
        assert!(
            elapsed < Duration::from_millis(idle_timeout_ms + 500),
            "idle timeout took too long: {:?} (expected ~{} ms)",
            elapsed,
            idle_timeout_ms,
        );
    }

    /// Verify that `ChromeProcess::has_exited()` (which uses `Child::try_wait()`)
    /// correctly detects a killed child, the same way the drain interval does
    /// in the fixed daemon code. This ensures crash detection works without
    /// a SIGCHLD handler.
    #[cfg(unix)]
    #[test]
    fn test_has_exited_detects_killed_process() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn child");

        // Process should be running
        match child.try_wait() {
            Ok(None) => {} // expected
            other => panic!("expected Ok(None) for running process, got {:?}", other),
        }

        // Kill it (simulates Chrome crash)
        child.kill().expect("failed to kill child");
        std::thread::sleep(std::time::Duration::from_millis(100));

        // try_wait should detect the exit
        match child.try_wait() {
            Ok(Some(_)) => {} // expected: detected the crash
            other => panic!(
                "expected Ok(Some(_)) after kill, got {:?}. \
                 Crash detection via try_wait() must work for the drain \
                 interval fix (issue #1035) to function correctly.",
                other
            ),
        }
    }
}
