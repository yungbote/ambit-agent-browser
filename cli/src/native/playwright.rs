//! A bounded Node operation against the browser already owned by this daemon.
//! The operation slot is cancellation, not a second control lease. Command
//! custody remains with the daemon and human input remains with BrowserControl.

mod transport;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Notify};

use super::actions::DaemonState;
use super::cdp::client::CdpClient;

const RUNNER: &str = include_str!("../../runtime/playwright-runner.mjs");
const MAX_CODE: usize = 1 << 20;
const MAX_RESULT: usize = 2 << 20;
const MAX_DIAGNOSTICS: usize = 64 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterruptReason {
    HumanControl,
    CallerDisconnected,
    Shutdown,
}

impl InterruptReason {
    fn message(self) -> &'static str {
        match self {
            Self::HumanControl => "Human takeover interrupted the Playwright program.",
            Self::CallerDisconnected => "The Playwright caller disconnected.",
            Self::Shutdown => "The browser owner stopped the Playwright program.",
        }
    }
}

#[derive(Default)]
struct OperationState {
    active: Option<watch::Sender<Option<InterruptReason>>>,
    interruptions: usize,
}

#[derive(Clone, Default)]
pub(crate) struct Operations {
    state: Arc<Mutex<OperationState>>,
    finished: Arc<Notify>,
}

pub(crate) struct Interruption(Operations);

impl Drop for Interruption {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().interruptions -= 1;
    }
}

struct Operation {
    owner: Operations,
    canceled: watch::Receiver<Option<InterruptReason>>,
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.owner.state.lock().unwrap().active = None;
        self.owner.finished.notify_waiters();
    }
}

impl Operations {
    /// Called before waiting for command custody. A queued program cannot
    /// start between the cancellation request and control acquisition.
    pub(crate) fn interrupt(&self, reason: InterruptReason) -> Interruption {
        let mut state = self.state.lock().unwrap();
        state.interruptions += 1;
        if let Some(active) = &state.active {
            active.send_replace(Some(reason));
        }
        Interruption(self.clone())
    }

    fn begin(&self) -> Result<Operation, String> {
        let mut state = self.state.lock().unwrap();
        if state.interruptions > 0 || state.active.is_some() {
            return Err("browser_operation_rejected: Browser command custody is being transferred; the program was not started.".into());
        }
        let (sender, canceled) = watch::channel(None);
        state.active = Some(sender);
        Ok(Operation {
            owner: self.clone(),
            canceled,
        })
    }

    pub(crate) async fn settled(&self) {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().unwrap().active.is_none() {
                return;
            }
            notified.await;
        }
    }
}

/// Kill the process group before reaping its leader. The leader PID therefore
/// cannot be recycled into an unrelated process group during cleanup.
struct RunnerProcess {
    child: Child,
    pid: u32,
    settled: bool,
}

impl RunnerProcess {
    fn kill_group(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
        let _ = self.child.start_kill();
    }

    async fn settle(&mut self) -> Result<(), String> {
        self.kill_group();
        self.child
            .wait()
            .await
            .map_err(|_| "The Playwright process could not be reaped.".to_string())?;
        self.settled = true;
        Ok(())
    }
}

impl Drop for RunnerProcess {
    fn drop(&mut self) {
        if !self.settled {
            self.kill_group();
        }
    }
}

fn unknown(message: impl AsRef<str>) -> String {
    format!("browser_operation_outcome_unknown: {} Earlier effects may have executed. Inspect the browser before continuing; do not replay the program.", message.as_ref())
}

async fn result_bytes(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    BufReader::new(reader.take((MAX_RESULT + 1) as u64))
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| unknown("The program result could not be read."))?;
    if bytes.len() > MAX_RESULT {
        return Err(unknown("The program result exceeded 2 MiB."));
    }
    Ok(bytes)
}

async fn diagnostics(mut reader: impl AsyncRead + Unpin) -> (String, bool) {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut bytes = [0; 8192];
    loop {
        match reader.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let keep = count.min(MAX_DIAGNOSTICS.saturating_sub(retained.len()));
                retained.extend_from_slice(&bytes[..keep]);
                truncated |= keep < count;
            }
        }
    }
    (String::from_utf8_lossy(&retained).into_owned(), truncated)
}

/// The enclosing command retains exclusive native custody until the temporary
/// connection, every input operation and the Node group have settled.
pub(crate) async fn run(command: &Value, state: &mut DaemonState) -> Result<Value, String> {
    #[cfg(not(unix))]
    return Err(
        "browser_operation_rejected: Supervised Playwright execution requires a Unix workspace."
            .into(),
    );

    #[cfg(unix)]
    {
        let code = command["code"]
            .as_str()
            .filter(|code| !code.trim().is_empty() && code.len() <= MAX_CODE)
            .ok_or(
                "browser_operation_rejected: Playwright code must be nonempty and at most 1 MiB.",
            )?;
        let timeout = command["timeoutMs"]
            .as_u64()
            .filter(|ms| (1..=120_000).contains(ms))
            .ok_or("browser_operation_rejected: timeoutMs must be between 1 and 120000.")?;
        let browser = state
            .browser
            .as_ref()
            .ok_or("browser_operation_rejected: Open the browser before running Playwright.")?;
        let target = match command.get("targetId") {
            Some(value) => value
                .as_str()
                .filter(|target| !target.is_empty())
                .ok_or("browser_operation_rejected: targetId must identify an existing tab.")?,
            None => browser.active_target_id()?,
        }
        .to_owned();
        let endpoint = browser.get_cdp_url().to_owned();
        let control = state.browser_control.clone();
        let mut operation = state.playwright_operations.begin()?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
        let client = tokio::select! {
            biased;
            _ = operation.canceled.changed() => return Err("browser_operation_rejected: The Playwright operation was canceled before attachment.".into()),
            result = tokio::time::timeout_at(deadline, CdpClient::connect(&endpoint)) =>
                Arc::new(result.map_err(|_| "browser_operation_rejected: Browser attachment timed out.")?
                    .map_err(|_| "browser_operation_rejected: The existing browser could not be attached.")?),
        };
        let mut tunnel = transport::Tunnel::start(client, control.clone()).await?;
        let request = json!({ "endpoint": tunnel.endpoint(), "targetId": target, "code": code, "timeoutMs": timeout });
        let mut node = Command::new(
            std::env::var("AGENT_BROWSER_NODE_PATH").unwrap_or_else(|_| "node".into()),
        );
        if let Ok(path) = std::env::var("AGENT_BROWSER_PLAYWRIGHT_RUNNER") {
            node.arg(path);
        } else {
            node.args(["--input-type=module", "--eval", RUNNER]);
        }
        node.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let child = match node.spawn() {
            Ok(child) => child,
            Err(_) => {
                tunnel.finish().await?;
                return Err("browser_operation_rejected: The installed Node Playwright runner is unavailable.".into());
            }
        };
        let mut process = RunnerProcess {
            pid: child.id().unwrap(),
            child,
            settled: false,
        };
        let mut stdin = process.child.stdin.take().unwrap();
        let stdout = process.child.stdout.take().unwrap();
        let stderr = process.child.stderr.take().unwrap();
        let mut logs = tokio::spawn(diagnostics(stderr));
        let execution = async {
            stdin
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .map_err(|_| unknown("The runner did not accept its invocation."))?;
            drop(stdin);
            result_bytes(stdout).await
        };
        let result = tokio::select! {
            biased;
            _ = operation.canceled.changed() => Err(unknown(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).message())),
            _ = tokio::time::sleep_until(deadline) => Err(unknown("The program reached its deadline.")),
            result = execution => result,
        };
        // Stop accepting protocol input first. A native event already admitted
        // is allowed to finish before reset; cancellation never poisons the
        // display helper by dropping its in-flight input request.
        tunnel.stop();
        let process_cleanup = process.settle().await;
        let tunnel_cleanup = tunnel.finish().await;
        let input_cleanup = control
            .lock()
            .await
            .finish_agent_program(&tunnel.client)
            .await;
        let (diagnostics, diagnostics_truncated) =
            match tokio::time::timeout(Duration::from_secs(1), &mut logs).await {
                Ok(Ok(logs)) => logs,
                _ => {
                    logs.abort();
                    (String::new(), true)
                }
            };
        process_cleanup.map_err(unknown)?;
        tunnel_cleanup.map_err(unknown)?;
        input_cleanup.map_err(unknown)?;
        state.ref_map.clear();
        state.active_frame_id = None;
        if result.is_err() {
            control.lock().await.cancel_native_input().await?;
        }
        let bytes = result?;
        let result: Value = serde_json::from_slice(&bytes)
            .map_err(|_| unknown("The runner did not produce a complete JSON result."))?;
        if result["success"] != true {
            let message = result["error"]
                .as_str()
                .unwrap_or("The Playwright program failed.");
            return Err(if result["started"] == false {
                format!("browser_operation_rejected: {message}")
            } else {
                unknown(message)
            });
        }
        Ok(
            json!({ "result": result["result"], "diagnostics": diagnostics, "diagnosticsTruncated": diagnostics_truncated, "targetId": target }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn takeover_cancels_current_and_excludes_queued_programs_until_admitted() {
        let operations = Operations::default();
        let mut current = operations.begin().unwrap();
        let takeover = operations.interrupt(InterruptReason::HumanControl);
        current.canceled.changed().await.unwrap();
        assert_eq!(
            *current.canceled.borrow(),
            Some(InterruptReason::HumanControl)
        );
        drop(current);
        operations.settled().await;
        assert!(operations.begin().is_err());
        drop(takeover);
        assert!(operations.begin().is_ok());
    }

    #[tokio::test]
    async fn results_and_diagnostics_have_independent_byte_bounds() {
        assert!(result_bytes(&vec![b'x'; MAX_RESULT + 1][..]).await.is_err());
        let (logs, truncated) = diagnostics(&vec![b'x'; MAX_DIAGNOSTICS + 1][..]).await;
        assert_eq!(logs.len(), MAX_DIAGNOSTICS);
        assert!(truncated);
    }

    #[tokio::test]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_uses_existing_target_and_retains_browser_after_timeout() {
        use crate::native::actions::execute_command;
        let mut state = DaemonState::new();
        let result = Box::pin(execute_command(&json!({"action":"navigate","url":"data:text/html,<title>Shared</title><input id=field><button onclick=\"document.title='Clicked'\">Go</button>"}), &mut state)).await;
        assert_eq!(result["success"], true, "{result}");
        let target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        let result = Box::pin(execute_command(&json!({"action":"run_playwright","code":"await page.locator('#field').fill('retained'); await page.getByRole('button').click(); console.log('diagnostic'); return {title: await page.title(), value: await page.locator('#field').inputValue()};","timeoutMs":15000}), &mut state)).await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(
            result["data"]["result"],
            json!({"title":"Clicked","value":"retained"})
        );
        assert_eq!(result["data"]["diagnostics"], "diagnostic\n");
        assert_eq!(
            state.browser.as_ref().unwrap().active_target_id().unwrap(),
            target
        );
        let result = Box::pin(execute_command(&json!({"action":"run_playwright","code":"await page.waitForTimeout(60000);","timeoutMs":1500}), &mut state)).await;
        assert_eq!(
            result["code"], "browser_operation_outcome_unknown",
            "{result}"
        );
        let result = Box::pin(execute_command(
            &json!({"action":"evaluate","script":"document.querySelector('#field').value"}),
            &mut state,
        ))
        .await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["data"]["result"], "retained");
        assert_eq!(
            state.browser.as_ref().unwrap().active_target_id().unwrap(),
            target
        );
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }
}
