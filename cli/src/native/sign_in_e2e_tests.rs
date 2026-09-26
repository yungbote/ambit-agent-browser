//! Sign-in mode through a real Chrome, its private display and the display
//! helper: the same launch without any automation channel while a person
//! signs in, native input and frames without DevTools, and the relaunch
//! with DevTools when the browser is handed back.
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

    fn automation_switches(&self) -> Vec<&str> {
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

    pub(super) fn has(&self, arg: &str) -> bool {
        self.args.iter().any(|candidate| candidate == arg)
    }

    fn user_data_dir(&self) -> &str {
        self.args
            .iter()
            .find_map(|arg| arg.strip_prefix("--user-data-dir="))
            .expect("a managed profile")
    }

    /// Every switch but the automation channel and session restoration, in
    /// order. A first launch's startup URL is not a switch.
    fn managed_switches(&self) -> Vec<&str> {
        let automation = self.automation_switches();
        self.args[1..]
            .iter()
            .map(String::as_str)
            .filter(|arg| {
                arg.starts_with("--")
                    && !automation.contains(arg)
                    && *arg != "--restore-last-session"
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
form.addEventListener('submit',event=>{event.preventDefault();report('submit',{value:email.value})});
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
                            "Content-Type: text/html\r\nSet-Cookie: ambit_sign_in=1; Path=/; Max-Age=86400; SameSite=Lax\r\n",
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
                            recorded.lock().unwrap().push(
                                url::form_urlencoded::parse(query.as_bytes())
                                    .into_owned()
                                    .collect(),
                            );
                            ("204 No Content", "", "")
                        }
                        "/" => ("200 OK", "Content-Type: text/html\r\n", FORM),
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
        automation.automation_switches(),
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

    // The same browser, profile and flags, with no automation channel at all.
    let person = ChromeMain::observe(&state).await;
    assert_ne!(person.pid, automation.pid);
    assert!(
        !process_alive(automation.pid),
        "the automation browser still runs"
    );
    assert!(person.automation_switches().is_empty(), "{:?}", person.args);
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

    // The person's own input reaches the page through the display alone. A
    // person points at the field before clicking and moves again when
    // nothing reacts; a Chrome that has just started can miss native input
    // for its first moments. Point until the page sees the pointer.
    let mut sequence = 2;
    let pointing = tokio::time::timeout(Duration::from_secs(10), async {
        for attempt in 0u32.. {
            let mut pointed = control("input", &controller);
            pointed["sequence"] = json!(sequence);
            pointed["expectedSurfaceGeneration"] = json!(signing_in);
            pointed["events"] = json!([{ "type": "input_mouse", "eventType": "mouseMoved",
                "x": x + f64::from(attempt % 2) * 8.0, "y": y, "button": "none", "buttons": 0 }]);
            assert_eq!(
                assert_success(&command(&pointed, &mut state).await)["status"],
                "applied"
            );
            sequence += 1;
            if site.sees_native_pointer(Duration::from_millis(250)).await {
                return attempt + 1;
            }
        }
        unreachable!()
    })
    .await;
    let native_input_ready = entered_at.elapsed();
    let Ok(pointing_attempts) = pointing else {
        let window = dump_window(&state, "pointing").await;
        panic!(
            "the page never saw the person's pointer; the window: {}; page reports: {:?}",
            window.display(),
            site.reports.lock().unwrap()
        );
    };
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
    assert_eq!(again.automation_switches(), ["--remote-debugging-port=0"]);
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
    assert_eq!(again.automation_switches(), ["--remote-debugging-port=0"]);
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
    assert_eq!(again.automation_switches(), ["--remote-debugging-port=0"]);
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
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
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
    let started = Instant::now();
    let released = command(&control("release", &controller), &mut state).await;
    assert_eq!(assert_success(&released)["status"], "released");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert!(
        state.browser.is_some(),
        "automation did not come back: its relaunch failed"
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
