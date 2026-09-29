//! What a step's input landed as (agent-channel contract §5 `landed`).
//!
//! A step that is not read-only starts recording once its preconditions
//! admitted it, just before its first input (`Landing::arm`): the page's
//! DevTools events (main-frame navigations and their HTTP status, JavaScript
//! dialogs, the agent's acknowledged input) and, in the channel's world, a
//! watcher that reports each batch of mutation records and each scroll on
//! the main document through a binding only that world has.
//!
//! After the operation the landing waits without command custody
//! (`Landing::wait`): for a main-frame navigation that began, until it
//! commits, within the step's deadline; otherwise until the page has been
//! quiet (no mutation, no scroll) for `QUIET` since the step's last input,
//! at most `SETTLE_LIMIT` after it. A step that sent no input waits so only
//! when its page changed during it, from its start. A committed document
//! ends the wait: the page it would watch is gone. A person taking control
//! ends the wait at once, and then no final read is taken: the block says
//! what was recorded, with `pending`. Otherwise custody is taken again only
//! for the final read of the addressed element and of the tabs the step
//! opened (`Landing::read`), which also leaves the active page painted, as
//! the file protocol's capture does, so the next step's input reaches it.
//! The final read reads nothing from a page between documents
//! (`documents`), which answers nothing until its navigation commits, and
//! nothing after the step's deadline; the block then says `pending` too.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use super::target::{self, Node, Recorders, FACTS};
use crate::native::actions::DaemonState;
use crate::native::browser_control::Interrupts;
use crate::native::cdp::client::CdpClient;
use crate::native::cdp::types::CdpEvent;
use crate::native::documents;

/// The page is quiet once this long has passed without a mutation or a
/// scroll.
const QUIET: Duration = Duration::from_millis(50);
/// The longest the landing waits for quiet after the step's last input.
const SETTLE_LIMIT: Duration = Duration::from_millis(400);
/// The most mutation records `changedNodes` counts.
const MOST_CHANGES: u64 = 10_000;
/// The most characters of an element's value `target.value` reports.
const MOST_VALUE_CHARS: usize = 1024;
/// How long making a page paint may take: a page too busy to answer is left
/// as it is.
const PAINT_WAIT: Duration = Duration::from_millis(1000);

/// The binding the watcher reports through: installed only in the channel's
/// world, so page script can neither call it nor see it.
pub(crate) const BINDING: &str = "__ambitLanded";

/// Starts a watcher for one step, reporting `<token> <records>` per batch of
/// mutation records, and `<token> 0` per scroll.
const WATCH: &str = r#"function(token) {
    const report = globalThis.__ambitLanded;
    if (typeof report !== 'function') return false;
    const watchers = globalThis.__ambitWatchers || (globalThis.__ambitWatchers = new Map());
    const observer = new MutationObserver((records) => report(token + ' ' + records.length));
    observer.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
    const scrolled = () => report(token + ' 0');
    document.addEventListener('scroll', scrolled, { capture: true, passive: true });
    watchers.set(token, () => {
        observer.disconnect();
        document.removeEventListener('scroll', scrolled, { capture: true });
        watchers.delete(token);
    });
    return true;
}"#;

const UNWATCH: &str = r#"function(token) {
    const stop = globalThis.__ambitWatchers && globalThis.__ambitWatchers.get(token);
    if (stop) stop();
}"#;

/// The codes of a step that was not performed (§5 "Status": addressing,
/// observation, custody, runtime and ceiling refusals). Such a step has no
/// landed block.
const REFUSALS: &[&str] = &[
    "browser_active_page_ambiguous",
    "tab_not_found",
    "tab_gone",
    "browser_observation_required",
    "browser_observation_stale",
    "browser_controlled_by_user",
    "browser_layout_pending",
    "browser_feedback_invalid",
    "browser_operation_rejected",
    "browser_runtime_unavailable",
    "browser_effect_refused",
    "browser_dialog_open",
];

/// Whether a step answered `response` may have acted, so its landing is
/// reported.
pub(crate) fn lands(response: &Value) -> bool {
    response["success"] == true
        || !response["code"]
            .as_str()
            .is_some_and(|code| REFUSALS.contains(&code))
}

/// Tells one step's watcher from another's, even of another channel.
static TOKENS: AtomicU64 = AtomicU64::new(1);

/// A person's hold on the browser, watched without command custody: a
/// takeover asked for since the watch began (pending, or already holding
/// command custody while it takes the browser), or a lease that holds it.
pub(crate) struct PersonWatch {
    interrupts: Interrupts,
    raised: watch::Receiver<u64>,
    custody: watch::Receiver<Option<Instant>>,
    /// The takeovers raised before the watch began.
    takeovers: u64,
}

impl PersonWatch {
    pub(crate) async fn of(state: &DaemonState) -> Self {
        let control = state.browser_control.lock().await;
        let interrupts = control.interrupts();
        Self {
            raised: interrupts.subscribe(),
            takeovers: interrupts.takeovers(),
            interrupts,
            custody: control.custody(),
        }
    }

    /// Whether a person holds the browser, or is taking it now.
    pub(crate) fn holds(&self) -> bool {
        self.interrupts.takeover()
            || self.interrupts.takeovers() != self.takeovers
            || self
                .custody
                .borrow()
                .is_some_and(|deadline| deadline > Instant::now())
    }

    /// Resolves once a person holds the browser or is taking it.
    pub(crate) async fn taken(&mut self) {
        while !self.holds() {
            tokio::select! {
                changed = self.raised.changed() => if changed.is_err() { std::future::pending::<()>().await },
                changed = self.custody.changed() => if changed.is_err() { std::future::pending::<()>().await },
            }
        }
    }
}

/// The page a step acted on, as its input began.
struct Page {
    client: Arc<CdpClient>,
    session: String,
    /// Its main frame.
    frame: String,
    /// The document's loader when the input began; empty when the page was
    /// between documents then.
    loader: String,
    /// The channel's world, when the watcher runs there.
    watched: Option<i64>,
}

/// What the page's events said since the landing was armed.
#[derive(Debug, Default, PartialEq)]
struct Recorded {
    /// The loader of the last document the main frame committed.
    committed: Option<String>,
    /// The main frame's URL after its last navigation of either kind.
    url: Option<String>,
    /// The page is between documents (`documents`), from what this step
    /// saw.
    navigating: bool,
    /// HTTP status of each main-frame document response, by loader.
    statuses: HashMap<String, i64>,
    dialog: Option<Value>,
    changes: u64,
    /// When the page last mutated or scrolled.
    activity: Option<Instant>,
    /// When the agent's last input was acknowledged.
    input: Option<Instant>,
    /// The page went away.
    closed: bool,
}

impl Recorded {
    fn note(&mut self, event: &CdpEvent, page: (&str, &str), token: &str, now: Instant) {
        let (session, frame) = page;
        let params = &event.params;
        if event.method == crate::native::activity::EVENT {
            if params["source"] == "agent" {
                self.input = Some(params["ts"].as_u64().map_or(now, media_instant));
            }
            return;
        }
        if event.method == "Target.detachedFromTarget" {
            self.closed |= params["sessionId"] == session;
            return;
        }
        if event.session_id.as_deref() != Some(session) {
            return;
        }
        if let Some(change) = documents::change(event, frame) {
            self.navigating = change == documents::Change::Leaving;
        }
        let main = params["frameId"] == frame;
        match event.method.as_str() {
            "Runtime.bindingCalled" if params["name"] == BINDING => {
                let reported = params["payload"].as_str().and_then(|payload| {
                    let (from, records) = payload.split_once(' ')?;
                    (from == token).then(|| records.parse::<u64>().ok())?
                });
                if let Some(records) = reported {
                    self.changes = self.changes.saturating_add(records);
                    self.activity = Some(now);
                }
            }
            "Page.frameNavigated" if params["frame"]["id"] == frame => {
                self.committed = params["frame"]["loaderId"].as_str().map(String::from);
                self.url = params["frame"]["url"].as_str().map(String::from);
            }
            "Page.navigatedWithinDocument" if main => {
                self.url = params["url"].as_str().map(String::from);
            }
            "Network.responseReceived" if main && params["type"] == "Document" => {
                if let (Some(loader), Some(status)) = (
                    params["loaderId"].as_str(),
                    params["response"]["status"].as_i64(),
                ) {
                    self.statuses.insert(loader.to_string(), status);
                }
            }
            "Page.javascriptDialogOpening" => {
                let mut dialog = json!({ "type": params["type"], "message": params["message"] });
                if let Some(prompt) = params["defaultPrompt"].as_str() {
                    dialog["defaultPrompt"] = json!(prompt);
                }
                self.dialog = Some(dialog);
            }
            "Inspector.detached" => self.closed = true,
            _ => {}
        }
    }

    /// Until when a landing that is not navigating waits: quiet for `QUIET`
    /// after `anchor` and after the last activity, and never past
    /// `SETTLE_LIMIT` after `anchor`. The anchor is the last input, or the
    /// step's start for a step that sent none but changed its page. A step
    /// that did neither, or whose document was replaced, has nothing to
    /// wait for.
    fn quiet_until(&self, armed: Instant) -> Option<Instant> {
        if self.committed.is_some() {
            return None;
        }
        let anchor = match (self.input, self.activity) {
            (Some(input), _) => input.max(armed),
            (None, Some(_)) => armed,
            (None, None) => return None,
        };
        let quiet_from = self
            .activity
            .map_or(anchor, |activity| activity.max(anchor));
        Some((quiet_from + QUIET).min(anchor + SETTLE_LIMIT))
    }

    /// The navigation the page made during the step: a new document if it
    /// committed one, else a change of its URL within the document, when
    /// that document's `loader` is known.
    fn navigation(&self, loader: &str) -> Option<Value> {
        if let Some(committed) = &self.committed {
            return Some(
                json!({ "kind": "document", "url": self.url, "loaderId": committed,
                "httpStatus": self.statuses.get(committed) }),
            );
        }
        let url = self.url.as_ref().filter(|_| !loader.is_empty())?;
        Some(
            json!({ "kind": "same_document", "url": url, "loaderId": loader,
            "httpStatus": null }),
        )
    }
}

/// The instant a media-clock timestamp (`stream::monotonic_us`) names.
fn media_instant(ts_us: u64) -> Instant {
    let now = Instant::now();
    let ago = crate::native::stream::monotonic_us().saturating_sub(ts_us);
    now.checked_sub(Duration::from_micros(ago)).unwrap_or(now)
}

/// One step's landing, from just before its first input to its final read.
pub(crate) struct Landing {
    token: String,
    armed: Instant,
    page: Option<Page>,
    events: Option<broadcast::Receiver<CdpEvent>>,
    /// The tab roster when the input began.
    tabs: Vec<String>,
    /// The element the step addressed.
    target: Option<Node>,
    recorded: Recorded,
}

impl Landing {
    /// Starts recording on the active page. `watch` is false when the page
    /// cannot run the watcher (a dialog blocks its scripts).
    pub(crate) async fn arm(
        state: &DaemonState,
        recorders: &Recorders,
        target: Option<Node>,
        watch: bool,
    ) -> Self {
        let token = TOKENS.fetch_add(1, Ordering::Relaxed).to_string();
        let mut landing = Self {
            token,
            armed: Instant::now(),
            page: None,
            events: None,
            tabs: Vec::new(),
            target,
            recorded: Recorded::default(),
        };
        let Some(browser) = state.browser.as_ref() else {
            return landing;
        };
        landing.tabs = tab_ids(&browser.tab_list());
        let Ok(session) = browser.active_session_id() else {
            return landing;
        };
        let client = browser.client.clone();
        landing.events = Some(client.subscribe());
        // A page between documents answers nothing until its navigation
        // commits, and the document a watcher would watch is going: only its
        // events are recorded. Its main frame has its target's id.
        let leaving = browser.active_page_between_documents();
        let (frame, loader) = if leaving {
            let Ok(target) = browser.active_target_id() else {
                return landing;
            };
            (target.to_string(), String::new())
        } else {
            let Ok(tree) = client
                .send_command_no_params("Page.getFrameTree", Some(session))
                .await
            else {
                return landing;
            };
            let main = &tree["frameTree"]["frame"];
            let (Some(frame), Some(loader)) = (main["id"].as_str(), main["loaderId"].as_str())
            else {
                return landing;
            };
            (frame.to_string(), loader.to_string())
        };
        let mut page = Page {
            client,
            session: session.to_string(),
            frame,
            loader,
            watched: None,
        };
        if watch && !leaving {
            if let Ok(context) = recorders.world(&page.client, &page.session).await {
                let started = page
                    .client
                    .send_command(
                        "Runtime.callFunctionOn",
                        Some(
                            json!({ "functionDeclaration": WATCH, "executionContextId": context,
                            "arguments": [{ "value": landing.token }], "returnByValue": true }),
                        ),
                        Some(&page.session),
                    )
                    .await;
                if started.is_ok_and(|started| started["result"]["value"] == true) {
                    page.watched = Some(context);
                }
            }
        }
        landing.page = Some(page);
        landing
    }

    fn note_available(&mut self) {
        let (Some(page), Some(events)) = (self.page.as_ref(), self.events.as_mut()) else {
            return;
        };
        loop {
            match events.try_recv() {
                Ok(event) => self.recorded.note(
                    &event,
                    (&page.session, &page.frame),
                    &self.token,
                    Instant::now(),
                ),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(broadcast::error::TryRecvError::Empty) => return,
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.recorded.closed = true;
                    return;
                }
            }
        }
    }

    /// Waits, without command custody, until the step's input has landed
    /// (see the module). Returns whether the wait ended before its answer:
    /// a person took the browser, or a navigation was still uncommitted at
    /// `deadline`.
    pub(crate) async fn wait(&mut self, deadline: Instant, person: &mut PersonWatch) -> bool {
        loop {
            self.note_available();
            if person.holds() {
                return true;
            }
            let recorded = &self.recorded;
            if recorded.closed || recorded.dialog.is_some() {
                return false;
            }
            let until = if recorded.navigating {
                deadline
            } else {
                match recorded.quiet_until(self.armed) {
                    Some(until) => until,
                    None => return false,
                }
            };
            if Instant::now() >= until {
                return recorded.navigating;
            }
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(until));
            let received = tokio::select! {
                biased;
                _ = person.taken() => None,
                received = next_event(self.events.as_mut()) => Some(received),
                _ = sleep => None,
            };
            match (received, &self.page) {
                (Some(Ok(event)), Some(page)) => self.recorded.note(
                    &event,
                    (&page.session, &page.frame),
                    &self.token,
                    Instant::now(),
                ),
                (Some(Err(broadcast::error::RecvError::Closed)), _) => self.recorded.closed = true,
                _ => {}
            }
        }
    }

    /// Stops the page's watcher. Needs no custody, since it only ends this
    /// step's own recording, and waits for no answer: a page busy with a
    /// navigation or blocked by a dialog stops it when it can.
    pub(crate) async fn stop_watching(&self) {
        let Some((page, context)) = self
            .page
            .as_ref()
            .and_then(|page| Some((page, page.watched?)))
        else {
            return;
        };
        let _ = page
            .client
            .enqueue_command(
                "Runtime.callFunctionOn",
                Some(
                    json!({ "functionDeclaration": UNWATCH, "executionContextId": context,
                    "arguments": [{ "value": self.token }] }),
                ),
                Some(&page.session),
            )
            .await;
    }

    /// The block when no final read may be taken: what was recorded.
    pub(crate) fn without_read(&mut self) -> Value {
        self.note_available();
        let target = self
            .target
            .as_ref()
            .map(|node| json!({ "backendNodeId": node.backend_node_id }));
        self.block(target, Vec::new(), true)
    }

    /// The final read, under command custody: the addressed element's state
    /// and the tabs the step opened. `pending` says a navigation was still
    /// uncommitted when the wait ended. Nothing is read from a page between
    /// documents, and the page is read only until the step's `deadline`:
    /// either way the block says `pending`, with the element unread.
    pub(crate) async fn read(
        &mut self,
        state: &mut DaemonState,
        pending: bool,
        deadline: Instant,
    ) -> Value {
        self.note_available();
        let _ = state.drain_cdp_events_background().await;
        let blocked = self.recorded.dialog.is_some() || state.dialog_blocks_active_page();
        let leaving = state.active_page_between_documents();
        let read = async {
            if let Some(browser) = state.browser.as_ref() {
                if let Ok(session) = browser.active_session_id() {
                    painted(&browser.client, session).await;
                }
            }
            match (&self.target, self.page.as_ref()) {
                (Some(node), Some(page)) => Some(element_state(&page.client, node).await),
                _ => None,
            }
        };
        let (read, cut) = if blocked || leaving {
            (None, leaving)
        } else {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => (None, true),
                read = read => (read, false),
            }
        };
        let target = read.or_else(|| {
            self.target
                .as_ref()
                .map(|node| json!({ "backendNodeId": node.backend_node_id }))
        });
        let opened = state
            .browser
            .as_ref()
            .map(|browser| {
                browser
                    .tab_list()
                    .into_iter()
                    .filter(|tab| {
                        tab["tabId"]
                            .as_str()
                            .is_some_and(|id| !self.tabs.iter().any(|before| before == id))
                    })
                    .map(|tab| json!({ "tabId": tab["tabId"], "url": tab["url"] }))
                    .collect()
            })
            .unwrap_or_default();
        self.block(target, opened, pending || cut)
    }

    fn block(&self, target: Option<Value>, opened: Vec<Value>, pending: bool) -> Value {
        let recorded = &self.recorded;
        let mut landed = json!({ "changedNodes": recorded.changes.min(MOST_CHANGES) });
        let loader = self.page.as_ref().map_or("", |page| page.loader.as_str());
        if let Some(navigation) = recorded.navigation(loader) {
            landed["navigation"] = navigation;
        }
        if let Some(target) = target {
            landed["target"] = target;
        }
        if let Some(dialog) = &recorded.dialog {
            landed["dialog"] = dialog.clone();
        }
        if !opened.is_empty() {
            landed["openedTabs"] = json!(opened);
        }
        if pending {
            landed["pending"] = json!(true);
        }
        landed
    }
}

/// Makes the page of `session` paint if it has not yet. Until a document
/// has painted, Chrome drops the discrete input (a press, a key) it is sent,
/// and it holds a same-origin navigation's first paint up to 500 ms for a
/// page that shows nothing contentful; a tab loaded in the background has
/// not painted when it is brought forward. The file protocol's capture at
/// the end of every call makes the page paint; a step on the channel ends
/// the same way, so the next step's input reaches the page. The check reads
/// the page's own paint timing: a page that misreports it keeps only its
/// own input from arriving, as it could by ignoring that input.
async fn painted(client: &CdpClient, session: &str) {
    let paint = async {
        let read = client
            .send_command(
                "Runtime.evaluate",
                Some(
                    json!({ "expression": "performance.getEntriesByType('paint').length > 0",
                    "returnByValue": true }),
                ),
                Some(session),
            )
            .await;
        if !read.is_ok_and(|read| read["result"]["value"] == true) {
            let _ = client
                .send_command(
                    "Page.captureScreenshot",
                    Some(json!({ "format": "jpeg", "quality": 1,
                        "clip": { "x": 0, "y": 0, "width": 1, "height": 1, "scale": 1 } })),
                    Some(session),
                )
                .await;
        }
    };
    let _ = tokio::time::timeout(PAINT_WAIT, paint).await;
}

async fn next_event(
    events: Option<&mut broadcast::Receiver<CdpEvent>>,
) -> Result<CdpEvent, broadcast::error::RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

fn tab_ids(tabs: &[Value]) -> Vec<String> {
    tabs.iter()
        .filter_map(|tab| tab["tabId"].as_str().map(String::from))
        .collect()
}

/// An element's state after a step: its value (never a secret field's) and
/// the projection's checked, selected and expanded states.
async fn element_state(client: &CdpClient, node: &Node) -> Value {
    let mut state = json!({ "backendNodeId": node.backend_node_id });
    let read = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({ "objectId": node.object, "returnByValue": true,
                "functionDeclaration": format!(r#"function() {{
                    const facts = {FACTS}(this);
                    const tag = this.localName || '';
                    const valued = tag === 'input' || tag === 'textarea' || tag === 'select';
                    return {{ secret: facts.secret,
                        value: !facts.secret && valued ? String(this.value).slice(0, {units}) : null }};
                }}"#, units = 2 * MOST_VALUE_CHARS) })),
            Some(&node.session),
        )
        .await;
    if let Ok(read) = read {
        let read = &read["result"]["value"];
        let value = read["value"].as_str().filter(|_| read["secret"] == false);
        if let Some(value) = value {
            state["value"] = json!(target::bounded(value, MOST_VALUE_CHARS));
        }
    }
    if let Ok(ax) = target::accessibility(client, &node.session, node.backend_node_id).await {
        for name in ["checked", "selected", "expanded"] {
            if let Some(value) = ax.states.get(name) {
                state[name] = value.clone();
            }
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "S";
    const FRAME: &str = "F";

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.into(),
            params,
            session_id: Some(SESSION.into()),
        }
    }

    fn noted(events: &[CdpEvent]) -> Recorded {
        let mut recorded = Recorded::default();
        for event in events {
            recorded.note(event, (SESSION, FRAME), "7", Instant::now());
        }
        recorded
    }

    #[test]
    fn a_committed_document_is_the_navigation_with_its_response_status() {
        let recorded = noted(&[
            event(
                "Page.frameStartedNavigating",
                json!({ "frameId": FRAME, "navigationType": "differentDocument" }),
            ),
            event(
                "Network.responseReceived",
                json!({ "frameId": FRAME, "type": "Document", "loaderId": "L2",
                    "response": { "status": 404 } }),
            ),
            // A child frame's response and commit are not the page's.
            event(
                "Network.responseReceived",
                json!({ "frameId": "child", "type": "Document", "loaderId": "L9",
                    "response": { "status": 200 } }),
            ),
            event(
                "Page.frameNavigated",
                json!({ "frame": { "id": "child", "parentId": FRAME, "loaderId": "L9",
                    "url": "https://ads.example/" } }),
            ),
            event(
                "Page.frameNavigated",
                json!({ "frame": { "id": FRAME, "loaderId": "L2",
                    "url": "https://shop.example/missing" } }),
            ),
            event(
                "Page.navigatedWithinDocument",
                json!({ "frameId": FRAME, "url": "https://shop.example/missing#top" }),
            ),
        ]);
        assert!(!recorded.navigating);
        assert_eq!(
            recorded.navigation("L1"),
            Some(
                json!({ "kind": "document", "url": "https://shop.example/missing#top",
                "loaderId": "L2", "httpStatus": 404 })
            )
        );
        let within = noted(&[event(
            "Page.navigatedWithinDocument",
            json!({ "frameId": FRAME, "url": "https://shop.example/#reviews" }),
        )]);
        assert_eq!(
            within.navigation("L1"),
            Some(
                json!({ "kind": "same_document", "url": "https://shop.example/#reviews",
                "loaderId": "L1", "httpStatus": null })
            )
        );
        assert_eq!(noted(&[]).navigation("L1"), None);
        // Within a document whose loader is not known, as when the step
        // began between documents, nothing is reported.
        assert_eq!(within.navigation(""), None);
    }

    #[test]
    fn a_navigation_is_awaited_from_its_start_until_it_commits_or_stops() {
        for (start, navigating) in [
            (
                json!({ "frameId": FRAME, "navigationType": "differentDocument" }),
                true,
            ),
            (
                json!({ "frameId": FRAME, "navigationType": "sameDocument" }),
                false,
            ),
            (
                json!({ "frameId": "child", "navigationType": "differentDocument" }),
                false,
            ),
        ] {
            let recorded = noted(&[event("Page.frameStartedNavigating", start.clone())]);
            assert_eq!(recorded.navigating, navigating, "{start}");
        }
        // A 204, a download and a stop all end the main frame's loading
        // without a commit.
        let stopped = noted(&[
            event(
                "Page.frameStartedNavigating",
                json!({ "frameId": FRAME, "navigationType": "differentDocument" }),
            ),
            event("Page.frameStoppedLoading", json!({ "frameId": FRAME })),
        ]);
        assert!(!stopped.navigating);
        assert_eq!(stopped.navigation("L1"), None);
    }

    #[test]
    fn only_this_steps_watcher_counts_changes_and_a_dialog_and_input_are_noted() {
        let recorded = noted(&[
            event(
                "Runtime.bindingCalled",
                json!({ "name": BINDING, "payload": "7 12" }),
            ),
            event(
                "Runtime.bindingCalled",
                json!({ "name": BINDING, "payload": "7 0" }),
            ),
            // Another step's watcher, and a binding of the same name on
            // another page, are not this step's.
            event(
                "Runtime.bindingCalled",
                json!({ "name": BINDING, "payload": "8 40" }),
            ),
            CdpEvent {
                method: "Runtime.bindingCalled".into(),
                params: json!({ "name": BINDING, "payload": "7 40" }),
                session_id: Some("other".into()),
            },
            event(
                "Page.javascriptDialogOpening",
                json!({ "type": "confirm", "message": "Delete?", "url": "https://x/" }),
            ),
            CdpEvent {
                method: crate::native::activity::EVENT.into(),
                params: json!({ "source": "human", "ts": 1 }),
                session_id: Some(SESSION.into()),
            },
        ]);
        assert_eq!(recorded.changes, 12);
        assert!(recorded.activity.is_some());
        assert_eq!(
            recorded.dialog,
            Some(json!({ "type": "confirm", "message": "Delete?" }))
        );
        assert_eq!(recorded.input, None, "a person's input is not the step's");
        let agent = noted(&[CdpEvent {
            method: crate::native::activity::EVENT.into(),
            params: json!({ "source": "agent", "ts": crate::native::stream::monotonic_us() }),
            session_id: Some("any page".into()),
        }]);
        assert!(agent.input.is_some());
    }

    #[test]
    fn the_page_is_quiet_after_the_last_input_within_its_limit() {
        let armed = Instant::now();
        let ms = |millis: u64| Duration::from_millis(millis);
        // Activity before the last input does not count.
        let typed = Recorded {
            input: Some(armed + ms(300)),
            activity: Some(armed + ms(250)),
            ..Recorded::default()
        };
        assert_eq!(typed.quiet_until(armed), Some(armed + ms(350)));
        // Activity after it moves quiet on, never past the limit.
        let busy = Recorded {
            input: Some(armed + ms(10)),
            activity: Some(armed + ms(380)),
            ..Recorded::default()
        };
        assert_eq!(busy.quiet_until(armed), Some(armed + ms(410)));
        let endless = Recorded {
            input: Some(armed + ms(10)),
            activity: Some(armed + ms(900)),
            ..Recorded::default()
        };
        assert_eq!(
            endless.quiet_until(armed),
            Some(armed + ms(10) + SETTLE_LIMIT)
        );
        // A step without input that changed its page is quiet from its start.
        let scripted = Recorded {
            activity: Some(armed + ms(20)),
            ..Recorded::default()
        };
        assert_eq!(scripted.quiet_until(armed), Some(armed + ms(70)));
        // Nothing to wait for: no input and no change, or a new document.
        assert_eq!(Recorded::default().quiet_until(armed), None);
        let replaced = Recorded {
            input: Some(armed + ms(10)),
            committed: Some("L2".into()),
            ..Recorded::default()
        };
        assert_eq!(replaced.quiet_until(armed), None);
    }

    #[test]
    fn a_refused_step_has_no_landing_and_any_other_outcome_has_one() {
        assert!(lands(&json!({ "success": true })));
        assert!(lands(
            &json!({ "success": false, "code": "browser_operation_interrupted" })
        ));
        assert!(lands(
            &json!({ "success": false, "code": "command_outcome_unknown" })
        ));
        assert!(lands(&json!({ "success": false, "error": "Timed out" })));
        for code in REFUSALS {
            assert!(!lands(&json!({ "success": false, "code": code })), "{code}");
        }
    }
}
