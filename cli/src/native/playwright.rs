//! A bounded Node operation against the browser already owned by this daemon.
//! The operation slot is cancellation, not a second control lease. Command
//! custody remains with the daemon and human input remains with BrowserControl.

mod transport;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Notify};

use super::actions::{CommandError, DaemonState};
use super::cdp::client::{replace_lone_surrogate_escapes, CdpClient};

const RUNNER: &str = include_str!("../../runtime/playwright-runner.mjs");
const MAX_CODE: usize = 1 << 20;
const MAX_RESULT: usize = 2 << 20;
const MAX_DIAGNOSTICS: usize = 64 << 10;
pub(crate) const ENVIRONMENT_FIELD: &str = "ambitProgram";
/// A program that failed before it issued any Playwright call: nothing was
/// done in the browser, so it is refused like any command turned away before
/// acting, and the caller fixes the program and runs it again.
pub(crate) const PROGRAM_ERROR: &str = "browser_program_error";

/// Host-selected paths for this Action. Tokens remain in the private relay
/// file and the existing SDK factory owns its schema and authorization.
#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProgramEnvironment {
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_judgement_config_path: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_judgement_client_module_path: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_network_bootstrap_path: Option<std::path::PathBuf>,
}

impl ProgramEnvironment {
    fn read(command: &Value) -> Result<Self, String> {
        let value = command
            .get(ENVIRONMENT_FIELD)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let environment: Self = serde_json::from_value(value)
            .map_err(|_| "browser_operation_rejected: The host program environment is invalid.")?;
        if environment.semantic_judgement_config_path.is_some()
            != environment.semantic_judgement_client_module_path.is_some()
            || [
                &environment.semantic_judgement_config_path,
                &environment.semantic_judgement_client_module_path,
                &environment.node_network_bootstrap_path,
            ]
            .into_iter()
            .flatten()
            .any(|path| !path.is_absolute())
        {
            return Err("browser_operation_rejected: Host program paths must be absolute and the semantic client must have its paired configuration.".into());
        }
        Ok(environment)
    }
}

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

    fn before_start(self) -> String {
        if self == Self::HumanControl {
            "browser_controlled_by_user: User control prevented this Playwright program from starting.".into()
        } else {
            format!(
                "browser_operation_rejected: {} The program was not started.",
                self.message()
            )
        }
    }

    fn stopped(self) -> CommandError {
        if self == Self::HumanControl {
            CommandError::with_data(
                "browser_operation_interrupted: The Playwright program stopped for user control. Earlier effects may have executed; inspect a fresh observation after handback and do not replay the program.",
                json!({"interruptedBy":"human","executionStopped":true,"effectsMayHaveOccurred":true}),
            )
        } else {
            unknown(self.message()).into()
        }
    }
}

#[derive(Default)]
struct OperationState {
    active: Option<watch::Sender<Option<InterruptReason>>>,
    /// Pending custody transfers, one entry per outstanding interruption.
    interruptions: Vec<InterruptReason>,
}

#[derive(Clone, Default)]
pub(crate) struct Operations {
    state: Arc<Mutex<OperationState>>,
    finished: Arc<Notify>,
}

pub(crate) struct Interruption(Operations, InterruptReason);

impl Drop for Interruption {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if let Some(index) = state
            .interruptions
            .iter()
            .position(|reason| *reason == self.1)
        {
            state.interruptions.swap_remove(index);
        }
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
        state.interruptions.push(reason);
        if let Some(active) = &state.active {
            active.send_replace(Some(reason));
        }
        Interruption(self.clone(), reason)
    }

    fn begin(&self) -> Result<Operation, String> {
        let mut state = self.state.lock().unwrap();
        // A queued program refused for a human takeover reports that
        // takeover, as a program stopped before its start would.
        let pending = state
            .interruptions
            .iter()
            .copied()
            .find(|reason| *reason == InterruptReason::HumanControl)
            .or_else(|| state.interruptions.first().copied());
        if let Some(reason) = pending {
            return Err(reason.before_start());
        }
        if state.active.is_some() {
            return Err("browser_operation_rejected: Another Playwright program holds the browser; the program was not started.".into());
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

/// Written by the runner on its private channel immediately before program
/// code runs. The runner ships with this daemon from one source revision.
const START_RECORD: &[u8] = b"{\"started\":true}\n";

/// The runner's private channel: an optional start record, then one result
/// record, each newline-terminated. Bytes read stay here across a canceled
/// read, so a stop can still tell whether program code was invoked.
struct RunnerChannel<R> {
    reader: R,
    received: Vec<u8>,
}

impl<R: AsyncRead + Unpin> RunnerChannel<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            received: Vec::new(),
        }
    }

    /// The next record including its newline, or the remaining bytes at end
    /// of stream. A record is at most 2 MiB.
    async fn record(&mut self) -> Result<Vec<u8>, String> {
        loop {
            if let Some(end) = self.received.iter().position(|byte| *byte == b'\n') {
                if end >= MAX_RESULT {
                    return Err("The program result exceeded 2 MiB.".into());
                }
                return Ok(self.received.drain(..=end).collect());
            }
            if self.received.len() >= MAX_RESULT {
                return Err("The program result exceeded 2 MiB.".into());
            }
            // Cancel-safe: a canceled read consumed nothing. Reading into the
            // retained buffer keeps this future, part of every command's,
            // small.
            self.received.reserve(8192);
            let count = self
                .reader
                .read_buf(&mut self.received)
                .await
                .map_err(|_| "The program result could not be read.".to_string())?;
            if count == 0 {
                return Ok(std::mem::take(&mut self.received));
            }
        }
    }
}

#[cfg(unix)]
impl RunnerChannel<tokio::net::UnixStream> {
    /// After the runner group is stopped, whether it had delivered the start
    /// record. Reads only what the kernel already holds, never waiting: a
    /// descendant that left the group may still hold the channel open. The
    /// reactor's cached readiness is not consulted, so delivered bytes it has
    /// not observed yet still count.
    fn started_before_stop(&mut self) -> bool {
        use std::os::fd::AsRawFd;
        let mut chunk = [0u8; 64];
        while self.received.len() < START_RECORD.len() {
            let count = unsafe {
                libc::recv(
                    self.reader.as_raw_fd(),
                    chunk.as_mut_ptr().cast(),
                    chunk.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if count <= 0 {
                break;
            }
            self.received.extend_from_slice(&chunk[..count as usize]);
        }
        self.received.starts_with(START_RECORD)
    }
}

/// How the supervised program's execution phase ended.
enum Ended {
    Record(Vec<u8>),
    Interrupted(InterruptReason),
    Deadline,
    Failed(String),
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
pub(crate) async fn run(command: &Value, state: &mut DaemonState) -> Result<Value, CommandError> {
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
        let environment = ProgramEnvironment::read(command)?;
        let mut operation = state.playwright_operations.begin()?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
        let browser = state
            .browser
            .as_mut()
            .ok_or("browser_operation_rejected: Open the browser before running Playwright.")?;
        let target = match command.get("targetId") {
            Some(value) => value
                .as_str()
                .filter(|target| !target.is_empty())
                .ok_or("browser_operation_rejected: targetId must identify an existing tab.")?,
            None => browser.active_target_id()?,
        }
        .to_owned();
        // The runner learns how many explicit isolated contexts exist. A
        // client without shared-context adoption would fold them into the
        // default profile and misreport cookies, so it refuses before start.
        let (download_context, isolated_contexts) = tokio::select! {
            biased;
            _ = operation.canceled.changed() => return Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).before_start().into()),
            result = tokio::time::timeout_at(deadline, async {
                let isolated = browser.isolated_context_ids().await?.len();
                let context = browser.download_context_for_target(&target).await?;
                browser.configure_downloads(context.as_deref()).await?;
                Ok::<_, String>((context, isolated))
            }) => result.map_err(|_| "browser_operation_rejected: Browser setup reached its deadline.")?
                .map_err(|error| format!("browser_operation_rejected: {error}"))?,
        };
        let endpoint = browser.get_cdp_url().to_owned();
        let artifacts = browser.downloads_path().map(std::path::Path::to_path_buf);
        let owner = browser.client.clone();
        let control = state.browser_control.clone();
        let client = tokio::select! {
            biased;
            _ = operation.canceled.changed() => return Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).before_start().into()),
            result = tokio::time::timeout_at(deadline, CdpClient::connect(&endpoint)) =>
                Arc::new(result.map_err(|_| "browser_operation_rejected: Browser attachment timed out.")?
                    .map_err(|_| "browser_operation_rejected: The existing browser could not be attached.")?),
        };
        // Viewers follow the owner's page sessions; program input appears
        // there exactly as native input does.
        client.publish_activity_as(&browser.client);
        let observed = tokio::select! {
            biased;
            _ = operation.canceled.changed() => Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).before_start()),
            result = tokio::time::timeout_at(deadline, browser.observe_owned_downloads(&client, download_context.as_deref())) => result.map_err(|_| "browser_operation_rejected: Browser attachment setup reached its deadline.".to_string()).and_then(|result| result),
        };
        if let Err(error) = observed {
            client.disconnect();
            return Err(error.into());
        }
        let mut tunnel = transport::Tunnel::start(client, control.clone()).await?;
        let request = json!({ "endpoint": tunnel.endpoint(), "targetId": target, "code": code, "artifactsDir": artifacts, "environment": environment, "isolatedContexts": isolated_contexts });
        let mut node = Command::new(
            std::env::var("AGENT_BROWSER_NODE_PATH").unwrap_or_else(|_| "node".into()),
        );
        if let Some(path) = environment.node_network_bootstrap_path.as_ref() {
            node.arg("--require")
                .arg(path)
                .env("AMBIT_WORKSPACE_NODE_NETWORK_BOOTSTRAP_ENABLED", "true");
        }
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
        // Results have a private inherited channel. Ordinary libraries may
        // write stdout/stderr without corrupting completion or ending custody.
        use std::os::fd::AsRawFd;
        let (result_reader, result_writer) = std::os::unix::net::UnixStream::pair()
            .map_err(|_| "browser_operation_rejected: The result channel could not be created.")?;
        result_reader.set_nonblocking(true).map_err(|_| {
            "browser_operation_rejected: The result channel could not be configured."
        })?;
        let result_reader = tokio::net::UnixStream::from_std(result_reader)
            .map_err(|_| "browser_operation_rejected: The result channel is unavailable.")?;
        let result_fd = result_writer.as_raw_fd();
        unsafe {
            node.pre_exec(move || {
                if libc::dup2(result_fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = match node.spawn() {
            Ok(child) => child,
            Err(_) => {
                tunnel.finish().await?;
                return Err("browser_operation_rejected: The installed Node Playwright runner is unavailable.".into());
            }
        };
        drop(result_writer);
        let mut process = RunnerProcess {
            pid: child.id().unwrap(),
            child,
            settled: false,
        };
        let mut stdin = process.child.stdin.take().unwrap();
        let stdout = process.child.stdout.take().unwrap();
        let stderr = process.child.stderr.take().unwrap();
        let mut logs =
            tokio::spawn(async move { tokio::join!(diagnostics(stdout), diagnostics(stderr)) });
        let mut channel = RunnerChannel::new(result_reader);
        let mut started = false;
        let execution = async {
            stdin
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .map_err(|_| "The runner did not accept its invocation.".to_string())?;
            drop(stdin);
            let mut record = channel.record().await?;
            if record == START_RECORD {
                started = true;
                record = channel.record().await?;
            }
            Ok::<_, String>(record)
        };
        let ended = tokio::select! {
            biased;
            _ = operation.canceled.changed() => Ended::Interrupted(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown)),
            _ = tokio::time::sleep_until(deadline) => Ended::Deadline,
            result = execution => match result {
                Ok(record) => Ended::Record(record),
                Err(error) => Ended::Failed(error),
            },
        };
        // Stop accepting protocol input first. A native event already admitted
        // is allowed to finish before reset; cancellation never poisons the
        // display helper by dropping its in-flight input request.
        tunnel.stop();
        let process_cleanup = process.settle().await;
        // Program code runs only after the runner delivers its start record,
        // and the runner group is stopped now: no record means no program.
        let started =
            started || (!matches!(ended, Ended::Record(_)) && channel.started_before_stop());
        let tunnel_cleanup = tunnel.finish().await;
        let input_cleanup = control
            .lock()
            .await
            .finish_agent_program(&tunnel.client)
            .await;
        let (diagnostics, diagnostics_truncated) =
            match tokio::time::timeout(Duration::from_secs(1), &mut logs).await {
                Ok(Ok(((stdout, out_truncated), (stderr, err_truncated)))) => {
                    let mut text = stdout + &stderr;
                    let mut truncated =
                        out_truncated || err_truncated || text.len() > MAX_DIAGNOSTICS;
                    if text.len() > MAX_DIAGNOSTICS {
                        let mut end = MAX_DIAGNOSTICS;
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                        truncated = true;
                    }
                    (text, truncated)
                }
                _ => {
                    logs.abort();
                    (String::new(), true)
                }
            };
        // Native refs and the selected frame outlive the program only while
        // its page keeps their document (`element::lookup_ref`,
        // `scoped_frame`), whatever the program did.
        let cleanup = process_cleanup.and(tunnel_cleanup).and(input_cleanup);
        // A program that ran and did not end cleanly may have left input
        // held; release it, and the next observation is taken afresh. One
        // that never ran sent no input and left the last observation current.
        let release = if started && (cleanup.is_err() || !matches!(ended, Ended::Record(_))) {
            control.lock().await.cancel_native_input().await
        } else {
            Ok(())
        };
        let outcome = match ended {
            // Stock Playwright attaches to every tab in the browser and waits
            // for each to commit its first navigation. One that never will
            // (a popup whose navigation was refused) keeps any program from
            // starting, whichever tab it selects; name it.
            Ended::Deadline if !started => {
                Err(not_started_by_deadline(&stalled_tabs(&owner).await).into())
            }
            ended => program_outcome(ended, started),
        }
        .map(|value| {
            json!({ "result": value, "diagnostics": diagnostics, "diagnosticsTruncated": diagnostics_truncated, "targetId": target })
        });
        after_cleanup(outcome, started, cleanup.and(release))
    }
}

/// A cleanup failure after the program ran leaves its effects unknown, and
/// keeps the program's own outcome in the report. Its data is not kept: a
/// stop that did not settle establishes none of it. Cleanup after a program
/// that never ran cannot change what happened: nothing did.
fn after_cleanup(
    outcome: Result<Value, CommandError>,
    started: bool,
    cleanup: Result<(), String>,
) -> Result<Value, CommandError> {
    match cleanup {
        Err(error) if started => Err(unknown(match outcome {
            Ok(_) => format!("{error} The program itself returned before cleanup failed."),
            Err(program) => format!("{error} The program itself reported: {}", program.error),
        })
        .into()),
        _ => outcome,
    }
}

/// Page targets that have not committed a first navigation: Playwright's
/// initial empty page. Best effort; the roster is only a diagnosis.
async fn stalled_tabs(client: &CdpClient) -> Vec<String> {
    let roster = tokio::time::timeout(
        Duration::from_secs(1),
        client.send_command_no_params("Target.getTargets", None),
    )
    .await;
    let Ok(Ok(roster)) = roster else {
        return Vec::new();
    };
    roster["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|target| target["type"] == "page" && target["url"] == "")
        .filter_map(|target| target["targetId"].as_str().map(str::to_owned))
        .collect()
}

fn not_started_by_deadline(stalled: &[String]) -> String {
    if stalled.is_empty() {
        return "browser_operation_rejected: The program did not start before its deadline.".into();
    }
    format!(
        "browser_operation_rejected: The program did not start before its deadline. Playwright attaches to every tab and waits for each to finish its first navigation; these tabs have not: {}. Close or navigate them, then run the program again.",
        stalled.join(", ")
    )
}

/// What the program's own end proves, before any cleanup failure. `started`
/// is the daemon's start record; a program cannot report itself not started.
fn program_outcome(ended: Ended, started: bool) -> Result<Value, CommandError> {
    let bytes = match ended {
        Ended::Record(bytes) => bytes,
        Ended::Interrupted(reason) if started => return Err(reason.stopped()),
        Ended::Interrupted(reason) => return Err(reason.before_start().into()),
        Ended::Deadline if started => {
            return Err(unknown("The program reached its deadline.").into())
        }
        Ended::Deadline => return Err(not_started_by_deadline(&[]).into()),
        Ended::Failed(error) if started => return Err(unknown(error).into()),
        Ended::Failed(error) => {
            return Err(
                format!("browser_operation_rejected: {error} The program was not started.").into(),
            )
        }
    };
    let malformed = || -> CommandError {
        if started {
            unknown("The runner did not produce a complete JSON result.").into()
        } else {
            "browser_operation_rejected: The Playwright runner stopped before starting the program."
                .into()
        }
    };
    let text = String::from_utf8(bytes).map_err(|_| malformed())?;
    let record: Value =
        serde_json::from_str(&replace_lone_surrogate_escapes(text)).map_err(|_| malformed())?;
    if record["success"] == true {
        return Ok(record["result"].clone());
    }
    if let Some(program) = record.get("program") {
        let failure: ProgramFailure =
            serde_json::from_value(program.clone()).map_err(|_| malformed())?;
        return Err(failure.settle(started));
    }
    // The runner itself failed: it could not attach or select the tab.
    let message = record["error"]
        .as_str()
        .unwrap_or("The Playwright program failed.");
    Err(if started {
        unknown(message)
    } else {
        format!("browser_operation_rejected: {message}")
    }
    .into())
}

/// The runner's report of a program that failed on its own: it did not
/// compile, it threw, or it returned a value JSON cannot carry. It says how
/// many Playwright calls the program issued before failing and which was
/// last, and is the failure's `data` once settled.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProgramFailure {
    error: ThrownError,
    /// None when the runner could not count: its Playwright client did not
    /// expose the calls it issues, or the attachment did not close.
    #[serde(skip_serializing_if = "Option::is_none")]
    page_calls_issued: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_page_call: Option<String>,
}

impl ProgramFailure {
    /// A program that issued no Playwright call did nothing in the browser,
    /// however far it ran: the caller fixes it and runs it again. One that
    /// issued calls, or whose calls were not counted, may have acted.
    fn settle(mut self, started: bool) -> CommandError {
        if !started {
            // No start record: no program code ran, whatever the record says.
            self.page_calls_issued = Some(0);
        }
        if self.page_calls_issued == Some(0) {
            self.last_page_call = None;
        }
        let thrown = &self.error;
        let error = match self.page_calls_issued {
            Some(0) => format!(
                "{PROGRAM_ERROR}: The program failed before it issued any Playwright call, so nothing was done in the browser. The program runs in Node, where `page` is a Playwright Page; `document` and `window` exist only inside `page.evaluate()`. Fix the program and run it again. {thrown}"
            ),
            Some(count) => {
                let last = self
                    .last_page_call
                    .as_deref()
                    .map(|call| format!(" (last: {call})"))
                    .unwrap_or_default();
                let plural = if count == 1 { "" } else { "s" };
                format!(
                    "{} {thrown}",
                    unknown(format!(
                        "The program failed after it issued {count} Playwright call{plural}{last}."
                    ))
                )
            }
            None => format!("{} {thrown}", unknown("The program failed while it ran.")),
        };
        let data = serde_json::to_value(&self).expect("a program failure is plain data");
        CommandError::with_data(error, data)
    }
}

/// What the program threw, located in the program's own lines when its
/// stack names them.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ThrownError {
    /// Absent when the program threw a value that is not an Error.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<u32>,
}

impl std::fmt::Display for ThrownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(f, "Line {line}, column {column}: ")?;
        }
        if let Some(name) = self.name.as_deref().filter(|name| !name.is_empty()) {
            write!(f, "{name}: ")?;
        }
        f.write_str(self.message.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_environment_is_host_paths_only() {
        let read = |value: Value| ProgramEnvironment::read(&json!({ ENVIRONMENT_FIELD: value }));
        assert!(ProgramEnvironment::read(&json!({})).is_ok());
        let paired = read(json!({
            "semanticJudgementConfigPath": "/actions/a/relay.json",
            "semanticJudgementClientModulePath": "/runtime/client.cjs",
            "nodeNetworkBootstrapPath": "/runtime/bootstrap.cjs",
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&paired).unwrap()["semanticJudgementConfigPath"],
            "/actions/a/relay.json"
        );
        // Absent host paths arrive as nulls from the host-bound profile.
        assert!(read(json!({
            "semanticJudgementConfigPath": null,
            "semanticJudgementClientModulePath": null,
            "nodeNetworkBootstrapPath": null,
        }))
        .is_ok());
        for invalid in [
            json!({ "semanticJudgementConfigPath": "/actions/a/relay.json" }),
            json!({ "semanticJudgementClientModulePath": "/runtime/client.cjs" }),
            json!({ "semanticJudgementConfigPath": "relay.json", "semanticJudgementClientModulePath": "/runtime/client.cjs" }),
            json!({ "nodeNetworkBootstrapPath": "bootstrap.cjs" }),
            json!({ "bearer": "not-a-path" }),
            json!("relay.json"),
        ] {
            let error = read(invalid.clone()).err().unwrap_or_default();
            assert!(
                error.starts_with("browser_operation_rejected: "),
                "{invalid}"
            );
            assert!(!error.contains("not-a-path"), "{error}");
        }
    }

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
        let queued = operations.begin().err().unwrap();
        assert!(
            queued.starts_with("browser_controlled_by_user:"),
            "{queued}"
        );
        let shutdown = operations.interrupt(InterruptReason::Shutdown);
        let queued = operations.begin().err().unwrap();
        assert!(
            queued.starts_with("browser_controlled_by_user:"),
            "{queued}"
        );
        drop(takeover);
        let queued = operations.begin().err().unwrap();
        assert!(
            queued.starts_with("browser_operation_rejected:"),
            "{queued}"
        );
        drop(shutdown);
        assert!(operations.begin().is_ok());
    }

    #[test]
    fn a_program_that_never_started_names_the_tabs_playwright_waits_for() {
        let plain = not_started_by_deadline(&[]);
        assert_eq!(
            plain,
            "browser_operation_rejected: The program did not start before its deadline."
        );
        let named = not_started_by_deadline(&["A1".into(), "B2".into()]);
        assert!(named.starts_with(&plain), "{named}");
        assert!(named.contains("these tabs have not: A1, B2."), "{named}");
        assert_eq!(program_outcome(Ended::Deadline, false), Err(plain.into()));
    }

    #[test]
    fn only_the_start_record_decides_whether_a_failure_ran_program_code() {
        let before = program_outcome(
            Ended::Failed("The runner did not accept its invocation.".into()),
            false,
        )
        .unwrap_err()
        .error;
        assert!(
            before.starts_with("browser_operation_rejected:"),
            "{before}"
        );
        let after = program_outcome(
            Ended::Failed("The program result could not be read.".into()),
            true,
        )
        .unwrap_err()
        .error;
        assert!(
            after.starts_with("browser_operation_outcome_unknown:"),
            "{after}"
        );
        let own = program_outcome(
            Ended::Record(br#"{"success":false,"started":false,"error":"boom"}"#.to_vec()),
            true,
        )
        .unwrap_err()
        .error;
        assert!(
            own.starts_with("browser_operation_outcome_unknown: boom"),
            "{own}"
        );
        assert_eq!(
            program_outcome(
                Ended::Record(br#"{"success":true,"result":7}"#.to_vec()),
                true
            ),
            Ok(json!(7))
        );
    }

    #[test]
    fn a_cleanup_failure_keeps_the_program_outcome_only_when_it_ran() {
        let failed = Err("The browser connection closed.".to_string());
        let reported = after_cleanup(
            Err(CommandError::with_data(
                "browser_operation_interrupted: stopped",
                json!({"executionStopped": true}),
            )),
            true,
            failed.clone(),
        )
        .unwrap_err();
        assert!(
            reported
                .error
                .starts_with("browser_operation_outcome_unknown: The browser connection closed."),
            "{reported:?}"
        );
        assert!(
            reported
                .error
                .contains("reported: browser_operation_interrupted: stopped"),
            "{reported:?}"
        );
        // A stop that did not settle cannot vouch for the program's facts.
        assert_eq!(reported.data, None);
        let returned = after_cleanup(Ok(json!(1)), true, failed.clone())
            .unwrap_err()
            .error;
        assert!(
            returned.contains("returned before cleanup failed"),
            "{returned}"
        );
        let never_ran = "browser_controlled_by_user: User control prevented this Playwright program from starting.";
        assert_eq!(
            after_cleanup(Err(never_ran.into()), false, failed),
            Err(never_ran.into())
        );
        assert_eq!(after_cleanup(Ok(json!(1)), true, Ok(())), Ok(json!(1)));
    }

    fn settled(record: Value, started: bool) -> CommandError {
        settled_bytes(&serde_json::to_vec(&record).unwrap(), started)
    }

    fn settled_bytes(record: &[u8], started: bool) -> CommandError {
        program_outcome(Ended::Record(record.to_vec()), started).unwrap_err()
    }

    /// The production failure: `document` read at the program's top level
    /// throws in Node before any Playwright call. Nothing reached the page,
    /// so it is a program error the model fixes, located in its own lines,
    /// and never an unknown outcome.
    #[test]
    fn a_program_that_failed_before_any_call_is_a_program_error() {
        let thrown = json!({"name":"ReferenceError","message":"document is not defined","line":1,"column":15});
        let failure = settled(
            json!({"success":false,"program":{"error":thrown,"pageCallsIssued":0}}),
            true,
        );
        assert_eq!(
            super::super::browser::error_code(&failure.error),
            Some(PROGRAM_ERROR)
        );
        assert!(
            failure.error.contains(
                "before it issued any Playwright call, so nothing was done in the browser"
            ),
            "{failure:?}"
        );
        assert!(
            failure.error.contains("The program runs in Node, where `page` is a Playwright Page; `document` and `window` exist only inside `page.evaluate()`."),
            "{failure:?}"
        );
        assert!(
            failure
                .error
                .ends_with("Line 1, column 15: ReferenceError: document is not defined"),
            "{failure:?}"
        );
        assert_eq!(
            failure.data,
            Some(json!({"error":thrown,"pageCallsIssued":0}))
        );
        // A program that did not compile never ran; it has no location.
        let syntax = settled(
            json!({"success":false,"program":{"error":{"name":"SyntaxError","message":"Unexpected token ';'"},"pageCallsIssued":0}}),
            false,
        );
        assert!(
            syntax
                .error
                .starts_with(&format!("{PROGRAM_ERROR}: The program failed before")),
            "{syntax:?}"
        );
        assert!(
            syntax
                .error
                .ends_with(". SyntaxError: Unexpected token ';'"),
            "{syntax:?}"
        );
        assert_eq!(
            syntax.data,
            Some(
                json!({"error":{"name":"SyntaxError","message":"Unexpected token ';'"},"pageCallsIssued":0})
            )
        );
        // A thrown value that is not an Error has only its text.
        let value = settled(
            json!({"success":false,"program":{"error":{"message":"boom"},"pageCallsIssued":0}}),
            true,
        );
        assert!(value.error.ends_with("run it again. boom"), "{value:?}");
    }

    /// Without its start record no program code ran, whatever the runner's
    /// record claims; with it, a counted call keeps the outcome unknown.
    #[test]
    fn only_a_started_program_can_have_issued_calls() {
        let forged = settled(
            json!({"success":false,"program":{"error":{"name":"Error","message":"x"},"pageCallsIssued":5,"lastPageCall":"page.goto"}}),
            false,
        );
        assert_eq!(
            super::super::browser::error_code(&forged.error),
            Some(PROGRAM_ERROR)
        );
        assert_eq!(
            forged.data,
            Some(json!({"error":{"name":"Error","message":"x"},"pageCallsIssued":0}))
        );
    }

    /// A program that issued calls may have acted: its outcome stays unknown
    /// and says how many calls it issued and which was last. One whose calls
    /// were not counted claims no count.
    #[test]
    fn a_program_that_failed_after_its_calls_leaves_the_outcome_unknown() {
        let thrown = json!({"name":"TimeoutError","message":"locator.click: Timeout 300ms exceeded.\nCall log:\n  - waiting for locator('#missing')\n","line":2,"column":34});
        let after = settled(
            json!({"success":false,"program":{"error":thrown,"pageCallsIssued":2,"lastPageCall":"locator.click"}}),
            true,
        );
        assert_eq!(
            super::super::browser::error_code(&after.error),
            Some("browser_operation_outcome_unknown")
        );
        assert!(
            after.error.starts_with("browser_operation_outcome_unknown: The program failed after it issued 2 Playwright calls (last: locator.click). Earlier effects may have executed. Inspect the browser before continuing; do not replay the program. Line 2, column 34: TimeoutError: locator.click: Timeout 300ms exceeded.\nCall log:"),
            "{after:?}"
        );
        assert!(!after.error.ends_with('\n'), "{after:?}");
        assert_eq!(
            after.data,
            Some(json!({"error":thrown,"pageCallsIssued":2,"lastPageCall":"locator.click"}))
        );
        let one = settled(
            json!({"success":false,"program":{"error":{"name":"Error","message":"x"},"pageCallsIssued":1,"lastPageCall":"page.goto"}}),
            true,
        );
        assert!(
            one.error
                .contains("after it issued 1 Playwright call (last: page.goto)."),
            "{one:?}"
        );
        let uncounted = settled(
            json!({"success":false,"program":{"error":{"name":"Error","message":"x"},"pageCallsIssued":null}}),
            true,
        );
        assert!(
            uncounted
                .error
                .starts_with("browser_operation_outcome_unknown: The program failed while it ran."),
            "{uncounted:?}"
        );
        assert_eq!(
            uncounted.data,
            Some(json!({"error":{"name":"Error","message":"x"}}))
        );
    }

    /// A report the runner did not write whole proves nothing about calls.
    #[test]
    fn a_malformed_program_report_is_not_trusted() {
        for program in [
            json!({"pageCallsIssued":0}),
            json!({"error":{"message":"x"},"pageCallsIssued":-1}),
            json!({"error":{"message":"x","line":"1"},"pageCallsIssued":0}),
            json!({"error":{"message":"x"},"pageCallsIssued":0,"extra":true}),
        ] {
            let record = json!({"success":false,"program":program});
            let started = settled(record.clone(), true);
            assert!(
                started
                    .error
                    .starts_with("browser_operation_outcome_unknown: The runner did not produce"),
                "{program}: {started:?}"
            );
            assert_eq!(started.data, None);
            let never = settled(record, false);
            assert!(
                never.error.starts_with("browser_operation_rejected:"),
                "{program}: {never:?}"
            );
        }
    }

    /// A program can produce a lone UTF-16 surrogate (by cutting text inside
    /// a pair), which Node's JSON.stringify writes as a `\uD800`-style escape,
    /// as Chrome's CDP serializer does. The record is decoded as the CDP
    /// transport decodes: the surrogate becomes U+FFFD and the program's own
    /// outcome is kept.
    #[test]
    fn a_lone_surrogate_in_a_record_keeps_the_program_outcome() {
        assert_eq!(
            program_outcome(
                Ended::Record(
                    br#"{"success":true,"result":{"title":"A\ud800B","pair":"\ud83d\ude00"}}"#
                        .to_vec()
                ),
                true
            ),
            Ok(json!({"title":"A\u{FFFD}B","pair":"\u{1F600}"}))
        );
        let failure = settled_bytes(
            br#"{"success":false,"program":{"error":{"name":"Error","message":"page said \udc00"},"pageCallsIssued":0}}"#,
            true,
        );
        assert_eq!(
            failure.data,
            Some(
                json!({"error":{"name":"Error","message":"page said \u{FFFD}"},"pageCallsIssued":0})
            )
        );
    }

    /// A human takeover's stop states its own facts; no other stop does.
    #[test]
    fn only_a_stop_for_human_control_reports_an_interruption() {
        let interrupted =
            program_outcome(Ended::Interrupted(InterruptReason::HumanControl), true).unwrap_err();
        assert!(
            interrupted
                .error
                .starts_with("browser_operation_interrupted: "),
            "{interrupted:?}"
        );
        assert_eq!(
            interrupted.data,
            Some(
                json!({"interruptedBy":"human","executionStopped":true,"effectsMayHaveOccurred":true})
            )
        );
        let shutdown =
            program_outcome(Ended::Interrupted(InterruptReason::Shutdown), true).unwrap_err();
        assert!(
            shutdown
                .error
                .starts_with("browser_operation_outcome_unknown: "),
            "{shutdown:?}"
        );
        assert_eq!(shutdown.data, None);
    }

    #[tokio::test]
    async fn results_and_diagnostics_have_independent_byte_bounds() {
        assert!(RunnerChannel::new(&vec![b'x'; MAX_RESULT + 1][..])
            .record()
            .await
            .is_err());
        let mut exact = vec![b'x'; MAX_RESULT - 1];
        exact.push(b'\n');
        assert_eq!(
            RunnerChannel::new(&exact[..]).record().await.unwrap().len(),
            MAX_RESULT
        );
        let (logs, truncated) = diagnostics(&vec![b'x'; MAX_DIAGNOSTICS + 1][..]).await;
        assert_eq!(logs.len(), MAX_DIAGNOSTICS);
        assert!(truncated);
    }

    #[tokio::test]
    async fn runner_records_arrive_split_and_merged_across_reads() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut channel = RunnerChannel::new(reader);
        let reading = tokio::spawn(async move {
            let first = channel.record().await.unwrap();
            let second = channel.record().await.unwrap();
            (first, second)
        });
        writer.write_all(&START_RECORD[..5]).await.unwrap();
        writer.write_all(&START_RECORD[5..]).await.unwrap();
        writer.write_all(b"{\"success\":true}\n").await.unwrap();
        drop(writer);
        let (first, second) = reading.await.unwrap();
        assert_eq!(first, START_RECORD);
        assert_eq!(second, b"{\"success\":true}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_runner_started_only_if_its_record_was_delivered() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut channel = RunnerChannel::new(tokio::net::UnixStream::from_std(reader).unwrap());
        assert!(!channel.started_before_stop());
        std::io::Write::write_all(&mut writer, START_RECORD).unwrap();
        // The writer stays open, as a surviving descendant could keep it.
        assert!(channel.started_before_stop());
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut channel = RunnerChannel::new(tokio::net::UnixStream::from_std(reader).unwrap());
        std::io::Write::write_all(&mut writer, b"{\"success\":false}\n").unwrap();
        assert!(!channel.started_before_stop());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
        let result = Box::pin(execute_command(&json!({"action":"run_playwright","code":"process.stdout.write(JSON.stringify({success:true,result:'premature'}) + String.fromCharCode(10)); await page.locator('#field').fill('retained'); await page.getByRole('button').click(); console.log('diagnostic'); return {title: await page.title(), value: await page.locator('#field').inputValue()};","timeoutMs":15000}), &mut state)).await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(
            result["data"]["result"],
            json!({"title":"Clicked","value":"retained"})
        );
        assert_eq!(
            result["data"]["diagnostics"],
            "{\"success\":true,\"result\":\"premature\"}\ndiagnostic\n"
        );
        assert_eq!(
            state.browser.as_ref().unwrap().active_target_id().unwrap(),
            target
        );
        // Text cut inside a surrogate pair holds a lone UTF-16 surrogate; the
        // result keeps the program's outcome, the surrogate replaced as the
        // CDP reader replaces one.
        let lone = Box::pin(execute_command(&json!({"action":"run_playwright","code":"return ('Ada ' + String.fromCodePoint(0x1F600)).slice(0, 5);","timeoutMs":15000}), &mut state)).await;
        assert_eq!(lone["success"], true, "{lone}");
        assert_eq!(lone["data"]["result"], "Ada \u{FFFD}");
        // A promise the program rejects and never awaits is console output,
        // not the program's outcome.
        let unobserved = Box::pin(execute_command(&json!({"action":"run_playwright","code":"Promise.reject(new Error('unobserved')); return 2;","timeoutMs":15000}), &mut state)).await;
        assert_eq!(unobserved["success"], true, "{unobserved}");
        assert_eq!(unobserved["data"]["result"], 2);
        assert!(
            unobserved["data"]["diagnostics"]
                .as_str()
                .unwrap()
                .starts_with("Unhandled rejection: Error: unobserved"),
            "{unobserved}"
        );
        let result = Box::pin(execute_command(&json!({"action":"run_playwright","code":"await page.waitForTimeout(60000);","timeoutMs":1500}), &mut state)).await;
        assert_eq!(
            result["code"], "browser_operation_outcome_unknown",
            "{result}"
        );
        let observed = Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
        assert_eq!(observed["success"], true, "{observed}");
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

    /// A program whose own attachment ends, and one whose browser connection
    /// is lost while it waits for an event, settle promptly as unknown
    /// outcomes rather than at their deadlines. The retained browser keeps
    /// its tab after the first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_lost_transports_settle_before_the_deadline() {
        use crate::native::actions::execute_command;
        let mut state = DaemonState::new();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":"data:text/html,<title>Retained</title>"}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();

        let began = std::time::Instant::now();
        let detached = Box::pin(execute_command(&json!({"action":"run_playwright","timeoutMs":30000,"code":"await browser.close(); return await page.title();"}), &mut state)).await;
        let detached_ms = began.elapsed().as_millis();
        assert_eq!(
            detached["code"], "browser_operation_outcome_unknown",
            "{detached}"
        );
        assert!(detached_ms < 10_000, "{detached_ms} ms");
        let observed = Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
        assert_eq!(observed["success"], true, "{observed}");
        let title = Box::pin(execute_command(&json!({"action":"title"}), &mut state)).await;
        assert_eq!(title["data"]["title"], "Retained", "{title}");
        assert_eq!(
            state.browser.as_ref().unwrap().active_target_id().unwrap(),
            target
        );

        let processes = state
            .browser
            .as_ref()
            .unwrap()
            .client
            .send_command_no_params("SystemInfo.getProcessInfo", None)
            .await
            .unwrap();
        let pid = processes["processInfo"]
            .as_array()
            .unwrap()
            .iter()
            .find(|process| process["type"] == "browser")
            .and_then(|process| process["id"].as_i64())
            .unwrap();
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        });
        let began = std::time::Instant::now();
        let lost = Box::pin(execute_command(&json!({"action":"run_playwright","timeoutMs":30000,"code":"await page.waitForEvent('popup', {timeout: 0}); return 'unreachable';"}), &mut state)).await;
        let lost_ms = began.elapsed().as_millis();
        killer.await.unwrap();
        eprintln!("SETTLE detached={detached_ms}ms lost={lost_ms}ms");
        assert_eq!(lost["code"], "browser_operation_outcome_unknown", "{lost}");
        assert!(lost_ms < 10_000, "{lost_ms} ms: {lost}");
        let _ = Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    /// A failed program settles by the Playwright calls it issued, in the
    /// owned window production uses. The production failure, `document` read
    /// at the top level of the Node program, throws before any call: a
    /// program error located in the program's own lines, with the page
    /// untouched. A wait that saw nothing and a program that does not compile
    /// issue no call either. A program that acted and then failed stays an
    /// unknown outcome naming its calls, and its effect is on the page. A call
    /// the program leaves running when it fails is never taken for no call,
    /// and only the runner's own close of the attachment goes uncounted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium, the browser display helper and installed playwright-core 1.62.1"]
    async fn e2e_playwright_program_errors_settle_by_the_calls_they_issued() {
        use crate::native::actions::execute_command;
        use crate::test_utils::EnvGuard;
        let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
        env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
        env.set("DISPLAY", "");
        let mut state = DaemonState::new();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":"data:text/html,<title>Untouched</title><button>Go</button>"}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        assert!(state.browser_control.lock().await.has_native_display());
        let target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        async fn program(state: &mut DaemonState, code: &str) -> Value {
            Box::pin(execute_command(
                &json!({"action":"run_playwright","timeoutMs":15000,"code":code}),
                state,
            ))
            .await
        }
        async fn title(state: &mut DaemonState) -> Value {
            Box::pin(execute_command(&json!({"action":"title"}), state)).await["data"]["title"]
                .clone()
        }

        let refused = program(
            &mut state,
            "const rows = document.querySelectorAll('tr');\nreturn rows.length;",
        )
        .await;
        eprintln!("PROGRAM ERROR {refused}");
        assert_eq!(refused["success"], false, "{refused}");
        assert_eq!(refused["code"], PROGRAM_ERROR, "{refused}");
        assert_eq!(
            refused["data"],
            json!({"error":{"name":"ReferenceError","message":"document is not defined","line":1,"column":14},"pageCallsIssued":0})
        );
        let text = refused["error"].as_str().unwrap();
        assert!(
            text.contains(
                "before it issued any Playwright call, so nothing was done in the browser"
            ) && text.contains("`document` and `window` exist only inside `page.evaluate()`")
                && text.ends_with("Line 1, column 14: ReferenceError: document is not defined"),
            "{text}"
        );

        let waited = program(
            &mut state,
            "await page.waitForEvent('popup', { timeout: 200 });",
        )
        .await;
        assert_eq!(waited["code"], PROGRAM_ERROR, "{waited}");
        assert_eq!(waited["data"]["pageCallsIssued"], 0, "{waited}");
        assert_eq!(waited["data"]["error"]["name"], "TimeoutError", "{waited}");
        assert_eq!(waited["data"]["error"]["line"], 1, "{waited}");
        assert_eq!(waited["data"]["error"]["column"], 12, "{waited}");

        // A promise the program rejects and never awaits does not end the
        // runner before it reports.
        let unobserved = program(
            &mut state,
            "Promise.reject(new Error('unobserved'));\nconst rows = document.querySelectorAll('tr');",
        )
        .await;
        assert_eq!(unobserved["code"], PROGRAM_ERROR, "{unobserved}");
        assert_eq!(
            unobserved["data"],
            json!({"error":{"name":"ReferenceError","message":"document is not defined","line":2,"column":14},"pageCallsIssued":0})
        );

        let syntax = program(&mut state, "const a = ;").await;
        assert_eq!(syntax["code"], PROGRAM_ERROR, "{syntax}");
        assert_eq!(
            syntax["data"],
            json!({"error":{"name":"SyntaxError","message":"Unexpected token ';'"},"pageCallsIssued":0})
        );
        assert_eq!(title(&mut state).await, "Untouched");

        let acted = program(
            &mut state,
            "await page.evaluate(() => { document.title = 'Touched'; });\nawait page.locator('#missing').click({ timeout: 300 });",
        )
        .await;
        eprintln!("ACTED {acted}");
        assert_eq!(
            acted["code"], "browser_operation_outcome_unknown",
            "{acted}"
        );
        assert_eq!(acted["data"]["pageCallsIssued"], 2, "{acted}");
        assert_eq!(acted["data"]["lastPageCall"], "locator.click", "{acted}");
        assert_eq!(acted["data"]["error"]["name"], "TimeoutError", "{acted}");
        assert_eq!(acted["data"]["error"]["line"], 2, "{acted}");
        assert_eq!(acted["data"]["error"]["column"], 32, "{acted}");
        let message = acted["data"]["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("locator.click: Timeout 300ms exceeded.")
                && !message.contains('\u{1b}'),
            "{message:?}"
        );
        assert!(
            acted["error"].as_str().unwrap().contains(
                "The program failed after it issued 2 Playwright calls (last: locator.click). Earlier effects may have executed."
            ),
            "{acted}"
        );
        assert_eq!(title(&mut state).await, "Touched");

        // A call scheduled before the failure and issued while the runner
        // closes its attachment: whichever lands first, a program error
        // always means the page was not touched.
        let reset = Box::pin(execute_command(
            &json!({"action":"evaluate","script":"document.title = 'Untouched'"}),
            &mut state,
        ))
        .await;
        assert_eq!(reset["success"], true, "{reset}");
        let floating = program(
            &mut state,
            "setTimeout(() => page.evaluate(() => { document.title = 'Floating'; }), 0);\nthrow new Error('scheduled');",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = title(&mut state).await;
        eprintln!(
            "FLOATING code={} calls={} title={after}",
            floating["code"], floating["data"]["pageCallsIssued"]
        );
        match floating["code"].as_str() {
            Some(PROGRAM_ERROR) => assert_eq!(after, "Untouched", "{floating}"),
            Some("browser_operation_outcome_unknown") => {}
            _ => panic!("{floating}"),
        }

        // Only the runner's own close goes uncounted: a program that wraps
        // `browser.close` to act on the page first is counted.
        let wrapped = program(
            &mut state,
            "const close = browser.close.bind(browser);\nbrowser.close = (...args) => { page.evaluate(() => { document.title = 'Wrapped'; }).catch(() => {}); return close(...args); };\nthrow new Error('wrapped');",
        )
        .await;
        assert_eq!(
            wrapped["code"], "browser_operation_outcome_unknown",
            "{wrapped}"
        );
        assert_eq!(wrapped["data"]["pageCallsIssued"], 1, "{wrapped}");
        assert_eq!(
            wrapped["data"]["lastPageCall"], "page.evaluate",
            "{wrapped}"
        );
        eprintln!("WRAPPED title={}", title(&mut state).await);

        assert_eq!(
            state.browser.as_ref().unwrap().active_target_id().unwrap(),
            target
        );
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_frames_popups_files_and_real_pointer_share_native_owners() {
        use crate::native::actions::execute_command;
        let mut state = DaemonState::new();
        let artifacts =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/playwright-e2e");
        std::fs::create_dir_all(&artifacts).unwrap();
        let output = serde_json::to_string(&artifacts).unwrap();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":"about:blank"}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        let code = r#"
const {createServer} = await import('node:http');
const {readFile} = await import('node:fs/promises');
const server = createServer((request, response) => {
  if (request.url === '/file') {
    response.writeHead(200, {'Content-Type':'text/plain','Content-Disposition':'attachment; filename=report.txt'});
    response.end('native download owner'); return;
  }
  response.setHeader('Content-Type','text/html');
  if (request.url === '/frame') response.end('<input aria-label="Frame field"><button onclick="document.title=\'Frame clicked\'">Frame action</button>');
  else if (request.url === '/popup') response.end('<title>Popup</title><button onclick="document.title=\'Popup clicked\'">Popup action</button>');
  else response.end(`<title>Playwright qualification</title><style>body{font:18px system-ui;padding:24px;height:2500px}button,input,a{margin:8px}iframe{display:block;width:600px;height:160px}</style><h1>Shared browser</h1><input aria-label="Name"><input type=file aria-label="Upload"><button id=dom onclick="window.domClicked=true">DOM action</button><a target=_blank href=/popup>Open popup</a><a href=/file>Download</a><iframe src="http://localhost:${server.address().port}/frame"></iframe><script>window.pointers=[];addEventListener('pointermove',e=>pointers.push({x:e.clientX,y:e.clientY,screenX:e.screenX,screenY:e.screenY,trusted:e.isTrusted}),true)</script>`);
});
await new Promise(resolve => server.listen(0, '0.0.0.0', resolve));
try {
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await context.addCookies([{name:'retained',value:'yes',url:page.url()}]);
  await page.getByRole('textbox', {name:'Name', exact:true}).fill('Ada');
  await page.getByRole('textbox', {name:'Name', exact:true}).press('End');
  await page.keyboard.type(' Lovelace');
  await page.frameLocator('iframe').getByRole('textbox', {name:'Frame field'}).fill('Frame retained');
  await page.frameLocator('iframe').getByRole('button', {name:'Frame action'}).click();
  const [popup] = await Promise.all([page.waitForEvent('popup'), page.getByRole('link', {name:'Open popup'}).click()]);
  await popup.waitForLoadState();
  await popup.getByRole('button', {name:'Popup action'}).click();
  const popupTitle = await popup.title();
  await popup.close();
  await page.bringToFront();
  await page.getByLabel('Upload').setInputFiles({name:'hello.txt',mimeType:'text/plain',buffer:Buffer.from('uploaded')});
  const [download] = await Promise.all([page.waitForEvent('download'), page.getByRole('link', {name:'Download',exact:true}).click()]);
  await download.saveAs(OUTPUT + '/report.txt');
  const text = await readFile(OUTPUT + '/report.txt', 'utf8');
  await page.mouse.move(210, 110);
  const pointer = await page.evaluate(() => pointers.at(-1));
  const beforeDomClick = await page.evaluate(() => pointers.length);
  await page.evaluate(() => document.querySelector('#dom').click());
  const afterDomClick = await page.evaluate(() => pointers.length);
  await page.mouse.wheel(0, 180);
  await page.waitForFunction(() => scrollY > 0);
  await page.screenshot({path:OUTPUT + '/page.png'});
  return {name:await page.getByRole('textbox', {name:'Name',exact:true}).inputValue(), frame:await page.frameLocator('iframe').getByRole('textbox').inputValue(), popupTitle, text, pointer, beforeDomClick, afterDomClick, uploaded:await page.getByLabel('Upload').evaluate(el=>el.files[0].name), cookies:await context.cookies(), scroll:await page.evaluate(()=>scrollY)};
} finally { await new Promise(resolve => server.close(resolve)); }
"#.replace("OUTPUT", &output);
        let result = Box::pin(execute_command(
            &json!({"action":"run_playwright","code":code,"timeoutMs":45000}),
            &mut state,
        ))
        .await;
        assert_eq!(result["success"], true, "{result}");
        let values = &result["data"]["result"];
        assert_eq!(values["name"], "Ada Lovelace");
        assert_eq!(values["frame"], "Frame retained");
        assert_eq!(values["popupTitle"], "Popup clicked");
        assert_eq!(values["text"], "native download owner");
        assert_eq!(values["uploaded"], "hello.txt");
        assert_eq!(values["pointer"]["trusted"], true);
        assert_eq!(values["pointer"]["x"], 210);
        assert_eq!(values["pointer"]["y"], 110);
        assert_eq!(values["beforeDomClick"], values["afterDomClick"]);
        assert!(values["scroll"].as_f64().unwrap() > 0.0);
        assert!(values["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cookie| cookie["name"] == "retained" && cookie["value"] == "yes"));
        assert!(std::fs::metadata(artifacts.join("page.png")).unwrap().len() > 100);
        assert_eq!(
            state.browser.as_ref().unwrap().active_target_id().unwrap(),
            target
        );
        // Resized geometry: real pointer coordinates still land where the
        // program asked after the native owner changes the page size.
        let resized = Box::pin(execute_command(
            &json!({"action":"viewport","width":900,"height":600}),
            &mut state,
        ))
        .await;
        assert_eq!(resized["success"], true, "{resized}");
        // The earlier wheel scrolled the iframe under this point; return to
        // the top so the pointer addresses the main document.
        let moved = Box::pin(execute_command(&json!({"action":"run_playwright","code":"await page.evaluate(() => scrollTo(0, 0)); await page.mouse.move(150, 90); return {pointer: await page.evaluate(() => pointers.at(-1)), width: await page.evaluate(() => innerWidth)};","timeoutMs":15000}), &mut state)).await;
        assert_eq!(moved["success"], true, "{moved}");
        assert_eq!(moved["data"]["result"]["width"], 900, "{moved}");
        assert_eq!(
            moved["data"]["result"]["pointer"]["trusted"], true,
            "{moved}"
        );
        assert_eq!(moved["data"]["result"]["pointer"]["x"], 150, "{moved}");
        assert_eq!(moved["data"]["result"]["pointer"]["y"], 90, "{moved}");
        if let Some(display) = state.browser.as_ref().unwrap().display_client() {
            use base64::Engine;
            let (capture, _) = display
                .capture(crate::native::display::CaptureRequest {
                    cursor: true,
                    budget_bytes: 0,
                    force: true,
                    patches: false,
                })
                .await
                .unwrap()
                .unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(capture.data.unwrap())
                .unwrap();
            std::fs::write(artifacts.join("native-window.jpg"), bytes).unwrap();
        }
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_windows_share_profile_and_isolated_contexts_are_never_misrepresented() {
        use crate::native::actions::execute_command;
        let mut state = DaemonState::new();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":"about:blank"}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let first_target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        let client = state.browser.as_ref().unwrap().client.clone();
        client.send_command("Storage.setCookies", Some(json!({"cookies":[{"name":"account","value":"persistent","url":"https://account.example/"}]})), None).await.unwrap();
        // A shared window is a second tab of the profile's own context.
        let created = Box::pin(execute_command(
            &json!({"action":"window_new","shared":true}),
            &mut state,
        ))
        .await;
        assert_eq!(created["success"], true, "{created}");
        assert_eq!(created["data"]["shared"], true);
        let target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        assert_ne!(target, first_target);
        // Both native tabs are pages of one shared context. Chrome may keep
        // internal pages of its own in that context, so membership is exact
        // and the count is not.
        let read = Box::pin(execute_command(&json!({"action":"run_playwright","code":"const targets = []; for (const page of context.pages()) { const session = await context.newCDPSession(page); targets.push((await session.send('Target.getTargetInfo')).targetInfo.targetId); await session.detach(); } return { cookies: (await context.cookies()).map(cookie => cookie.name + '=' + cookie.value), targets };","timeoutMs":15000}), &mut state)).await;
        assert_eq!(read["success"], true, "{read}");
        let targets = read["data"]["result"]["targets"].as_array().unwrap();
        assert!(targets.contains(&json!(first_target)), "{read}");
        assert!(targets.contains(&json!(target)), "{read}");
        assert!(read["data"]["result"]["cookies"]
            .as_array()
            .unwrap()
            .contains(&json!("account=persistent")));
        // The default window has its own context, without the profile's cookies.
        let isolated = Box::pin(execute_command(&json!({"action":"window_new"}), &mut state)).await;
        assert_eq!(isolated["success"], true, "{isolated}");
        assert_eq!(isolated["data"]["shared"], false);
        let isolated_target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        let info = client
            .send_command(
                "Target.getTargetInfo",
                Some(json!({"targetId":isolated_target})),
                None,
            )
            .await
            .unwrap();
        let isolated_context = info["targetInfo"]["browserContextId"].clone();
        assert!(isolated_context.is_string());
        let cookies = client
            .send_command(
                "Storage.getCookies",
                Some(json!({"browserContextId":isolated_context})),
                None,
            )
            .await
            .unwrap();
        assert!(cookies["cookies"].as_array().unwrap().is_empty());
        // With an isolated window open, the installed client either represents
        // each tab's real context (adoption build) or refuses before starting
        // (stock client). A merged, misreported context is never an outcome.
        let cookie_values = "return (await context.cookies()).map(cookie => cookie.value);";
        let mut adopted = None;
        for (selected, expects_persistent) in [(&isolated_target, false), (&target, true)] {
            let read = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":selected,"code":cookie_values,"timeoutMs":15000}), &mut state)).await;
            let outcome = read["success"] == true;
            assert_eq!(*adopted.get_or_insert(outcome), outcome, "{read}");
            if outcome {
                let values = read["data"]["result"].as_array().unwrap();
                assert_eq!(
                    values.contains(&json!("persistent")),
                    expects_persistent,
                    "{read}"
                );
            } else {
                assert_eq!(read["code"], "browser_operation_rejected", "{read}");
                assert!(
                    read["error"].as_str().unwrap().contains("isolated window"),
                    "{read}"
                );
            }
        }
        if let Ok(stock) = std::env::var("AGENT_BROWSER_TEST_STOCK_PLAYWRIGHT_MODULE") {
            let env = crate::test_utils::EnvGuard::new(&["AGENT_BROWSER_PLAYWRIGHT_MODULE"]);
            env.set("AGENT_BROWSER_PLAYWRIGHT_MODULE", &stock);
            for selected in [&isolated_target, &target] {
                let read = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":selected,"code":"throw new Error('PROGRAM MUST NOT START');","timeoutMs":15000}), &mut state)).await;
                assert_eq!(read["code"], "browser_operation_rejected", "{read}");
                let error = read["error"].as_str().unwrap();
                assert!(error.contains("isolated window"), "{read}");
                assert!(!error.contains("PROGRAM MUST NOT START"), "{read}");
            }
        }
        // Closing the isolated window through the native tools discards its
        // context with it, so no client is left with anything to misrepresent.
        let closed = Box::pin(execute_command(
            &json!({"action":"tab_close","tabId":isolated_target}),
            &mut state,
        ))
        .await;
        assert_eq!(closed["success"], true, "{closed}");
        assert!(!state
            .browser
            .as_ref()
            .unwrap()
            .isolated_context_ids()
            .await
            .unwrap()
            .contains(&isolated_context.as_str().unwrap().to_owned()));
        let selected = Box::pin(execute_command(
            &json!({"action":"tab_switch","tabId":target}),
            &mut state,
        ))
        .await;
        assert_eq!(selected["success"], true, "{selected}");
        let observed = Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
        assert_eq!(observed["success"], true, "{observed}");
        // Without isolated contexts every client attaches and sees the profile.
        let read = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":target,"code":cookie_values,"timeoutMs":15000}), &mut state)).await;
        assert_eq!(read["success"], true, "{read}");
        assert!(read["data"]["result"]
            .as_array()
            .unwrap()
            .contains(&json!("persistent")));
        if let Ok(stock) = std::env::var("AGENT_BROWSER_TEST_STOCK_PLAYWRIGHT_MODULE") {
            let env = crate::test_utils::EnvGuard::new(&["AGENT_BROWSER_PLAYWRIGHT_MODULE"]);
            env.set("AGENT_BROWSER_PLAYWRIGHT_MODULE", &stock);
            let read = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":target,"code":cookie_values,"timeoutMs":15000}), &mut state)).await;
            assert_eq!(read["success"], true, "{read}");
        }
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    /// Playwright pipelines each page's setup (Runtime.enable before
    /// Runtime.runIfWaitingForDebugger) and relies on Chrome running it in
    /// order. Sent out of order on the daemon's multi-threaded runtime, an
    /// attachment occasionally lost the main context and hung to its deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_repeated_attachments_keep_every_page_context() {
        use crate::native::actions::execute_command;
        let (port, server) = serve_pages(
            "<title>Main</title><p>main text</p>",
            "<title>Popup</title><p>popup text</p>",
        )
        .await;
        let mut state = DaemonState::new();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":format!("http://127.0.0.1:{port}/")}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let code = format!("const title = await page.evaluate(() => document.title); const text = await page.locator('p').innerText(); const [popup] = await Promise.all([page.waitForEvent('popup'), page.evaluate(() => {{ window.open('http://127.0.0.1:{port}/frame'); }})]); await popup.waitForLoadState('load'); const popupText = await popup.locator('p').innerText(); await popup.close(); return {{ title, text, popupText }};");
        let mut failures = Vec::new();
        for run in 0..64 {
            let result = Box::pin(execute_command(
                &json!({"action":"run_playwright","timeoutMs":8000,"code":code}),
                &mut state,
            ))
            .await;
            if result["data"]["result"]
                != json!({"title":"Main","text":"main text","popupText":"popup text"})
            {
                failures.push((run, result));
                // An unknown outcome requires a fresh observation first.
                Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
            }
        }
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
        server.abort();
        assert!(failures.is_empty(), "{failures:?}");
    }

    /// Stock Playwright waits for every tab's first navigation before any
    /// program starts. A popup that never navigates is named, closing it
    /// through the native tools recovers, and nothing ran meanwhile.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium and installed playwright-core 1.62.1"]
    async fn e2e_playwright_names_a_tab_that_never_navigated_and_recovers_after_closing_it() {
        use crate::native::actions::execute_command;
        let mut state = DaemonState::new();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":"data:text/html,<title>Main</title><p>main</p>"}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        let main = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        // Chrome refuses a renderer-initiated top-level data: navigation, so
        // this popup stays on its initial empty document.
        let opener = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":main,"timeoutMs":15000,"code":"await page.evaluate(() => { window.open('data:text/html,<p>never</p>'); }); return true;"}), &mut state)).await;
        assert_eq!(opener["success"], true, "{opener}");
        let client = state.browser.as_ref().unwrap().client.clone();
        let stalled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let found = stalled_tabs(&client).await;
                if !found.is_empty() {
                    break found;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(stalled.len(), 1, "{stalled:?}");
        let blocked = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":main,"timeoutMs":3000,"code":"await page.evaluate(() => { document.title = 'RAN'; }); return true;"}), &mut state)).await;
        assert_eq!(blocked["code"], "browser_operation_rejected", "{blocked}");
        assert!(
            blocked["error"].as_str().unwrap().contains(&stalled[0]),
            "{blocked}"
        );
        let closed = Box::pin(execute_command(
            &json!({"action":"tab_close","tabId":stalled[0]}),
            &mut state,
        ))
        .await;
        assert_eq!(closed["success"], true, "{closed}");
        let selected = Box::pin(execute_command(
            &json!({"action":"tab_switch","tabId":main}),
            &mut state,
        ))
        .await;
        assert_eq!(selected["success"], true, "{selected}");
        let read = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":main,"timeoutMs":15000,"code":"return await page.title();"}), &mut state)).await;
        assert_eq!(read["success"], true, "{read}");
        assert_eq!(
            read["data"]["result"], "Main",
            "the blocked program ran: {read}"
        );
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    /// Serves `/` and `/frame` from one port; `PORT` in a body becomes that
    /// port. Requesting the frame through another host name makes Chrome
    /// place it in its own renderer process.
    async fn serve_pages(main: &str, frame: &str) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (main, frame) = (
            main.replace("PORT", &port.to_string()),
            frame.replace("PORT", &port.to_string()),
        );
        let task = tokio::spawn(async move {
            // Chrome may preconnect sockets that never send a request, so
            // each connection is read on its own task.
            while let Ok((mut stream, _)) = listener.accept().await {
                let (main, frame) = (main.clone(), frame.clone());
                tokio::spawn(async move {
                    let mut request = vec![0u8; 8192];
                    let count = stream.read(&mut request).await.unwrap_or(0);
                    let body = if request[..count].starts_with(b"GET /frame ") {
                        frame
                    } else {
                        main
                    };
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (port, task)
    }

    /// Real Playwright pointer input on a scrolled page, inside an
    /// out-of-process frame and after the native window is resized, lands at
    /// the requested client points and is published as the owner's activity.
    /// DOM `element.click()` publishes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium, the browser display helper and installed playwright-core 1.62.1"]
    async fn e2e_playwright_pointer_reaches_scrolled_frames_after_resize_and_is_published() {
        use crate::native::actions::execute_command;
        use crate::test_utils::EnvGuard;
        let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
        env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
        env.set("DISPLAY", "");
        let log = "<script>window.pointerLog=[];for(const type of ['pointermove','pointerdown','pointerup'])addEventListener(type,e=>pointerLog.push({type,x:e.clientX,y:e.clientY,trusted:e.isTrusted}),true)</script>";
        let (port, server) = serve_pages(
            &format!("<!doctype html><title>Scrolled frames</title><body style='margin:0;height:3000px'><div style='height:400px'></div><iframe src='http://localhost:PORT/frame' style='display:block;border:0;margin-left:40px;width:400px;height:300px'></iframe><button id=dom onclick='window.domClicked=true'>DOM</button>{log}"),
            &format!("<!doctype html><body style='margin:0;height:600px'><button id=inside style='position:absolute;left:130px;top:120px;width:120px;height:40px' onclick='window.clicked=event.isTrusted'>Inside</button>{log}"),
        )
        .await;
        let mut state = DaemonState::new();
        let opened = Box::pin(execute_command(
            &json!({"action":"navigate","url":"about:blank"}),
            &mut state,
        ))
        .await;
        assert_eq!(opened["success"], true, "{opened}");
        assert!(state.browser_control.lock().await.has_native_display());
        let resized = Box::pin(execute_command(
            &json!({"action":"viewport","width":900,"height":600}),
            &mut state,
        ))
        .await;
        assert_eq!(resized["success"], true, "{resized}");
        let navigated = Box::pin(execute_command(
            &json!({"action":"navigate","url":format!("http://127.0.0.1:{port}/")}),
            &mut state,
        ))
        .await;
        assert_eq!(navigated["success"], true, "{navigated}");
        let scrolled = Box::pin(execute_command(
            &json!({"action":"evaluate","script":"scrollTo(0, 300); [scrollY, innerWidth]"}),
            &mut state,
        ))
        .await;
        assert_eq!(scrolled["data"]["result"], json!([300, 900]), "{scrolled}");
        let observed = Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
        assert_eq!(observed["success"], true, "{observed}");
        let client = state.browser.as_ref().unwrap().client.clone();
        let session = state
            .browser
            .as_ref()
            .unwrap()
            .active_session_id()
            .unwrap()
            .to_owned();
        let mut published = client.subscribe();
        // At this scroll the iframe occupies client x 40..440, y 100..400;
        // its button spans frame x 130..250, y 120..160.
        let code = r#"
const frame = page.frames().find(candidate => candidate !== page.mainFrame());
await page.mouse.move(100, 50);
await page.mouse.move(140, 150);
await page.mouse.click(190, 240);
await page.evaluate(() => document.querySelector('#dom').click());
// The owned window's wheel is discrete: 100 units per notch.
await page.mouse.move(600, 50);
await page.mouse.wheel(0, 200);
await page.waitForFunction(() => scrollY > 300);
return {main: await page.evaluate(() => pointerLog), frame: await frame.evaluate(() => pointerLog), framePath: await frame.evaluate(() => location.pathname), clicked: await frame.evaluate(() => window.clicked === true), domClicked: await page.evaluate(() => window.domClicked === true), scrollY: await page.evaluate(() => scrollY)};
"#;
        let result = Box::pin(execute_command(
            &json!({"action":"run_playwright","code":code,"timeoutMs":30000}),
            &mut state,
        ))
        .await;
        server.abort();
        assert_eq!(result["success"], true, "{result}");
        let values = &result["data"]["result"];
        let trusted = |log: &Value, kind: &str, x: i64, y: i64| {
            log.as_array().unwrap().iter().any(|event| {
                event["type"] == kind
                    && event["x"] == x
                    && event["y"] == y
                    && event["trusted"] == true
            })
        };
        assert_eq!(values["framePath"], "/frame", "{values}");
        assert!(trusted(&values["main"], "pointermove", 100, 50), "{values}");
        assert!(trusted(&values["main"], "pointermove", 600, 50), "{values}");
        assert!(
            trusted(&values["frame"], "pointermove", 100, 50),
            "{values}"
        );
        assert!(
            trusted(&values["frame"], "pointerdown", 150, 140),
            "{values}"
        );
        assert!(trusted(&values["frame"], "pointerup", 150, 140), "{values}");
        assert_eq!(values["clicked"], true, "{values}");
        assert_eq!(values["domClicked"], true, "{values}");
        assert!(values["scrollY"].as_f64().unwrap() > 300.0, "{values}");
        let mut activity = Vec::new();
        while let Ok(event) = published.try_recv() {
            if event.method == crate::native::activity::EVENT
                && event.params["source"] == "agent"
                && event.params["type"] == "pointer"
                && event.session_id.as_deref() == Some(session.as_str())
            {
                activity.push((
                    event.params["eventType"].as_str().unwrap().to_owned(),
                    event.params["x"].as_f64().unwrap(),
                    event.params["y"].as_f64().unwrap(),
                ));
            }
        }
        for (kind, x, y) in [
            ("move", 100.0, 50.0),
            ("move", 140.0, 150.0),
            ("press", 190.0, 240.0),
            ("release", 190.0, 240.0),
            ("scroll", 600.0, 50.0),
        ] {
            assert!(
                activity
                    .iter()
                    .any(|(k, ax, ay)| k == kind && *ax == x && *ay == y),
                "{kind} {x},{y} missing from {activity:?}"
            );
        }
        // Every published agent pointer event is one of the program's real
        // input points; the DOM click contributed none.
        assert!(
            activity.iter().all(|(_, x, y)| {
                [(100.0, 50.0), (140.0, 150.0), (190.0, 240.0), (600.0, 50.0)].contains(&(*x, *y))
            }),
            "{activity:?}"
        );
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    /// One Chrome process, profile and tab across native commands, Playwright
    /// programs, a human click and new tabs, in the native window mode that
    /// production uses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local Chromium, the browser display helper and installed playwright-core 1.62.1"]
    async fn e2e_playwright_and_native_share_one_chrome_across_human_control_and_new_tabs() {
        use crate::native::actions::execute_command;
        use crate::test_utils::EnvGuard;
        let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
        env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
        env.set("DISPLAY", "");
        let mut state = DaemonState::new();
        let html = "<!doctype html><title>Journey</title><style>body{margin:0}input{position:absolute;left:40px;top:40px;width:200px;height:32px}button{position:absolute;left:40px;top:120px;width:160px;height:48px}</style><input id=visitor aria-label=Name><button id=go onclick=\"document.title='Human '+visitor.value\">Go</button>";
        let opened = Box::pin(execute_command(&json!({"action":"navigate","url":format!("data:text/html,{}", urlencoding::encode(html))}), &mut state)).await;
        assert_eq!(opened["success"], true, "{opened}");
        assert!(state.browser_control.lock().await.has_native_display());
        let client = state.browser.as_ref().unwrap().client.clone();
        let session = state
            .browser
            .as_ref()
            .unwrap()
            .active_session_id()
            .unwrap()
            .to_owned();
        let target = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        async fn browser_pid(client: &CdpClient) -> u64 {
            let processes = client
                .send_command_no_params("SystemInfo.getProcessInfo", None)
                .await
                .unwrap();
            processes["processInfo"]
                .as_array()
                .unwrap()
                .iter()
                .find(|process| process["type"] == "browser")
                .and_then(|process| process["id"].as_u64())
                .unwrap()
        }
        let pid = browser_pid(&client).await;
        client.send_command("Storage.setCookies", Some(json!({"cookies":[{"name":"journey","value":"shared","url":"https://journey.example/"}]})), None).await.unwrap();

        let filled = Box::pin(execute_command(&json!({"action":"run_playwright","timeoutMs":15000,"code":"await page.getByLabel('Name').fill('Ada'); return {title: await page.title(), cookies: (await context.cookies('https://journey.example/')).map(cookie => cookie.name + '=' + cookie.value)};"}), &mut state)).await;
        assert_eq!(filled["success"], true, "{filled}");
        assert_eq!(filled["data"]["result"]["title"], "Journey");
        assert_eq!(
            filled["data"]["result"]["cookies"],
            json!(["journey=shared"])
        );
        assert_eq!(filled["data"]["targetId"], target);

        let observed = Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
        assert_eq!(observed["success"], true, "{observed}");
        assert!(
            observed["data"]["snapshot"]
                .as_str()
                .unwrap()
                .contains("Name"),
            "{observed}"
        );
        let typed = Box::pin(execute_command(
            &json!({"action":"evaluate","script":"visitor.value"}),
            &mut state,
        ))
        .await;
        assert_eq!(typed["data"]["result"], "Ada", "{typed}");

        // A human takes control and clicks the button in the same window.
        let controller = uuid::Uuid::new_v4().to_string();
        let expires = crate::native::stream::timestamp_ms() + 30_000;
        let acquired = Box::pin(execute_command(&json!({"action":crate::native::browser_control::ACTION,"op":"acquire","controllerId":controller,"expiresAt":expires}), &mut state)).await;
        assert_eq!(acquired["success"], true, "{acquired}");
        let refused = Box::pin(execute_command(
            &json!({"action":"run_playwright","timeoutMs":15000,"code":"return 1;"}),
            &mut state,
        ))
        .await;
        assert_eq!(refused["code"], "browser_controlled_by_user", "{refused}");
        let (x, y, surface) = crate::native::e2e_tests::window_point(&state, 120.0, 144.0).await;
        let click = |event: &str| json!({"type":"input_mouse","eventType":event,"x":x,"y":y,"button":"left","clickCount":1});
        let input = Box::pin(execute_command(&json!({"action":crate::native::browser_control::ACTION,"op":"input","controllerId":controller,"sequence":1,"expectedSurfaceGeneration":surface,"events":[click("mousePressed"), click("mouseReleased")]}), &mut state)).await;
        assert_eq!(input["success"], true, "{input}");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let title = client
                    .send_command(
                        "Runtime.evaluate",
                        Some(json!({"expression":"document.title","returnByValue":true})),
                        Some(&session),
                    )
                    .await
                    .unwrap();
                if title["result"]["value"] == "Human Ada" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the human click reached the shared page");
        let released = Box::pin(execute_command(&json!({"action":crate::native::browser_control::ACTION,"op":"release","controllerId":controller}), &mut state)).await;
        assert_eq!(released["success"], true, "{released}");
        let observed = Box::pin(execute_command(&json!({"action":"snapshot"}), &mut state)).await;
        assert_eq!(observed["success"], true, "{observed}");

        // Playwright continues on the page the human changed.
        let continued = Box::pin(execute_command(&json!({"action":"run_playwright","timeoutMs":15000,"code":"return {title: await page.title(), value: await page.getByLabel('Name').inputValue()};"}), &mut state)).await;
        assert_eq!(continued["success"], true, "{continued}");
        assert_eq!(
            continued["data"]["result"],
            json!({"title":"Human Ada","value":"Ada"})
        );

        // A native new tab and a program-created page share the profile.
        let tab = Box::pin(execute_command(
            &json!({"action":"tab_new","url":"about:blank"}),
            &mut state,
        ))
        .await;
        assert_eq!(tab["success"], true, "{tab}");
        let second = state
            .browser
            .as_ref()
            .unwrap()
            .active_target_id()
            .unwrap()
            .to_owned();
        assert_ne!(second, target);
        let read = Box::pin(execute_command(&json!({"action":"run_playwright","targetId":second,"timeoutMs":15000,"code":"await page.goto('data:text/html,<title>Second</title>'); const extra = await context.newPage(); await extra.goto('data:text/html,<title>Third</title>'); const third = await extra.title(); await extra.close(); return {title: await page.title(), third, cookies: (await context.cookies('https://journey.example/')).map(cookie => cookie.value)};"}), &mut state)).await;
        assert_eq!(read["success"], true, "{read}");
        assert_eq!(read["data"]["result"]["title"], "Second");
        assert_eq!(read["data"]["result"]["third"], "Third");
        assert_eq!(read["data"]["result"]["cookies"], json!(["shared"]));
        let switched = Box::pin(execute_command(
            &json!({"action":"tab_switch","tabId":target}),
            &mut state,
        ))
        .await;
        assert_eq!(switched["success"], true, "{switched}");
        let title = Box::pin(execute_command(&json!({"action":"title"}), &mut state)).await;
        assert_eq!(title["data"]["title"], "Human Ada", "{title}");
        assert_eq!(browser_pid(&client).await, pid);
        assert!(Arc::ptr_eq(
            &client,
            &state.browser.as_ref().unwrap().client
        ));
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }
}
