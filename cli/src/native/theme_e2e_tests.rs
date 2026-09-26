//! The browser theme through a real Chrome, its private display and the
//! display helper: Chrome's own window UI takes the session theme at every
//! launch, and every page's `prefers-color-scheme` takes it and switches
//! with it live, whoever opened the page.
//!
//! These tests need a Chromium executable, Xvfb and the `browser-display`
//! helper, so they are `#[ignore]`d. Run them serially against the qualified
//! workspace runtime, away from any desktop session a host Chrome could take
//! a theme from:
//!   env -u DBUS_SESSION_BUS_ADDRESS GTK_THEME=Adwaita DISPLAY= \
//!   AGENT_BROWSER_EXECUTABLE_PATH=… AGENT_BROWSER_DISPLAY_HELPER=… \
//!   cargo test theme_e2e -- --ignored --test-threads=1

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::actions::{execute_command, DaemonState};
use super::display::CaptureRequest;
use super::sign_in_e2e_tests::{acquire, control, sign_in, ChromeMain};
use crate::test_utils::EnvGuard;
use Report::{Changed, Parsed};
use Scheme::{Dark, Light};

async fn command(command: &Value, state: &mut DaemonState) -> Value {
    Box::pin(execute_command(command, state)).await
}

fn assert_success(response: &Value) -> &Value {
    assert_eq!(response["success"], true, "expected success: {response:#}");
    &response["data"]
}

fn set_theme(theme: &str) -> Value {
    json!({ "action": "set_theme", "theme": theme })
}

fn navigate(url: String) -> Value {
    json!({ "action": "navigate", "url": url })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Dark,
    Light,
}

/// What a document reported of its `prefers-color-scheme`: the value its
/// first script saw, or a change it was told of. A hidden tab renders no
/// frames, so it hears of a change only once it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    Parsed(Scheme),
    Changed(Scheme),
}

/// One document: it reports its scheme when its first script runs and at
/// every change. Reports are never cached: a retained profile would answer a
/// repeated report from its HTTP cache.
const DOCUMENT: &str = "<!doctype html><title>{doc}</title><script>\
const scheme=matchMedia('(prefers-color-scheme: dark)');\
const report=why=>fetch('/report?'+new URLSearchParams({doc:'{doc}',why,dark:scheme.matches}),{cache:'no-store'});\
report('parse');scheme.addEventListener('change',()=>report('change'))</script>";

/// A frame from the other site (`localhost`), which Chrome runs out of
/// process.
const CROSS_SITE_FRAME: &str = "<iframe id=frame></iframe><script>\
frame.src=`http://localhost:${location.port}/page?doc={doc}-frame`</script>";

/// A local site of reporting documents. `/top?doc=x` is document `x` with
/// document `x-frame` in a cross-site frame; `/page?doc=x` is document `x`
/// alone.
struct SchemeSite {
    port: u16,
    reports: Arc<Mutex<Vec<(String, Report)>>>,
    /// Every request target, in order, for a failure's evidence.
    requests: Arc<Mutex<Vec<String>>>,
    _server: tokio::task::JoinHandle<()>,
}

impl SchemeSite {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (recorded, logged) = (reports.clone(), requests.clone());
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (recorded, logged) = (recorded.clone(), logged.clone());
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8 * 1024];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let target = request.split_whitespace().nth(1).unwrap_or("/");
                    logged.lock().unwrap().push(target.to_string());
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let params: HashMap<String, String> =
                        url::form_urlencoded::parse(query.as_bytes())
                            .into_owned()
                            .collect();
                    let param = |name: &str| params.get(name).map(String::as_str).unwrap_or("");
                    let doc = param("doc");
                    let body = match path {
                        "/report" => {
                            let scheme = if param("dark") == "true" { Dark } else { Light };
                            let report = if param("why") == "parse" {
                                Parsed(scheme)
                            } else {
                                Changed(scheme)
                            };
                            recorded.lock().unwrap().push((doc.to_string(), report));
                            None
                        }
                        "/top" => Some(
                            DOCUMENT.replace("{doc}", doc)
                                + &CROSS_SITE_FRAME.replace("{doc}", doc),
                        ),
                        "/page" => Some(DOCUMENT.replace("{doc}", doc)),
                        _ => None,
                    };
                    let response = match body {
                        Some(body) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => {
                            "HTTP/1.1 204 No Content\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
                                .into()
                        }
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            port,
            reports,
            requests,
            _server: server,
        }
    }

    fn top(&self, doc: &str) -> String {
        format!("http://127.0.0.1:{}/top?doc={doc}", self.port)
    }

    fn page(&self, doc: &str) -> String {
        format!("http://127.0.0.1:{}/page?doc={doc}", self.port)
    }

    fn reports_of(&self, doc: &str) -> Vec<Report> {
        self.reports
            .lock()
            .unwrap()
            .iter()
            .filter(|(reported, _)| reported == doc)
            .map(|(_, report)| *report)
            .collect()
    }

    /// `doc`'s reports once it has made `count`. Meanwhile the daemon's
    /// maintenance drain runs, as it does every 100 ms in the daemon: it
    /// adopts the pages Chrome opened outside the agent's commands.
    async fn reports(&self, state: &mut DaemonState, doc: &str, count: usize) -> Vec<Report> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.reports_of(doc);
            if seen.len() >= count {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "{doc} made {} of {count} reports: {:?}; requests: {:?}",
                seen.len(),
                self.reports.lock().unwrap(),
                self.requests.lock().unwrap()
            );
            maintain(state).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// One maintenance drain, as the daemon runs every 100 ms.
async fn maintain(state: &mut DaemonState) {
    state
        .drain_cdp_events_background()
        .await
        .expect("the maintenance drain succeeds");
}

/// The `prefers-color-scheme` every attached document has now, by title:
/// each page and each cross-site frame answers in its own session, a hidden
/// tab included. A page behind an open dialog cannot answer and is left out.
async fn schemes_now(state: &DaemonState) -> HashMap<String, Scheme> {
    let browser = state.browser.as_ref().expect("an automated browser");
    let sessions = browser
        .pages_list()
        .into_iter()
        .map(|page| page.session_id)
        .chain(state.iframe_sessions.values().cloned());
    let mut schemes = HashMap::new();
    for session in sessions {
        let query = browser.client.send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "[document.title, matchMedia('(prefers-color-scheme: dark)').matches]",
                "returnByValue": true,
            })),
            Some(&session),
        );
        let Ok(Ok(answer)) = tokio::time::timeout(Duration::from_secs(1), query).await else {
            continue;
        };
        let value = &answer["result"]["value"];
        if let Some(title) = value[0].as_str() {
            let scheme = if value[1] == true { Dark } else { Light };
            schemes.insert(title.to_string(), scheme);
        }
    }
    schemes
}

/// Waits until each of `docs` has `scheme` now, running the maintenance
/// drain meanwhile.
async fn settle(state: &mut DaemonState, docs: &[&str], scheme: Scheme) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = schemes_now(state).await;
        if docs.iter().all(|doc| now.get(*doc) == Some(&scheme)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "not every one of {docs:?} is {scheme:?}: {now:?}"
        );
        maintain(state).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Chrome's tab strip: the top 42 DIP of the window (measured on the
/// qualification image, Chrome for Testing 152).
const TAB_STRIP_DIP: u32 = 42;

/// The scheme Chrome's own window UI shows now, from the mean luma of its tab
/// strip in a whole frame of the window: about 0.9 light and 0.15 dark on the
/// qualification runtime. Each frame is saved for the evidence.
async fn window_ui_now(state: &DaemonState, name: &str) -> Result<Scheme, String> {
    let display = state.window_display().ok_or("no owned browser window")?;
    let request = CaptureRequest {
        cursor: false,
        budget_bytes: 0,
        force: true,
        patches: false,
    };
    let (capture, surface) = display
        .capture(request)
        .await
        .map_err(|error| error.message)?
        .ok_or("a forced capture returned no frame")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(capture.data.ok_or("the capture is not a whole frame")?)
        .map_err(|error| error.to_string())?;
    let frame = image::load_from_memory(&bytes)
        .map_err(|error| error.to_string())?
        .to_luma8();
    let rows = (TAB_STRIP_DIP * surface.device_scale_factor).min(frame.height());
    let sum: u64 = frame
        .rows()
        .take(rows as usize)
        .flatten()
        .map(|pixel| u64::from(pixel[0]))
        .sum();
    let luma = sum as f64 / (f64::from(rows) * f64::from(frame.width()) * 255.0);
    let path = std::env::temp_dir().join(format!(
        "theme-e2e-{name}-{}.{}",
        std::process::id(),
        capture.encoding
    ));
    std::fs::write(&path, &bytes).map_err(|error| error.to_string())?;
    println!(
        "THEME_FRAME {name} tabStripLuma={luma:.3} {}",
        path.display()
    );
    Ok(if luma < 0.5 { Dark } else { Light })
}

/// Waits until Chrome's window UI shows `scheme`: a new window paints its tab
/// strip a moment after its browser answers.
async fn window_ui(state: &DaemonState, scheme: Scheme, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let shown = window_ui_now(state, name).await;
        if shown == Ok(scheme) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the window UI shows {shown:?}, not {scheme:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Chrome's window UI takes the session theme at every launch, and every
/// page takes it and switches with it live: the agent's tabs, cross-site
/// frames, a popup and a tab the person opens. The UI of a running window
/// keeps its launch's theme, and the theme change says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_theme_switches_every_page_live_and_the_window_ui_at_the_next_launch() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = SchemeSite::start().await;
    let mut state = DaemonState::new();

    // A dark launch: the window UI, the page and its cross-site frame.
    assert_success(&command(&json!({ "action": "launch", "theme": "dark" }), &mut state).await);
    assert!(ChromeMain::observe(&state).await.has("--force-dark-mode"));
    assert_success(&command(&navigate(site.top("first")), &mut state).await);
    for doc in ["first", "first-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 1).await,
            [Parsed(Dark)],
            "{doc}"
        );
    }
    assert!(
        !state.iframe_sessions.is_empty(),
        "the cross-site frame must run out of process"
    );
    window_ui(&state, Dark, "launched-dark").await;

    // Every page switches now and hears of it; the running window's UI keeps
    // its launch's theme, as the result says.
    let started = Instant::now();
    let switched = command(&set_theme("light"), &mut state).await;
    let answered = started.elapsed();
    assert_eq!(
        assert_success(&switched),
        &json!({ "theme": "light", "pages": "live", "ui": "next_launch" })
    );
    for doc in ["first", "first-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 2).await,
            [Parsed(Dark), Changed(Light)],
            "{doc}"
        );
    }
    let heard = started.elapsed();
    assert_eq!(window_ui_now(&state, "pages-light").await, Ok(Dark));
    println!(
        "THEME_SWITCH {}",
        json!({ "answeredMs": answered.as_millis(), "pageAndFrameHeardMs": heard.as_millis() })
    );

    // An explicit scheme overrides the theme until the next theme change.
    assert_success(
        &command(
            &json!({ "action": "set_media", "colorScheme": "dark" }),
            &mut state,
        )
        .await,
    );
    for doc in ["first", "first-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 3).await[2],
            Changed(Dark),
            "{doc}"
        );
    }
    assert_success(&command(&set_theme("light"), &mut state).await);
    for doc in ["first", "first-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 4).await[3],
            Changed(Light),
            "{doc}"
        );
    }

    // A tab the agent opens after the change, and its cross-site frame, start
    // in it before their first script, though Chrome's own preference is
    // still the launch's dark.
    assert_success(
        &command(
            &json!({ "action": "tab_new", "url": site.top("tab") }),
            &mut state,
        )
        .await,
    );
    for doc in ["tab", "tab-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 1).await,
            [Parsed(Light)],
            "{doc}"
        );
    }
    // A popup and a tab the person opens run before the daemon adopts them,
    // so their first script may see Chrome's own preference; adoption gives
    // them the theme.
    let popup = format!("open({:?}); 'opened'", site.page("popup"));
    assert_success(
        &command(
            &json!({ "action": "evaluate", "script": popup }),
            &mut state,
        )
        .await,
    );
    state
        .browser
        .as_ref()
        .unwrap()
        .client
        .send_command(
            "Target.createTarget",
            Some(json!({ "url": site.page("person") })),
            None,
        )
        .await
        .expect("the person opens a tab");
    for doc in ["popup", "person"] {
        site.reports(&mut state, doc, 1).await;
    }
    settle(&mut state, &["popup", "person"], Light).await;
    println!(
        "THEME_UNPAUSED_FIRST_SCRIPT {}",
        json!({
            "popup": format!("{:?}", site.reports_of("popup")),
            "person": format!("{:?}", site.reports_of("person")),
        })
    );

    // Every page switches live, shown or hidden. A hidden tab's cross-site
    // frame follows once its tab is shown, the first time anyone can see it
    // (measured: Chrome may defer it while the tab is hidden).
    assert_success(&command(&set_theme("dark"), &mut state).await);
    settle(&mut state, &["first", "tab", "popup", "person"], Dark).await;
    for (tab, frame) in [("t1", "first-frame"), ("t2", "tab-frame")] {
        assert_success(
            &command(&json!({ "action": "tab_switch", "tabId": tab }), &mut state).await,
        );
        settle(&mut state, &[frame], Dark).await;
    }
    assert_success(&command(&set_theme("light"), &mut state).await);

    // The next launch draws the window UI in the session theme.
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
    assert_success(&command(&json!({ "action": "launch" }), &mut state).await);
    assert!(!ChromeMain::observe(&state).await.has("--force-dark-mode"));
    window_ui(&state, Light, "relaunched-light").await;
    assert_success(&command(&navigate(site.top("relaunched")), &mut state).await);
    for doc in ["relaunched", "relaunched-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 1).await,
            [Parsed(Light)],
            "{doc}"
        );
    }
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// Opens an isolated window and one sharing the browser's cookies, each on
/// document `isolated{suffix}` or `shared{suffix}` with its cross-site frame.
async fn open_windows(state: &mut DaemonState, site: &SchemeSite, suffix: &str) {
    for (shared, name) in [(false, "isolated"), (true, "shared")] {
        let window = json!({ "action": "window_new", "shared": shared });
        assert_success(&command(&window, state).await);
        let doc = format!("{name}{suffix}");
        assert_success(&command(&navigate(site.top(&doc)), state).await);
    }
}

/// A window the agent opens, isolated or sharing the browser's cookies,
/// starts in the session theme and switches with it, also after a live change
/// has left Chrome's own preference at the launch's theme.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_theme_reaches_every_window_the_agent_opens() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = SchemeSite::start().await;
    let mut state = DaemonState::new();
    assert_success(&command(&json!({ "action": "launch", "theme": "dark" }), &mut state).await);
    open_windows(&mut state, &site, "").await;
    for doc in ["isolated", "isolated-frame", "shared", "shared-frame"] {
        assert_eq!(
            site.reports(&mut state, doc, 1).await,
            [Parsed(Dark)],
            "{doc}"
        );
    }

    assert_success(&command(&set_theme("light"), &mut state).await);
    settle(&mut state, &["isolated", "shared"], Light).await;
    open_windows(&mut state, &site, "-after").await;
    for doc in [
        "isolated-after",
        "isolated-after-frame",
        "shared-after",
        "shared-after-frame",
    ] {
        assert_eq!(
            site.reports(&mut state, doc, 1).await,
            [Parsed(Light)],
            "{doc}"
        );
    }
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// Counts the documents an init script ran in, per window.
const COUNT_RUNS: &str = "window.__runs = (window.__runs || 0) + 1";

/// How many times the init script ran in the active page's document.
async fn runs(state: &mut DaemonState) -> Value {
    let read = json!({ "action": "evaluate", "script": "window.__runs ?? 0" });
    assert_success(&command(&read, state).await)["result"].clone()
}

/// Every page gets the session setup once, however it opened: the tab and
/// the window the agent opens, whose own attachment reaches the adoption of
/// discovered pages too, and a tab the person opens. An init script runs once
/// in each document, and removing it removes it from every page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_theme_every_page_gets_the_session_setup_once() {
    let site = SchemeSite::start().await;
    let mut state = DaemonState::new();
    let launch = json!({ "action": "launch", "headless": true, "theme": "dark" });
    assert_success(&command(&launch, &mut state).await);
    assert_success(&command(&navigate(site.page("first")), &mut state).await);
    let add = json!({ "action": "addinitscript", "script": COUNT_RUNS });
    let identifier = assert_success(&command(&add, &mut state).await)["identifier"].clone();

    let tab = json!({ "action": "tab_new", "url": site.page("tab") });
    assert_success(&command(&tab, &mut state).await);
    assert_eq!(runs(&mut state).await, 1, "a tab the agent opens");

    assert_success(&command(&json!({ "action": "window_new" }), &mut state).await);
    assert_success(&command(&navigate(site.page("window")), &mut state).await);
    assert_eq!(runs(&mut state).await, 1, "a window the agent opens");

    state
        .browser
        .as_ref()
        .unwrap()
        .client
        .send_command(
            "Target.createTarget",
            Some(json!({ "url": site.page("person") })),
            None,
        )
        .await
        .expect("the person opens a tab");
    site.reports(&mut state, "person", 1).await;
    let tabs = command(&json!({ "action": "tab_list" }), &mut state).await;
    let person = assert_success(&tabs)["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tab| {
            tab["url"]
                .as_str()
                .is_some_and(|url| url.ends_with("doc=person"))
        })
        .map(|tab| tab["tabId"].clone())
        .expect("the person's tab is adopted");
    // Chrome did not pause that tab, so its first document ran before the
    // daemon adopted it; the next one runs under the setup.
    let switch = json!({ "action": "tab_switch", "tabId": person });
    assert_success(&command(&switch, &mut state).await);
    assert_success(&command(&navigate(site.page("person-next")), &mut state).await);
    assert_eq!(runs(&mut state).await, 1, "a tab the person opens");

    let remove = json!({ "action": "removeinitscript", "identifier": identifier });
    assert_success(&command(&remove, &mut state).await);
    let open_tabs = command(&json!({ "action": "tab_list" }), &mut state).await;
    let open_tabs: Vec<Value> = assert_success(&open_tabs)["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tab| tab["tabId"].clone())
        .collect();
    assert_eq!(open_tabs.len(), 4, "{open_tabs:?}");
    for tab in open_tabs {
        let switch = json!({ "action": "tab_switch", "tabId": tab });
        assert_success(&command(&switch, &mut state).await);
        assert_success(&command(&navigate(site.page("reloaded")), &mut state).await);
        assert_eq!(runs(&mut state).await, 0, "{tab} after the removal");
    }
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// A page the daemon opens because the last one closed gets the session
/// setup like any page it opens: the theme, not Chrome's own preference, and
/// each init script once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_theme_reaches_the_page_that_replaces_the_last_one() {
    let site = SchemeSite::start().await;
    let mut state = DaemonState::new();
    // Headless Chrome prefers light on its own; the session theme is dark.
    let launch = json!({ "action": "launch", "headless": true, "theme": "dark" });
    assert_success(&command(&launch, &mut state).await);
    assert_success(&command(&navigate(site.page("first")), &mut state).await);
    let add = json!({ "action": "addinitscript", "script": COUNT_RUNS });
    assert_success(&command(&add, &mut state).await);

    let browser = state.browser.as_ref().unwrap();
    let only = browser.pages_list()[0].target_id.clone();
    browser
        .client
        .send_command(
            "Target.closeTarget",
            Some(json!({ "targetId": only })),
            None,
        )
        .await
        .expect("the last page closes");
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.browser.as_ref().unwrap().page_count() > 0 {
        assert!(Instant::now() < deadline, "the closed page is still listed");
        maintain(&mut state).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_success(&command(&navigate(site.page("replacement")), &mut state).await);
    assert_eq!(
        site.reports(&mut state, "replacement", 1).await,
        [Parsed(Dark)]
    );
    assert_eq!(runs(&mut state).await, 1);
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// A page behind an open JavaScript dialog applies media emulation only
/// once the dialog closes (measured on Chrome for Testing 152). The theme
/// change still answers within its bound, the other pages switch at once,
/// and the page follows as soon as the dialog is handled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_theme_change_is_not_held_by_a_page_behind_a_dialog() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = SchemeSite::start().await;
    let mut state = DaemonState::new();
    assert_success(&command(&json!({ "action": "launch", "theme": "light" }), &mut state).await);
    assert_success(&command(&navigate(site.page("asking")), &mut state).await);
    assert_success(
        &command(
            &json!({ "action": "tab_new", "url": site.page("other") }),
            &mut state,
        )
        .await,
    );
    for doc in ["asking", "other"] {
        assert_eq!(
            site.reports(&mut state, doc, 1).await,
            [Parsed(Light)],
            "{doc}"
        );
    }
    assert_success(
        &command(
            &json!({ "action": "tab_switch", "tabId": "t1" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &command(
            &json!({ "action": "evaluate", "script": "setTimeout(() => confirm('Keep going?')); 'asked'" }),
            &mut state,
        )
        .await,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while command(
        &json!({ "action": "dialog", "response": "status" }),
        &mut state,
    )
    .await["data"]["hasDialog"]
        != true
    {
        assert!(Instant::now() < deadline, "the dialog never opened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let started = Instant::now();
    let switched = command(&set_theme("dark"), &mut state).await;
    let answered = started.elapsed();
    assert_eq!(
        assert_success(&switched),
        &json!({ "theme": "dark", "pages": "live", "ui": "next_launch" })
    );
    assert!(answered < Duration::from_secs(3), "{answered:?}");
    let now = schemes_now(&state).await;
    assert_eq!(now.get("other"), Some(&Dark), "{now:?}");
    assert!(!now.contains_key("asking"), "{now:?}");
    assert_eq!(site.reports_of("asking"), [Parsed(Light)]);

    assert_success(
        &command(
            &json!({ "action": "dialog", "response": "accept" }),
            &mut state,
        )
        .await,
    );
    assert_eq!(
        site.reports(&mut state, "asking", 2).await,
        [Parsed(Light), Changed(Dark)]
    );
    println!(
        "THEME_DIALOG {}",
        json!({ "answeredMs": answered.as_millis() })
    );
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}

/// The theme follows the browser through sign-in: the sign-in relaunch keeps
/// the session theme; a theme change while the person signs in reaches no
/// page (that browser has no automation channel) and takes effect at the
/// hand-back relaunch, in the window UI and in the restored pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_theme_sign_in_relaunches_take_the_session_theme() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let site = SchemeSite::start().await;
    let mut state = DaemonState::new();

    // Without a browser the theme waits for the next launch, which takes it.
    assert_eq!(
        assert_success(&command(&set_theme("dark"), &mut state).await),
        &json!({ "theme": "dark", "pages": "next_launch", "ui": "next_launch" })
    );
    assert_success(&command(&navigate(site.page("form")), &mut state).await);
    let automation = ChromeMain::observe(&state).await;
    assert!(automation.has("--force-dark-mode"));
    assert_eq!(site.reports(&mut state, "form", 1).await, [Parsed(Dark)]);
    window_ui(&state, Dark, "automation-dark").await;

    // Sign-in relaunches without automation, in the session theme.
    let controller = acquire(&mut state).await;
    let (entered, _) = sign_in(&mut state, &controller, 1, 600_000).await;
    assert_eq!(assert_success(&entered)["status"], "applied");
    let person = ChromeMain::observe(&state).await;
    assert_ne!(person.pid, automation.pid);
    assert!(person.has("--force-dark-mode"));
    assert_eq!(
        site.reports(&mut state, "form", 2).await,
        [Parsed(Dark), Parsed(Dark)]
    );
    window_ui(&state, Dark, "signing-in-dark").await;

    // No page can switch while the person signs in, and nothing relaunches.
    assert_eq!(
        assert_success(&command(&set_theme("light"), &mut state).await),
        &json!({ "theme": "light", "pages": "next_launch", "ui": "next_launch" })
    );
    assert_eq!(ChromeMain::observe(&state).await.pid, person.pid);
    assert_eq!(
        window_ui_now(&state, "signing-in-after-change").await,
        Ok(Dark)
    );
    assert_eq!(site.reports_of("form").len(), 2);

    // The hand-back relaunch takes the session theme.
    let released = command(&control("release", &controller), &mut state).await;
    assert_eq!(assert_success(&released)["status"], "released");
    assert!(!ChromeMain::observe(&state).await.has("--force-dark-mode"));
    assert_eq!(
        site.reports(&mut state, "form", 3).await,
        [Parsed(Dark), Parsed(Dark), Parsed(Light)]
    );
    window_ui(&state, Light, "handed-back-light").await;
    assert_success(&command(&json!({ "action": "close" }), &mut state).await);
}
