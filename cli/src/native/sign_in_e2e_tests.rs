//! Sign-in mode through a real Chrome, its private display and the display
//! helper: the same launch without any automation channel while a person
//! signs in, native input and frames without it, the relaunch with
//! automation when the browser is handed back, and the quit that keeps the
//! person's sign-in across it.
//!
//! These tests need a Chromium executable, Xvfb and the `browser-display`
//! helper, so they are `#[ignore]`d. Run them serially against the qualified
//! workspace runtime:
//!   AGENT_BROWSER_EXECUTABLE_PATH=… AGENT_BROWSER_DISPLAY_HELPER=… \
//!   cargo test sign_in_e2e -- --ignored --test-threads=1

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::actions::{execute_command, DaemonState};
use super::e2e_tests::window_point;
use crate::test_utils::EnvGuard;

const TYPED: &str = "person@example.test";

async fn command(command: &Value, state: &mut DaemonState) -> Value {
    Box::pin(execute_command(command, state)).await
}

/// One maintenance tick: the expiry path every command also runs.
async fn maintain(state: &mut DaemonState) {
    state
        .expire_browser_control()
        .await
        .expect("maintenance succeeds");
}

fn assert_success(response: &Value) -> &Value {
    assert_eq!(response["success"], true, "expected success: {response:#}");
    &response["data"]
}

fn assert_refused(response: &Value, code: &str) -> String {
    assert_eq!(response["success"], false, "expected {code}: {response:#}");
    assert_eq!(response["code"], code, "{response:#}");
    response["error"].as_str().unwrap_or_default().to_string()
}

pub(super) fn control(op: &str, controller: &str) -> Value {
    json!({ "action": "ambit_browser_control", "op": op, "controllerId": controller })
}

fn lease_expiry() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 25_000
}

pub(super) async fn acquire(state: &mut DaemonState) -> String {
    let controller = uuid::Uuid::new_v4().to_string();
    let mut request = control("acquire", &controller);
    request["expiresAt"] = json!(lease_expiry());
    assert_eq!(
        assert_success(&command(&request, state).await)["status"],
        "controlled"
    );
    controller
}

async fn renew(state: &mut DaemonState, controller: &str) -> Value {
    let mut request = control("renew", controller);
    request["expiresAt"] = json!(lease_expiry());
    command(&request, state).await
}

pub(super) async fn sign_in(
    state: &mut DaemonState,
    controller: &str,
    sequence: u64,
    idle_timeout_ms: u64,
) -> (Value, Duration) {
    let mut request = control("input", controller);
    request["sequence"] = json!(sequence);
    request["events"] = json!([{ "type": "sign_in", "idleTimeoutMs": idle_timeout_ms }]);
    let started = Instant::now();
    let response = command(&request, state).await;
    (response, started.elapsed())
}

/// The browser process behind the owned window, as its display reports it.
pub(super) struct ChromeMain {
    pub(super) pid: u32,
    args: Vec<String>,
}

impl ChromeMain {
    pub(super) async fn observe(state: &DaemonState) -> Self {
        let display = state.window_display().expect("an owned browser window");
        let info = display.info().await.expect("the display helper answers");
        let pid = info
            .active_window()
            .and_then(|window| window.pid)
            .expect("the browser window names its process");
        // Chrome retitles its browser process: the command line reads as one
        // space-joined string. This fixture's paths contain no spaces.
        let cmdline =
            std::fs::read(format!("/proc/{pid}/cmdline")).expect("the browser process is alive");
        let args = String::from_utf8_lossy(&cmdline)
            .split(['\0', ' '])
            .filter(|arg| !arg.is_empty())
            .map(str::to_string)
            .collect();
        Self { pid, args }
    }

    /// Every switch that opens DevTools or can mark the browser automated.
    fn devtools_switches(&self) -> Vec<&str> {
        self.args
            .iter()
            .map(String::as_str)
            .filter(|arg| {
                arg.starts_with("--remote-debugging")
                    || arg.starts_with("--enable-automation")
                    || arg.starts_with("--headless")
            })
            .collect()
    }

    /// A browser without automation has one DevTools switch: the private
    /// port its owner quits it through, never the automation port 0.
    fn assert_without_automation(&self) {
        let [switch] = self.devtools_switches()[..] else {
            panic!("one private DevTools port: {:?}", self.args);
        };
        let port = switch
            .strip_prefix("--remote-debugging-port=")
            .and_then(|port| port.parse::<u16>().ok());
        assert!(port.is_some_and(|port| port != 0), "{switch}");
    }

    pub(super) fn has(&self, arg: &str) -> bool {
        self.args.iter().any(|candidate| candidate == arg)
    }

    fn user_data_dir(&self) -> &str {
        self.args
            .iter()
            .find_map(|arg| arg.strip_prefix("--user-data-dir="))
            .expect("a managed profile")
    }

    /// Every switch but DevTools and session restoration, in order. A first
    /// launch's startup URL is not a switch.
    fn managed_switches(&self) -> Vec<&str> {
        let devtools = self.devtools_switches();
        self.args[1..]
            .iter()
            .map(String::as_str)
            .filter(|arg| {
                arg.starts_with("--") && !devtools.contains(arg) && *arg != "--restore-last-session"
            })
            .collect()
    }
}

fn process_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(") ").next().unwrap_or("").starts_with('Z'))
}

/// A local site: a sign-in form that reports what the page can observe,
/// a second tab, and a persistent cookie. It records every request path.
struct Site {
    url: String,
    reports: Arc<Mutex<Vec<HashMap<String, String>>>>,
    paths: Arc<Mutex<Vec<String>>>,
    /// While set, every `/busy` page that loads keeps its renderer busy.
    busy: Arc<std::sync::atomic::AtomicBool>,
    _server: tokio::task::JoinHandle<()>,
}

const FORM: &str = r#"<!doctype html><title>Sign-in fixture</title>
<style>html,body{margin:0;height:100%}#email{position:fixed;inset:0;width:100%;height:100%;box-sizing:border-box;border:0;padding:24px;font:32px sans-serif}</style>
<form id=form><input id=email autocomplete=off aria-label="Email"></form>
<script>
const report=(kind,extra)=>fetch('/report?'+new URLSearchParams({kind,webdriver:String(navigator.webdriver),...extra}));
report('load',{cookie:document.cookie});
for(const type of ['pointermove','pointerdown','keydown','paste'])document.addEventListener(type,e=>report('event',{type}),true);
form.addEventListener('submit',event=>{event.preventDefault();report('submit',{value:email.value}).then(()=>report('signed_in',{}))});
</script>"#;

/// A page a person leaves mid-task: without automation, the first key a
/// person presses arms its `beforeunload` guard (sticky activation), signs
/// in, and opens a modal dialog that stays open.
const GUARDED: &str = r#"<!doctype html><title>Guarded fixture</title>
<style>html,body{margin:0;height:100%}#field{position:fixed;inset:0;width:100%;height:100%;box-sizing:border-box;border:0;padding:24px;font:32px sans-serif}</style>
<input id=field autocomplete=off aria-label="Field">
<script>
const report=(kind,extra)=>fetch('/report?'+new URLSearchParams({kind,webdriver:String(navigator.webdriver),...extra}));
report('load',{cookie:document.cookie,browserUi:String(outerHeight-innerHeight)});
document.addEventListener('pointermove',e=>report('event',{type:'pointermove'}),true);
if(!navigator.webdriver){
addEventListener('beforeunload',e=>{e.preventDefault();e.returnValue=''});
field.addEventListener('keydown',()=>report('guarded',{}).then(()=>{alert('Unsaved changes');report('dismissed',{})}),{once:true});
}
</script>"#;

impl Site {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let reports = Arc::new(Mutex::new(Vec::new()));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (recorded, visited, held) = (reports.clone(), paths.clone(), busy.clone());
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (recorded, visited, held) = (recorded.clone(), visited.clone(), held.clone());
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 16 * 1024];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let target = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    visited.lock().unwrap().push(path.to_string());
                    let (status, headers, body) = match path {
                        "/login" => (
                            "200 OK",
                            "Content-Type: text/html\r\nSet-Cookie: ambit_sign_in=1; Path=/; Max-Age=86400; SameSite=Lax\r\nSet-Cookie: ambit_session=1; Path=/; SameSite=Lax\r\n",
                            "<!doctype html><title>Signed in</title><p>Cookie set</p>",
                        ),
                        "/second" => (
                            "200 OK",
                            "Content-Type: text/html\r\n",
                            "<!doctype html><title>Second tab</title><p>Second tab</p>",
                        ),
                        // A heavy page: while the site holds it busy, its
                        // renderer is blocked from the moment it loads, as
                        // when a restored session reloads it.
                        "/busy" => (
                            "200 OK",
                            "Content-Type: text/html\r\n",
                            "<!doctype html><title>Busy tab</title><script>const x=new XMLHttpRequest();x.open('GET','/hold',false);x.send()</script><p>Busy tab</p>",
                        ),
                        "/hold" => {
                            while held.load(std::sync::atomic::Ordering::SeqCst) {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                            ("200 OK", "Content-Type: text/plain\r\n", "released")
                        }
                        "/report" => {
                            let mut report: HashMap<String, String> =
                                url::form_urlencoded::parse(query.as_bytes())
                                    .into_owned()
                                    .collect();
                            // A person signs in on the form or the guarded page.
                            let signs_in = report
                                .get("kind")
                                .is_some_and(|kind| kind == "submit" || kind == "guarded");
                            // Deliberate nosecret fixture cookie; only its presence is
                            // reported, never a credential or cookie value.
                            let signed_in = request.lines().any(|line| {
                                line.to_ascii_lowercase().starts_with("cookie:")
                                    && line.contains("ambit_recent_sign_in=1")
                            });
                            report.insert("recentSignIn".into(), signed_in.to_string());
                            let signed_in_session = request.lines().any(|line| {
                                line.to_ascii_lowercase().starts_with("cookie:")
                                    && line.contains("ambit_recent_session=1")
                            });
                            report.insert("recentSession".into(), signed_in_session.to_string());
                            recorded.lock().unwrap().push(report);
                            ("204 No Content", if signs_in {
                                "Set-Cookie: ambit_recent_sign_in=1; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax\r\nSet-Cookie: ambit_recent_session=1; Path=/; HttpOnly; SameSite=Lax\r\n"
                            } else { "" }, "")
                        }
                        "/" => ("200 OK", "Content-Type: text/html\r\n", FORM),
                        "/guarded" => ("200 OK", "Content-Type: text/html\r\n", GUARDED),
                        _ => ("404 Not Found", "", ""),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\n{headers}Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            url,
            reports,
            paths,
            busy,
            _server: server,
        }
    }

    fn page(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn reports(&self, kind: &str) -> Vec<HashMap<String, String>> {
        self.reports
            .lock()
            .unwrap()
            .iter()
            .filter(|report| report.get("kind").map(String::as_str) == Some(kind))
            .cloned()
            .collect()
    }

    /// The page's report of `kind` after `after` earlier ones, once it comes.
    async fn report(&self, kind: &str, after: usize) -> Option<HashMap<String, String>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(report) = self.reports(kind).into_iter().nth(after) {
                    return report;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .ok()
    }

    async fn wait_for_report(&self, kind: &str, after: usize) -> HashMap<String, String> {
        self.report(kind, after)
            .await
            .unwrap_or_else(|| panic!("the page never reported {kind} #{after}"))
    }

    /// Whether the page without automation reports native pointer movement
    /// within `within`.
    async fn sees_native_pointer(&self, within: Duration) -> bool {
        let seen = || {
            self.reports("event")
                .iter()
                .any(|event| event["type"] == "pointermove" && event["webdriver"] == "false")
        };
        tokio::time::timeout(within, async {
            while !seen() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok()
    }

    fn hold_busy_pages(&self, busy: bool) {
        self.busy.store(busy, std::sync::atomic::Ordering::SeqCst);
    }

    fn visited(&self, path: &str) -> bool {
        self.paths.lock().unwrap().iter().any(|seen| seen == path)
    }
}

/// One live-view client: what it was told, in order. Frames keep only the
/// surface generation they carry.
struct Viewer {
    seen: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Viewer {
    async fn connect(port: u64) -> Self {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .expect("the stream accepts a viewer");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let task = tokio::spawn(async move {
            while let Some(Ok(message)) = socket.next().await {
                let Ok(text) = message.to_text() else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                let kept = match value["type"].as_str() {
                    Some("frame") => {
                        json!({ "type": "frame", "generation": value["surface"]["generation"] })
                    }
                    Some("status" | "error") => value,
                    _ => continue,
                };
                record.lock().unwrap().push(kept);
            }
        });
        Self { seen, task }
    }

    fn mark(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// Waits for a frame of `generation` published after `mark`.
    async fn frame_of(&self, generation: &str, mark: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.seen.lock().unwrap()[mark..]
                    .iter()
                    .any(|seen| seen["type"] == "frame" && seen["generation"] == generation)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no frame of surface {generation} reached the viewer"));
    }

    /// Nothing since `mark` told the viewer its view failed or stopped.
    fn never_failed_since(&self, mark: usize) {
        for seen in &self.seen.lock().unwrap()[mark..] {
            assert_ne!(seen["type"], "error", "the view failed: {seen}");
            if seen["type"] == "status" {
                assert_eq!(seen["connected"], true, "the view disconnected: {seen}");
                assert_eq!(seen["screencasting"], true, "the view stopped: {seen}");
            }
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A launched owned window on the form page, streaming to one viewer.
async fn open_form(state: &mut DaemonState, site: &Site) -> Viewer {
    let stream = command(&json!({ "action": "stream_enable", "port": 0 }), state).await;
    let port = assert_success(&stream)["port"].as_u64().unwrap();
    for path in ["/login", "/second"] {
        assert_success(
            &command(
                &json!({ "action": "navigate", "url": site.page(path) }),
                state,
            )
            .await,
        );
    }
    assert_success(
        &command(
            &json!({ "action": "tab_new", "url": site.page("/") }),
            state,
        )
        .await,
    );
    site.wait_for_report("load", 0).await;
    Viewer::connect(port).await
}

/// Saves what the owned window shows now, for a failure's evidence.
async fn dump_window(state: &DaemonState, name: &str) -> std::path::PathBuf {
    use base64::Engine;
    let display = state.window_display().expect("an owned browser window");
    let request = super::display::CaptureRequest {
        cursor: true,
        budget_bytes: 4 * 1024 * 1024,
        force: true,
        patches: false,
        ..Default::default()
    };
    let (capture, _) = display
        .capture(request)
        .await
        .expect("the window can be captured")
        .frame
        .expect("a forced capture returns a frame");
    let path = std::env::temp_dir().join(format!(
        "sign-in-e2e-{name}-{}.{}",
        std::process::id(),
        capture.encoding
    ));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(capture.data.expect("a whole frame"))
        .unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A person points at `(x, y)` through the controller's input line on the
/// window `surface` until the page sees the pointer: a person moves again
/// when nothing reacts, and a Chrome that has just started can miss native
/// input for its first moments. Answers the moves it took; `sequence` is the
/// next one.
async fn point_until_seen(
    state: &mut DaemonState,
    site: &Site,
    (controller, surface): (&str, &str),
    sequence: &mut u64,
    (x, y): (f64, f64),
) -> u32 {
    let pointing = tokio::time::timeout(Duration::from_secs(10), async {
        for attempt in 0u32.. {
            let mut pointed = control("input", controller);
            pointed["sequence"] = json!(*sequence);
            pointed["expectedSurfaceGeneration"] = json!(surface);
            pointed["events"] = json!([{ "type": "input_mouse", "eventType": "mouseMoved",
                "x": x + f64::from(attempt % 2) * 8.0, "y": y, "button": "none", "buttons": 0 }]);
            assert_eq!(
                assert_success(&command(&pointed, state).await)["status"],
                "applied"
            );
            *sequence += 1;
            if site.sees_native_pointer(Duration::from_millis(250)).await {
                return attempt + 1;
            }
        }
        unreachable!()
    })
    .await;
    let Ok(attempts) = pointing else {
        let window = dump_window(state, "pointing").await;
        panic!(
            "the page never saw the person's pointer; the window: {}; page reports: {:?}",
            window.display(),
            site.reports.lock().unwrap()
        );
    };
    attempts
}

async fn evaluate(state: &mut DaemonState, script: &str) -> Value {
    let response = command(&json!({ "action": "evaluate", "script": script }), state).await;
    assert_success(&response)["result"].clone()
}

/// The person's click on the form and their typing, sent through the
/// controller's input line to the window's native devices.
fn person_types(point: (f64, f64)) -> Value {
    let (x, y) = point;
    json!([
        { "type": "input_mouse", "eventType": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0 },
        { "type": "input_mouse", "eventType": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 },
        { "type": "input_mouse", "eventType": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 },
        { "type": "input_keyboard", "eventType": "insertText", "text": TYPED },
        { "type": "input_keyboard", "eventType": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 },
        { "type": "input_keyboard", "eventType": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }
    ])
}

/// A trusted receipt from a page loaded after the automated calibration page.
/// A click also confirms that the username has focus before typing begins.
fn credential_page_receipt(
    target: &str,
    kind: &str,
    automated: &(String, u64),
) -> Option<(String, u64)> {
    if !matches!(kind, "pointer" | "pointerdown") {
        return None;
    }
    let query = target.strip_prefix(&format!("/{kind}?"))?;
    let fields: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let page = fields.get("page")?;
    let load = fields.get("load")?.parse::<u64>().ok()?;
    if page.is_empty()
        || page == &automated.0
        || load <= automated.1
        || fields.get("webdriver").map(String::as_str) != Some("false")
        || fields.get("trusted").map(String::as_str) != Some("true")
        || (kind == "pointerdown"
            && fields.get("usernameFocused").map(String::as_str) != Some("true"))
    {
        return None;
    }
    Some((page.clone(), load))
}

#[test]
fn credential_receipts_require_a_fresh_trusted_human_page() {
    let automated = ("automated".into(), 1);
    let receipt = |page, load, webdriver, trusted| {
        format!("/pointer?page={page}&load={load}&webdriver={webdriver}&trusted={trusted}")
    };
    assert_eq!(
        credential_page_receipt(
            &receipt("human", "2", "false", "true"),
            "pointer",
            &automated
        ),
        Some(("human".into(), 2))
    );
    for target in [
        receipt("automated", "2", "false", "true"),
        receipt("human", "1", "false", "true"),
        receipt("human", "0", "false", "true"),
        receipt("human", "-1", "false", "true"),
        receipt("human", "invalid", "false", "true"),
        receipt("human", "2", "true", "true"),
        receipt("human", "2", "false", "false"),
        receipt("", "2", "false", "true"),
        "/pointer".into(),
        "/pointer?page=human&load=2&trusted=true".into(),
        "/pointer?page=human&load=2&webdriver=false".into(),
    ] {
        assert_eq!(
            credential_page_receipt(&target, "pointer", &automated),
            None,
            "{target}"
        );
    }
    assert_eq!(
        credential_page_receipt(
            &receipt("human", "2", "false", "true"),
            "pointerdown",
            &automated
        ),
        None
    );
}

#[test]
fn credential_click_receipt_requires_observed_username_focus() {
    let automated = ("automated".into(), 1);
    let target = "/pointerdown?page=human&load=2&webdriver=false&trusted=true";
    for suffix in ["", "&usernameFocused=false"] {
        assert_eq!(
            credential_page_receipt(&format!("{target}{suffix}"), "pointerdown", &automated),
            None
        );
    }
    assert_eq!(
        credential_page_receipt(
            &format!("{target}&usernameFocused=true"),
            "pointerdown",
            &automated
        ),
        Some(("human".into(), 2))
    );
}

/// A synthetic password form submitted through human sign-in mode. The
/// fixture contains no real credential; its body is never read or logged.
/// Run in separate processes with role=code and role=browser_host to inspect
/// the native Save prompt and prove the fresh-profile preference boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_new_profile_credential_save_preference_preserves_human_signin() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (sent, mut seen) = tokio::sync::mpsc::unbounded_channel::<String>();
    let loads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let sent = sent.clone();
            let loads = loads.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                let size = stream.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..size]);
                let first = request.lines().next().unwrap_or("");
                let target = first.split_whitespace().nth(1).unwrap_or("/");
                let submitted = first.starts_with("POST /signed-in ");
                let body = if submitted {
                    "<!doctype html><title>Fixture signed in</title><body style='background:#f0fff0'><h1>Signed in</h1><script>requestAnimationFrame(()=>requestAnimationFrame(()=>fetch('/settled')))</script>".to_string()
                } else if target == "/" {
                    let metadata = json!({"page":uuid::Uuid::new_v4().to_string(),"load":loads.fetch_add(1,std::sync::atomic::Ordering::SeqCst)+1});
                    let form = "<!doctype html><title>Fixture sign in</title><style>body{font:20px sans-serif}input{display:block;width:400px;height:40px;margin:12px}button{margin:12px}</style><form method=post action=/signed-in><label>Username<input name=username autocomplete=username autofocus></label><label>Password<input name=password type=password autocomplete=current-password></label><button>Sign in</button></form>";
                    let script = "const report=(kind,event,extra={})=>fetch('/'+kind+'?'+new URLSearchParams({...credentialFixture,webdriver:String(navigator.webdriver),trusted:String(event.isTrusted),...extra}));addEventListener('pointermove',event=>report('pointer',event),{once:true});addEventListener('pointerdown',event=>requestAnimationFrame(()=>report('pointerdown',event,{usernameFocused:String(document.activeElement===document.querySelector('[name=username]'))})),{once:true})";
                    format!("{form}<script>window.credentialFixture={metadata};{script}</script>")
                } else {
                    String::new()
                };
                let _ = sent.send(if submitted {
                    "submitted".into()
                } else {
                    target.into()
                });
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut state = DaemonState::new();
    assert_success(&command(&json!({"action":"navigate","url":url}), &mut state).await);
    let automation = ChromeMain::observe(&state).await;
    let profile = automation.user_data_dir().to_string();
    let preferences = std::fs::read(std::path::Path::new(&profile).join("Default/Preferences"))
        .map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap())
        .unwrap_or_else(|error| {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            json!({})
        });
    let host = super::workspace_role::current().unwrap().is_browser_host();
    if host {
        assert_eq!(preferences["credentials_enable_service"], false);
    } else {
        assert_ne!(preferences["credentials_enable_service"], false);
    }
    assert!(
        preferences
            .get("signin")
            .is_none_or(|signin| signin.get("allowed").is_none()),
        "no Chrome-account sign-in policy is added"
    );
    let automated_page = evaluate(&mut state, "window.credentialFixture").await;
    assert!(automated_page["page"].is_string());
    assert!(automated_page["load"].is_u64());
    let automated_page = (
        automated_page["page"].as_str().unwrap().to_string(),
        automated_page["load"].as_u64().unwrap(),
    );
    let (x, y, _) = window_point(&state, 100.0, 70.0).await;
    while seen.try_recv().is_ok() {}
    let controller = acquire(&mut state).await;
    assert_success(&sign_in(&mut state, &controller, 1, 600_000).await.0);
    let person = ChromeMain::observe(&state).await;
    person.assert_without_automation();
    assert_eq!(person.user_data_dir(), profile);
    // Accept only a fresh human page's trusted pointer, never a delayed
    // report from the automated page used for coordinate calibration.
    let mut sequence = 2;
    let human_page = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let mut input = control("input", &controller);
            input["sequence"] = json!(sequence);
            input["expectedSurfaceGeneration"] = json!(state.window_display().unwrap().surface().generation);
            input["events"] = json!([{"type":"input_mouse","eventType":"mouseMoved","x":x + f64::from((sequence % 2) as u32)*4.0,"y":y,"button":"none","buttons":0}]);
            assert_eq!(assert_success(&command(&input,&mut state).await)["status"], "applied");
            sequence += 1;
            if let Ok(Some(path)) = tokio::time::timeout(Duration::from_millis(250),seen.recv()).await {
                if let Some(page) = credential_page_receipt(&path, "pointer", &automated_page) {
                    break page;
                }
            }
        }
    }).await;
    if human_page.is_err() {
        eprintln!(
            "credential_failure_window={}",
            dump_window(&state, "credential-fresh-page-failed")
                .await
                .display()
        );
    }
    let human_page = human_page.expect("fresh human sign-in page accepted a real pointer");
    let mut input = control("input", &controller);
    input["sequence"] = json!(sequence);
    input["expectedSurfaceGeneration"] =
        json!(state.window_display().unwrap().surface().generation);
    let click = vec![
        json!({"type":"input_mouse","eventType":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),
        json!({"type":"input_mouse","eventType":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),
    ];
    input["events"] = json!(click);
    assert_eq!(
        assert_success(&command(&input, &mut state).await)["status"],
        "applied"
    );
    sequence += 1;
    let clicked = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(path) = seen.recv().await {
            if credential_page_receipt(&path, "pointerdown", &automated_page).as_ref()
                == Some(&human_page)
            {
                return;
            }
        }
        panic!("fixture server stopped");
    })
    .await;
    if clicked.is_err() {
        eprintln!(
            "credential_failure_window={}",
            dump_window(&state, "credential-fresh-focus-failed")
                .await
                .display()
        );
    }
    clicked.expect("fresh human page received the trusted click and focused username");
    let focused = state.window_display().unwrap().info().await.unwrap();
    assert!(
        focused
            .windows
            .iter()
            .any(|window| window.mapped && window.focused && window.pid == Some(person.pid)),
        "the owned human Chrome window has native focus before keys"
    );
    input["sequence"] = json!(sequence);
    let mut events = Vec::new();
    events.extend(super::interaction::native_inserted_events("fixture"));
    events.extend(super::interaction::native_key_chord_events("Tab", None));
    // Deliberate nosecret test fixture only.
    events.extend(super::interaction::native_inserted_events("nosecret"));
    events.extend(super::interaction::native_key_chord_events("Enter", None));
    input["events"] = json!(events);
    assert_eq!(
        assert_success(&command(&input, &mut state).await)["status"],
        "applied"
    );
    let submitted = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(path) = seen.recv().await {
            if path == "/settled" {
                return;
            }
        }
        panic!("fixture server stopped");
    })
    .await;
    if submitted.is_err() {
        let path = dump_window(&state, "credential-submit-failed").await;
        eprintln!("credential_failure_window={}", path.display());
    }
    submitted.expect("human sign-in submitted and rendered success");
    let display = state.window_display().unwrap();
    let screenshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let captured = display
                .capture(super::display::CaptureRequest {
                    patches: false,
                    cursor: true,
                    wait_ms: 250,
                    budget_bytes: 4 * 1024 * 1024,
                    ..Default::default()
                })
                .await
                .unwrap();
            let Some((capture, _)) = captured.frame else {
                continue;
            };
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(capture.data.unwrap())
                .unwrap();
            let image = image::load_from_memory(&bytes).unwrap().to_rgb8();
            let pixel = image.get_pixel(image.width() / 2, image.height() * 3 / 4).0;
            if pixel[1] > 250 && (234..=245).contains(&pixel[0]) && (234..=245).contains(&pixel[2])
            {
                return bytes;
            }
        }
    })
    .await
    .expect("native pixels show the rendered success page");
    if let Some(directory) = std::env::var_os("AMBIT_HOST_SAVE_EVIDENCE") {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            std::path::PathBuf::from(directory).join(if host { "host.jpeg" } else { "code.jpeg" }),
            screenshot,
        )
        .unwrap();
    }
    if !host {
        // Fixture's fixed 1280px Chrome toolbar: the baseline shows its key
        // icon; open the actual Save prompt without saving the test value.
        let surface = display.surface();
        let (key_x, key_y) = (f64::from(surface.width) - 278.0, 125.0);
        let mut clicked = control("input", &controller);
        clicked["sequence"] = json!(sequence + 1);
        clicked["expectedSurfaceGeneration"] = json!(surface.generation);
        clicked["events"] = json!([{"type":"input_mouse","eventType":"mouseMoved","x":key_x,"y":key_y,"button":"none","buttons":0},{"type":"input_mouse","eventType":"mousePressed","x":key_x,"y":key_y,"button":"left","buttons":1,"clickCount":1},{"type":"input_mouse","eventType":"mouseReleased","x":key_x,"y":key_y,"button":"left","buttons":0,"clickCount":1}]);
        assert_success(&command(&clicked, &mut state).await);
        let captured = display
            .capture(super::display::CaptureRequest {
                patches: false,
                cursor: true,
                wait_ms: 250,
                budget_bytes: 4 * 1024 * 1024,
                ..Default::default()
            })
            .await
            .unwrap();
        if let (Some((capture, _)), Some(directory)) =
            (captured.frame, std::env::var_os("AMBIT_HOST_SAVE_EVIDENCE"))
        {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(capture.data.unwrap())
                .unwrap();
            std::fs::write(
                std::path::PathBuf::from(directory).join("code-save-prompt.jpeg"),
                bytes,
            )
            .unwrap();
        }
    }
    assert_success(&command(&control("release", &controller), &mut state).await);
    if host {
        let persisted: Value = serde_json::from_slice(
            &std::fs::read(std::path::Path::new(&profile).join("Default/Preferences")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            persisted["credentials_enable_service"], false,
            "human hand-back keeps the profile preference"
        );
    }
    assert_success(&command(&json!({"action":"navigate","url":url}), &mut state).await);
    assert_success(&command(&json!({"action":"close"}), &mut state).await);
    // A caller-selected fixture profile is never rewritten by host defaults.
    let selected = tempfile::tempdir().unwrap();
    std::fs::create_dir(selected.path().join("Default")).unwrap();
    let selected_preferences = selected.path().join("Default/Preferences");
    std::fs::write(
        &selected_preferences,
        b"{\"credentials_enable_service\":true}",
    )
    .unwrap();
    assert_success(
        &command(
            &json!({"action":"launch","headless":true,"profile":selected.path().to_string_lossy()}),
            &mut state,
        )
        .await,
    );
    let preferences: Value =
        serde_json::from_slice(&std::fs::read(&selected_preferences).unwrap()).unwrap();
    assert_eq!(
        preferences["credentials_enable_service"], true,
        "caller-selected profile remains unchanged"
    );
    assert_success(&command(&json!({"action":"close"}), &mut state).await);
    server.abort();
}

/// Automation → sign-in mode → automation on one profile, as the Product's
/// dock drives it through the control protocol.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_sign_in_relaunches_without_automation_and_hands_back() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = Site::start().await;
    let mut state = DaemonState::new();
    let viewer = open_form(&mut state, &site).await;

    // Under automation the page truthfully sees automation.
    let automation = ChromeMain::observe(&state).await;
    assert_eq!(
        automation.devtools_switches(),
        ["--remote-debugging-port=0"]
    );
    assert_eq!(evaluate(&mut state, "navigator.webdriver").await, true);
    assert_eq!(site.reports("load")[0]["webdriver"], "true");
    // Calibrated with DevTools, used without it: the window keeps its
    // geometry and the field fills the page.
    let (x, y, _) = window_point(&state, 320.0, 240.0).await;

    let controller = acquire(&mut state).await;
    let before = viewer.mark();
    let (entered, enter_time) = sign_in(&mut state, &controller, 1, 600_000).await;
    let entered_at = Instant::now();
    let entered = assert_success(&entered).clone();
    assert_eq!(entered["status"], "applied");
    assert_eq!(entered["lastSequence"], 1);
    let signing_in = entered["surface"]["generation"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(enter_time < Duration::from_secs(8), "{enter_time:?}");

    // The same browser, profile and flags, without the automation channel.
    let person = ChromeMain::observe(&state).await;
    assert_ne!(person.pid, automation.pid);
    assert!(
        !process_alive(automation.pid),
        "the automation browser still runs"
    );
    person.assert_without_automation();
    assert!(person.has("--restore-last-session"));
    assert_eq!(person.args[0], automation.args[0]);
    assert_eq!(person.user_data_dir(), automation.user_data_dir());
    assert_eq!(person.managed_switches(), automation.managed_switches());
    assert!(state.browser.is_none());
    viewer.frame_of(&signing_in, before).await;

    // The restored tabs reloaded without automation, and the page sees it.
    let load = site.wait_for_report("load", 1).await;
    assert_eq!(load["webdriver"], "false");
    assert!(load["cookie"].contains("ambit_sign_in=1"), "{load:?}");
    assert!(load["cookie"].contains("ambit_session=1"), "{load:?}");

    // Every agent command is refused for the sign-in, before any effect.
    for agent in [
        json!({ "action": "snapshot" }),
        json!({ "action": "evaluate", "script": "1" }),
        json!({ "action": "navigate", "url": site.page("/never") }),
        json!({ "action": "tab_list" }),
        json!({ "action": "launch" }),
        json!({ "action": "close" }),
    ] {
        let message = assert_refused(
            &command(&agent, &mut state).await,
            "browser_controlled_by_user",
        );
        assert!(message.contains("signing in"), "{message}");
    }
    assert_eq!(ChromeMain::observe(&state).await.pid, person.pid);
    assert!(state.browser.is_none());
    assert!(!site.visited("/never"));
    let inspected = assert_success(
        &command(
            &json!({ "action": "ambit_browser_control", "op": "inspect" }),
            &mut state,
        )
        .await,
    )
    .clone();
    assert_eq!(inspected["supported"], true);
    assert_eq!(inspected["controlled"], true);
    assert!(inspected.get("filesSupported").is_none(), "{inspected}");
    // Another controller cannot take the browser, and navigation is Chrome's.
    let mut competing = control("acquire", &uuid::Uuid::new_v4().to_string());
    competing["expiresAt"] = json!(lease_expiry());
    assert_refused(
        &command(&competing, &mut state).await,
        "browser_control_conflict",
    );
    let mut navigate = control("input", &controller);
    navigate["sequence"] = json!(2);
    navigate["expectedSurfaceGeneration"] = json!(signing_in);
    navigate["events"] = json!([{ "type": "navigation", "action": "reload" }]);
    let message = assert_refused(
        &command(&navigate, &mut state).await,
        "browser_control_invalid",
    );
    assert!(message.contains("address bar"), "{message}");
    assert_success(&renew(&mut state, &controller).await);

    // The person's own input reaches the page through the display alone.
    let mut sequence = 2;
    let pointing_attempts = point_until_seen(
        &mut state,
        &site,
        (&controller, &signing_in),
        &mut sequence,
        (x, y),
    )
    .await;
    let native_input_ready = entered_at.elapsed();
    let mut typed = control("input", &controller);
    typed["sequence"] = json!(sequence);
    typed["expectedSurfaceGeneration"] = json!(signing_in);
    typed["events"] = person_types((x, y));
    assert_eq!(
        assert_success(&command(&typed, &mut state).await)["status"],
        "applied"
    );
    let Some(submitted) = site.report("submit", 0).await else {
        let window = dump_window(&state, "typed").await;
        panic!(
            "the typed form was not submitted; the window: {}; page reports: {:?}",
            window.display(),
            site.reports.lock().unwrap()
        );
    };
    assert_eq!(submitted["value"], TYPED);
    assert_eq!(submitted["webdriver"], "false");
    let signed_in = site.wait_for_report("signed_in", 0).await;
    assert_eq!(signed_in["recentSignIn"], "true", "{signed_in:?}");
    assert_eq!(signed_in["recentSession"], "true", "{signed_in:?}");
    viewer.never_failed_since(before);

    // Hand back: automation again, on the same profile, tabs and cookies.
    let before = viewer.mark();
    let started = Instant::now();
    let released = command(&control("release", &controller), &mut state).await;
    let hand_back_time = started.elapsed();
    let released = assert_success(&released).clone();
    assert_eq!(released["status"], "released");
    assert!(
        hand_back_time < Duration::from_secs(8),
        "{hand_back_time:?}"
    );
    let handed_back = released["surface"]["generation"]
        .as_str()
        .unwrap()
        .to_string();
    let again = ChromeMain::observe(&state).await;
    assert!(!process_alive(person.pid), "the sign-in browser still runs");
    assert_eq!(again.devtools_switches(), ["--remote-debugging-port=0"]);
    assert!(again.has("--restore-last-session"));
    assert_eq!(again.user_data_dir(), automation.user_data_dir());
    assert_eq!(again.managed_switches(), automation.managed_switches());
    viewer.frame_of(&handed_back, before).await;
    viewer.never_failed_since(before);
    assert_eq!(
        site.wait_for_report("load", 2).await["webdriver"],
        "true",
        "the restored page sees automation again"
    );

    // A command that names its target runs at once; key input, which goes
    // to whatever has focus, waits for an observation. Then the agent finds
    // its session intact.
    assert_eq!(evaluate(&mut state, "1").await, 1);
    assert_refused(
        &command(&json!({ "action": "press", "key": "Enter" }), &mut state).await,
        "browser_observation_required",
    );
    let started = Instant::now();
    assert_success(&command(&json!({ "action": "snapshot" }), &mut state).await);
    let first_observation_time = started.elapsed();
    assert_eq!(evaluate(&mut state, "navigator.webdriver").await, true);
    assert_eq!(evaluate(&mut state, "location.pathname").await, "/");
    assert!(evaluate(&mut state, "document.cookie")
        .await
        .as_str()
        .unwrap()
        .contains("ambit_sign_in=1"));
    // The restored DOM can show the signed-in page even when its cookies
    // were lost. A real navigation must send them to the fixture, which a
    // cached page cannot prove: the persistent and session cookies from
    // before the sign-in, and the persistent and session HttpOnly cookies
    // the sign-in set, which only a quit that ends the session keeps.
    let loads = site.reports("load").len();
    assert_success(
        &command(
            &json!({ "action": "navigate", "url": site.page("/") }),
            &mut state,
        )
        .await,
    );
    let restored = site.wait_for_report("load", loads).await;
    assert!(
        restored["cookie"].contains("ambit_sign_in=1")
            && restored["cookie"].contains("ambit_session=1"),
        "{restored:?}"
    );
    assert_eq!(restored["recentSignIn"], "true", "{restored:?}");
    assert_eq!(restored["recentSession"], "true", "{restored:?}");
    let tabs = command(&json!({ "action": "tab_list" }), &mut state).await;
    let urls: Vec<String> = assert_success(&tabs)["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tab| tab["url"].as_str().map(str::to_string))
        .collect();
    assert!(urls.contains(&site.page("/second")), "{urls:?}");
    assert!(urls.contains(&site.page("/")), "{urls:?}");
    assert_eq!(
        assert_success(&command(&control("release", &controller), &mut state).await)["status"],
        "released"
    );
    assert_refused(
        &renew(&mut state, &controller).await,
        "browser_control_stale",
    );

    println!(
        "SIGN_IN_TRANSITIONS {}",
        json!({
            "enterMs": enter_time.as_millis(),
            "nativeInputReadyMs": native_input_ready.as_millis(),
            "pointingAttempts": pointing_attempts,
            "handBackMs": hand_back_time.as_millis(),
            "firstObservationMs": first_observation_time.as_millis(),
        })
    );
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// A person leaves the page mid-task: it guards its unload with
/// `beforeunload` and shows a modal dialog. The hand-back still quits the
/// sign-in browser on its own, neither held by the guard nor by the dialog,
/// and the session cookie the sign-in set survives into automation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_hand_back_quits_past_an_open_dialog_and_a_beforeunload_guard() {
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_WINDOW_STREAM",
        "DISPLAY",
        "AGENT_BROWSER_SOCKET_DIR",
    ]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let logs = tempfile::tempdir().unwrap();
    env.set("AGENT_BROWSER_SOCKET_DIR", logs.path().to_str().unwrap());
    let site = Site::start().await;
    let mut state = DaemonState::new();
    for path in ["/login", "/guarded"] {
        assert_success(
            &command(
                &json!({ "action": "navigate", "url": site.page(path) }),
                &mut state,
            )
            .await,
        );
    }
    let automated = site.wait_for_report("load", 0).await;
    assert_eq!(automated["webdriver"], "true");
    let (x, y, _) = window_point(&state, 320.0, 240.0).await;

    let controller = acquire(&mut state).await;
    let (entered, _) = sign_in(&mut state, &controller, 1, 600_000).await;
    let surface = assert_success(&entered)["surface"]["generation"]
        .as_str()
        .unwrap()
        .to_string();
    let person = ChromeMain::observe(&state).await;
    person.assert_without_automation();
    // The private port leaves no automation state and no bar: the browser
    // shows the same toolbars above the page as the automation browser,
    // whose launch carries no flag Chrome warns about (an unsupported flag,
    // such as hiding automation from Blink, adds a bar and moves the page).
    let signing_in = site.wait_for_report("load", 1).await;
    assert_eq!(signing_in["webdriver"], "false");
    assert_eq!(
        signing_in["browserUi"], automated["browserUi"],
        "{signing_in:?} against {automated:?}"
    );

    // The person's click and key arm the guard, sign in and open the dialog.
    let mut sequence = 2;
    point_until_seen(
        &mut state,
        &site,
        (&controller, &surface),
        &mut sequence,
        (x, y),
    )
    .await;
    let mut pressed = control("input", &controller);
    pressed["sequence"] = json!(sequence);
    pressed["expectedSurfaceGeneration"] = json!(surface);
    pressed["events"] = json!([
        { "type": "input_mouse", "eventType": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 },
        { "type": "input_mouse", "eventType": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 },
        { "type": "input_keyboard", "eventType": "keyDown", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65 },
        { "type": "input_keyboard", "eventType": "keyUp", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65 }
    ]);
    assert_eq!(
        assert_success(&command(&pressed, &mut state).await)["status"],
        "applied"
    );
    let Some(guarded) = site.report("guarded", 0).await else {
        let window = dump_window(&state, "guarded").await;
        panic!(
            "the person's key never reached the page; the window: {}; page reports: {:?}",
            window.display(),
            site.reports.lock().unwrap()
        );
    };
    assert_eq!(guarded["webdriver"], "false");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(site.reports("dismissed").is_empty(), "the dialog is open");

    let loads = site.reports("load").len();
    let started = Instant::now();
    let released = command(&control("release", &controller), &mut state).await;
    let hand_back_time = started.elapsed();
    assert_eq!(assert_success(&released)["status"], "released");
    assert!(
        hand_back_time < Duration::from_secs(8),
        "{hand_back_time:?}"
    );
    assert!(!process_alive(person.pid), "the sign-in browser still runs");
    assert_eq!(
        ChromeMain::observe(&state).await.devtools_switches(),
        ["--remote-debugging-port=0"]
    );
    let session_log =
        std::fs::read_to_string(logs.path().join(format!("{}.log", state.session_id)))
            .unwrap_or_default();
    assert!(
        !session_log.contains("did not quit in time"),
        "{session_log}"
    );

    // The restored page's own request carries every cookie, including the
    // session cookie set seconds before the hand-back.
    let restored = site.wait_for_report("load", loads).await;
    assert_eq!(restored["webdriver"], "true");
    assert!(
        restored["cookie"].contains("ambit_session=1"),
        "{restored:?}"
    );
    assert_eq!(restored["recentSignIn"], "true", "{restored:?}");
    assert_eq!(restored["recentSession"], "true", "{restored:?}");
    println!(
        "HAND_BACK_GUARDED {}",
        json!({ "handBackMs": hand_back_time.as_millis() })
    );
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// Inside a size class a cropping presenter leaves the framebuffer larger
/// than the window. Sign-in relaunches Chrome at the window's size, and so
/// does the hand-back, with no presenter connected to correct either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_sign_in_and_hand_back_keep_the_window_size_inside_a_size_class() {
    use super::stream::layout;
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = Site::start().await;
    let mut state = DaemonState::new();
    assert_success(
        &command(
            &json!({ "action": "navigate", "url": site.page("/") }),
            &mut state,
        )
        .await,
    );
    let display = state.window_display().expect("an owned browser window");
    let features = display.info().await.unwrap().features;
    if !features.iter().any(|feature| feature == "sizeClass") {
        eprintln!("SIZE_CLASS_UNAVAILABLE: this helper keeps the framebuffer at the window");
        assert_success(&command(&json!({ "action": "close" }), &mut state).await);
        return;
    }
    let framebuffer = |display: &super::display::DisplayClient| {
        let surface = display.surface();
        (surface.width, surface.height)
    };
    // What a cropping presenter's layout leaves while no viewer is connected.
    layout::apply(&display, 780, 600, true).await.unwrap();
    assert_eq!(display.window(), (1560, 1200));
    assert_ne!(framebuffer(&display), (1560, 1200), "a larger size class");

    let controller = acquire(&mut state).await;
    assert_success(&sign_in(&mut state, &controller, 1, 600_000).await.0);
    let signing_in = state.window_display().expect("the sign-in window");
    assert_eq!(
        signing_in.window(),
        (1560, 1200),
        "sign-in keeps the window"
    );
    // The dock narrows during sign-in, in a size class, and then closes.
    layout::apply(&signing_in, 700, 500, true).await.unwrap();
    assert_eq!(signing_in.window(), (1400, 1000));
    assert_ne!(
        framebuffer(&signing_in),
        (1400, 1000),
        "a larger size class"
    );

    assert_eq!(
        assert_success(&command(&control("release", &controller), &mut state).await)["status"],
        "released"
    );
    let automation = state.window_display().expect("the automation window");
    assert_eq!(
        automation.window(),
        (1400, 1000),
        "hand-back keeps the window"
    );
    assert_success(&command(&json!({ "action": "snapshot" }), &mut state).await);
    assert_eq!(evaluate(&mut state, "innerWidth").await, 700);
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// The watchdog ends a sign-in nobody is using, and a person closing the
/// browser ends it without relaunching automation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_sign_in_ends_when_idle_and_when_the_person_closes_the_browser() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = Site::start().await;
    let mut state = DaemonState::new();
    let viewer = open_form(&mut state, &site).await;
    let automation = ChromeMain::observe(&state).await;

    // Renewal keeps the lease but is not presence: the idle bound ends it.
    let controller = acquire(&mut state).await;
    assert_success(&sign_in(&mut state, &controller, 1, 10_000).await.0);
    let person = ChromeMain::observe(&state).await;
    let idle_from = Instant::now();
    while idle_from.elapsed() < Duration::from_millis(10_300) {
        tokio::time::sleep(Duration::from_millis(2_000)).await;
        let renewed = renew(&mut state, &controller).await;
        if renewed["success"] != true {
            assert_refused(&renewed, "browser_control_stale");
            break;
        }
    }
    // The maintenance tick's own expiry path hands the browser back.
    maintain(&mut state).await;
    let again = ChromeMain::observe(&state).await;
    assert!(!process_alive(person.pid));
    assert_eq!(again.devtools_switches(), ["--remote-debugging-port=0"]);
    assert_eq!(again.user_data_dir(), automation.user_data_dir());
    assert_refused(
        &renew(&mut state, &controller).await,
        "browser_control_stale",
    );
    assert_eq!(
        assert_success(&command(&control("release", &controller), &mut state).await)["status"],
        "released"
    );
    assert_success(&command(&json!({ "action": "snapshot" }), &mut state).await);
    assert_eq!(evaluate(&mut state, "navigator.webdriver").await, true);

    // The person closes the sign-in browser: sign-in ends, nothing relaunches.
    let controller = acquire(&mut state).await;
    assert_success(&sign_in(&mut state, &controller, 1, 600_000).await.0);
    let person = ChromeMain::observe(&state).await;
    let before = viewer.mark();
    // SAFETY: signalling the sign-in browser this test's daemon launched.
    unsafe {
        libc::kill(person.pid as i32, libc::SIGTERM);
    }
    // The maintenance tick notices the exit on a later tick once the
    // process can be reaped; nothing relaunches.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            maintain(&mut state).await;
            if state.window_display().is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the closed sign-in browser was never noticed"
    );
    assert!(!process_alive(person.pid));
    assert!(state.browser.is_none());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        viewer.seen.lock().unwrap()[before..]
            .iter()
            .any(|seen| seen["type"] == "status" && seen["connected"] == false),
        "the view learns the browser is gone"
    );
    assert_refused(
        &renew(&mut state, &controller).await,
        "browser_control_stale",
    );
    assert_eq!(
        assert_success(&command(&control("release", &controller), &mut state).await)["status"],
        "released"
    );

    // The agent's continuation observes first, which launches the browser
    // normally into the retained profile.
    assert_success(&command(&json!({ "action": "snapshot" }), &mut state).await);
    assert_success(
        &command(
            &json!({ "action": "navigate", "url": site.page("/") }),
            &mut state,
        )
        .await,
    );
    let relaunched = ChromeMain::observe(&state).await;
    assert_eq!(relaunched.user_data_dir(), automation.user_data_dir());
    assert!(evaluate(&mut state, "document.cookie")
        .await
        .as_str()
        .unwrap()
        .contains("ambit_sign_in=1"));
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// A sign-in window that never appears returns the browser to automation
/// and ends the lease inside the relay's deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_sign_in_that_cannot_start_returns_the_browser_to_automation() {
    use std::os::unix::fs::PermissionsExt;
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_WINDOW_STREAM",
        "DISPLAY",
        "AGENT_BROWSER_DISPLAY_HELPER",
        "AMBIT_TEST_DISPLAY_HELPER",
        "AMBIT_TEST_DISPLAY_HELPER_FAIL_ONCE",
    ]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    // The display helper refuses exactly once when asked to: for the
    // sign-in window, never for automation before or after it.
    let helper = std::env::var("AGENT_BROWSER_DISPLAY_HELPER")
        .expect("the qualified display helper for this test");
    let scratch = tempfile::tempdir().unwrap();
    let wrapper = scratch.path().join("browser-display");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nif rm \"$AMBIT_TEST_DISPLAY_HELPER_FAIL_ONCE\" 2>/dev/null; then exit 1; fi\nexec \"$AMBIT_TEST_DISPLAY_HELPER\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fail_once = scratch.path().join("fail-once");
    env.set("AMBIT_TEST_DISPLAY_HELPER", &helper);
    env.set(
        "AMBIT_TEST_DISPLAY_HELPER_FAIL_ONCE",
        fail_once.to_str().unwrap(),
    );
    env.set("AGENT_BROWSER_DISPLAY_HELPER", wrapper.to_str().unwrap());

    let site = Site::start().await;
    let mut state = DaemonState::new();
    let viewer = open_form(&mut state, &site).await;
    let automation = ChromeMain::observe(&state).await;
    let controller = acquire(&mut state).await;
    std::fs::write(&fail_once, b"").unwrap();
    let before = viewer.mark();
    let (refused, elapsed) = sign_in(&mut state, &controller, 1, 600_000).await;
    let message = assert_refused(&refused, "browser_control_unavailable");
    assert!(
        message.contains("back under the agent's control"),
        "{message}"
    );
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
    assert!(
        !fail_once.exists(),
        "the sign-in window's helper was refused"
    );

    // Automation is back on the same profile and the lease has ended.
    let again = ChromeMain::observe(&state).await;
    assert_eq!(again.devtools_switches(), ["--remote-debugging-port=0"]);
    assert_eq!(again.user_data_dir(), automation.user_data_dir());
    viewer.never_failed_since(before);
    assert_refused(
        &renew(&mut state, &controller).await,
        "browser_control_stale",
    );
    assert_eq!(
        assert_success(&command(&control("release", &controller), &mut state).await)["status"],
        "released"
    );
    assert_eq!(evaluate(&mut state, "location.pathname").await, "/");
    assert_refused(
        &command(
            &json!({ "action": "keyboard", "subaction": "type", "text": "x" }),
            &mut state,
        )
        .await,
        "browser_observation_required",
    );
    assert_success(&command(&json!({ "action": "snapshot" }), &mut state).await);
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// Production, run 7bb8369f: after a sign-in hand-back every command failed
/// with the bare `browser_active_page_ambiguous` until the browser was
/// closed, because a launch that could not tell which page its window shows
/// failed as a whole. A relaunched window has no focus, so telling needs
/// every restored tab to answer, and a background tab whose renderer is
/// still busy loading does not; it also held every window layout, which
/// waited on each page in turn. Automation now comes back with the person's
/// tabs, and the next command acts, or is refused alone with the open tabs
/// listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_hand_back_with_a_busy_restored_tab_keeps_the_browser_and_its_tabs() {
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_WINDOW_STREAM",
        "DISPLAY",
        "AGENT_BROWSER_SOCKET_DIR",
    ]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let logs = tempfile::tempdir().unwrap();
    env.set("AGENT_BROWSER_SOCKET_DIR", logs.path().to_str().unwrap());
    let site = Site::start().await;
    let mut state = DaemonState::new();
    let _viewer = open_form(&mut state, &site).await;
    // A heavy page in a background tab; the person stays on the form.
    assert_success(
        &command(
            &json!({ "action": "tab_new", "url": site.page("/busy") }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &command(
            &json!({ "action": "tab_switch", "tabId": "t2" }),
            &mut state,
        )
        .await,
    );

    let controller = acquire(&mut state).await;
    let (entered, _) = sign_in(&mut state, &controller, 1, 600_000).await;
    assert_success(&entered);
    // Hand back while the restored heavy page loads again.
    site.hold_busy_pages(true);
    let loads = site.reports("load").len();
    let started = Instant::now();
    let released = command(&control("release", &controller), &mut state).await;
    let hand_back_time = started.elapsed();
    let session_log =
        std::fs::read_to_string(logs.path().join(format!("{}.log", state.session_id)))
            .unwrap_or_default();
    println!(
        "HAND_BACK_BUSY {}",
        json!({ "handBackMs": hand_back_time.as_millis(), "sessionLog": session_log })
    );
    assert_eq!(assert_success(&released)["status"], "released");
    assert!(
        hand_back_time < Duration::from_secs(8),
        "{hand_back_time:?}"
    );
    assert!(
        state.browser.is_some(),
        "automation did not come back: {session_log}"
    );
    assert!(
        !session_log.contains("did not quit in time"),
        "{session_log}"
    );
    // The restored form's own request carries the cookies the site set
    // before the sign-in, its session cookie included: both relaunches kept
    // them while a tab was busy. The sign-in browser's own restored pages
    // may still report their loads after the release began.
    let mut seen = loads;
    let restored = loop {
        let report = site.wait_for_report("load", seen).await;
        if report["webdriver"] == "true" {
            break report;
        }
        seen += 1;
    };
    assert!(
        restored["cookie"].contains("ambit_session=1")
            && restored["cookie"].contains("ambit_sign_in=1"),
        "{restored:?}"
    );

    let url = command(&json!({ "action": "url" }), &mut state).await;
    if url["success"] != true {
        assert_eq!(url["code"], "browser_active_page_ambiguous", "{url:#}");
        assert!(url["data"]["tabs"].is_array(), "{url:#}");
    }
    // Every tab is restored; a busy one commits its address when it can.
    let restored: Vec<String> = ["/second", "/", "/busy"]
        .iter()
        .map(|path| site.page(path))
        .collect();
    let mut urls = Vec::new();
    for _ in 0..25 {
        let tabs = command(&json!({ "action": "tab_list" }), &mut state).await;
        urls = assert_success(&tabs)["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tab| tab["url"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        if restored.iter().all(|url| urls.contains(url)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(restored.iter().all(|url| urls.contains(url)), "{urls:?}");
    // Once the heavy page settles, the browser is observed as usual.
    site.hold_busy_pages(false);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_success(&command(&json!({ "action": "snapshot" }), &mut state).await);
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}
