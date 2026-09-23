//! A bounded Node operation against the browser already owned by this daemon.
//! The operation slot is cancellation, not a second control lease. Command
//! custody remains with the daemon and human input remains with BrowserControl.

mod transport;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
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
pub(crate) const ENVIRONMENT_FIELD: &str = "ambitProgram";

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

    fn stopped(self) -> String {
        if self == Self::HumanControl {
            "browser_operation_interrupted: The Playwright program stopped for user control. Earlier effects may have executed; inspect a fresh observation after handback and do not replay the program.".into()
        } else {
            unknown(self.message())
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
            _ = operation.canceled.changed() => return Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).before_start()),
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
        let control = state.browser_control.clone();
        let client = tokio::select! {
            biased;
            _ = operation.canceled.changed() => return Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).before_start()),
            result = tokio::time::timeout_at(deadline, CdpClient::connect(&endpoint)) =>
                Arc::new(result.map_err(|_| "browser_operation_rejected: Browser attachment timed out.")?
                    .map_err(|_| "browser_operation_rejected: The existing browser could not be attached.")?),
        };
        let observed = tokio::select! {
            biased;
            _ = operation.canceled.changed() => Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).before_start()),
            result = tokio::time::timeout_at(deadline, browser.observe_owned_downloads(&client, download_context.as_deref())) => result.map_err(|_| "browser_operation_rejected: Browser attachment setup reached its deadline.".to_string()).and_then(|result| result),
        };
        if let Err(error) = observed {
            client.disconnect();
            return Err(error);
        }
        let mut tunnel = transport::Tunnel::start(client, control.clone()).await?;
        let request = json!({ "endpoint": tunnel.endpoint(), "targetId": target, "code": code, "timeoutMs": timeout, "artifactsDir": artifacts, "environment": environment, "isolatedContexts": isolated_contexts });
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
        let execution = async {
            stdin
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .map_err(|_| unknown("The runner did not accept its invocation."))?;
            drop(stdin);
            result_bytes(result_reader).await
        };
        let result = tokio::select! {
            biased;
            _ = operation.canceled.changed() => Err(operation.canceled.borrow().unwrap_or(InterruptReason::Shutdown).stopped()),
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

    #[tokio::test]
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
        let moved = Box::pin(execute_command(&json!({"action":"run_playwright","code":"await page.mouse.move(150, 90); return {pointer: await page.evaluate(() => pointers.at(-1)), width: await page.evaluate(() => innerWidth)};","timeoutMs":15000}), &mut state)).await;
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

    #[tokio::test]
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
        let created = Box::pin(execute_command(&json!({"action":"window_new"}), &mut state)).await;
        assert_eq!(created["success"], true, "{created}");
        assert_eq!(created["data"]["isolated"], false);
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
        let isolated = Box::pin(execute_command(
            &json!({"action":"window_new","isolated":true}),
            &mut state,
        ))
        .await;
        assert_eq!(isolated["success"], true, "{isolated}");
        assert_eq!(isolated["data"]["isolated"], true);
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
        client
            .send_command(
                "Target.disposeBrowserContext",
                Some(json!({"browserContextId":isolated_context})),
                None,
            )
            .await
            .unwrap();
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
        Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    }

    /// One Chrome process, profile and tab across native commands, Playwright
    /// programs, a human click and new tabs, in the native window mode that
    /// production uses.
    #[tokio::test]
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
        let click = |event: &str| json!({"type":"input_mouse","eventType":event,"x":120,"y":144,"button":"left","clickCount":1});
        let input = Box::pin(execute_command(&json!({"action":crate::native::browser_control::ACTION,"op":"input","controllerId":controller,"sequence":1,"events":[click("mousePressed"), click("mouseReleased")]}), &mut state)).await;
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
