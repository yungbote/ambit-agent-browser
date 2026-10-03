use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use super::types::{CdpCommand, CdpEvent, CdpMessage};
use crate::native::activity::{self, ActivityObservation, InputSource};

/// Confirmed page emulation shared by the native and operation connections.
/// Outstanding or unobserved changes are unknown, never an enabled default.
#[derive(Default)]
struct ScriptState {
    disabled: Option<bool>,
    issued: u64,
    confirmed: u64,
    pending: HashMap<u64, ScriptPending>,
    sessions: HashMap<String, (u64, bool)>,
}

type ScriptPages = Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<ScriptState>>>>>;
type ScriptStates = Arc<std::sync::RwLock<ScriptPages>>;

struct ScriptPending {
    session: String,
    targets: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

struct ScriptChange {
    state: Arc<std::sync::Mutex<ScriptState>>,
    targets: Arc<std::sync::Mutex<HashMap<String, String>>>,
    session: String,
    target: String,
    order: u64,
    disabled: bool,
    previous_actor: Option<bool>,
    previous_page: Option<bool>,
}

impl ScriptChange {
    fn complete(self, accepted: bool) {
        let current = self
            .targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.session)
            == Some(&self.target);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.remove(&self.order);
        if accepted && current {
            if state
                .sessions
                .get(&self.session)
                .is_none_or(|(order, _)| *order < self.order)
            {
                state
                    .sessions
                    .insert(self.session, (self.order, self.disabled));
            }
            if self.order > state.confirmed {
                state.confirmed = self.order;
                // Blink's equal-value setter is a no-op on this attachment.
                // It cannot prove another attachment's shared page override.
                state.disabled = match self.previous_actor {
                    Some(previous) if previous != self.disabled => Some(self.disabled),
                    Some(_) => self.previous_page,
                    None => None,
                };
            }
        }
    }
}

fn script_target(states: &ScriptStates, target: &str) -> Arc<std::sync::Mutex<ScriptState>> {
    states
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(target.into())
        .or_default()
        .clone()
}

fn script_detached(states: &ScriptStates, session: &str, target: &str) {
    let entry = script_target(states, target);
    let mut state = entry.lock().unwrap_or_else(|e| e.into_inner());
    // Blink disables this session's emulation during detach. A true override
    // or an unsettled command can change the shared page state without a reply.
    if state
        .sessions
        .remove(session)
        .is_some_and(|(_, disabled)| disabled)
        || state
            .pending
            .values()
            .any(|pending| pending.session == session)
    {
        state.disabled = None;
        state.confirmed = state.issued;
    }
}

fn script_attached(states: &ScriptStates, session: &str, target: &str) {
    let entry = script_target(states, target);
    let mut state = entry.lock().unwrap_or_else(|e| e.into_inner());
    let order = state.issued;
    state
        .sessions
        .entry(session.into())
        .or_insert((order, false));
}

fn script_seed_enabled(states: &ScriptStates, target: &str) {
    let entry = script_target(states, target);
    let mut state = entry.lock().unwrap_or_else(|e| e.into_inner());
    if state.issued == 0 {
        state.disabled = Some(false);
    }
}

fn script_unobserved(
    states: &ScriptStates,
    targets: &Arc<std::sync::Mutex<HashMap<String, String>>>,
    body: &str,
) {
    let Ok(command) = serde_json::from_str::<Value>(body) else {
        return;
    };
    if !matches!(
        command["method"].as_str(),
        Some("Emulation.setScriptExecutionDisabled" | "Emulation.disable")
    ) {
        return;
    }
    let target = command["sessionId"].as_str().and_then(|session| {
        targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .cloned()
    });
    let pages = states.read().unwrap_or_else(|e| e.into_inner()).clone();
    let pages = pages.lock().unwrap_or_else(|e| e.into_inner());
    for (_, entry) in pages
        .iter()
        .filter(|(id, _)| target.as_ref().is_none_or(|target| *id == target))
    {
        let mut state = entry.lock().unwrap_or_else(|e| e.into_inner());
        state.issued += 1;
        state.confirmed = state.issued;
        state.disabled = None;
        if let Some(session) = command["sessionId"].as_str().filter(|_| target.is_some()) {
            state.sessions.remove(session);
        } else {
            state.sessions.clear();
        }
    }
}

struct PendingResponse {
    created_target: bool,
    attached_target: Option<String>,
    detached_session: Option<String>,
    sender: oneshot::Sender<CdpMessage>,
    response: Option<CdpMessage>,
    activity: Option<ActivityObservation>,
    reset_page: Option<String>,
    script_change: Option<ScriptChange>,
    _pointer_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    native_pointer: Arc<std::sync::Mutex<Option<activity::NativePointer>>>,
}

impl PendingResponse {
    fn complete(
        self,
        response: CdpMessage,
        pages: &PageGenerations,
        events: &broadcast::Sender<CdpEvent>,
    ) {
        if let Some(change) = self.script_change {
            change.complete(response.error.is_none());
        }
        if response.error.is_none() {
            if let Some(session) = self.reset_page.as_deref() {
                reset_page(pages, events, session);
            }
            if let Some(observation) = self.activity {
                *self.native_pointer.lock().unwrap() = observation.native_pointer();
                observation.acknowledged();
            }
        } else if let Some(observation) = self.activity {
            observation.refused();
        }
        let _ = self.sender.send(response);
    }
}

type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;
#[derive(Clone, PartialEq, Eq, Hash)]
enum GenerationKey {
    Page(String),
    Frame(String, String),
}
type PageGenerations = Arc<std::sync::Mutex<HashMap<GenerationKey, String>>>;

/// A pointer realm armed without a command: the session it was armed in and
/// where its first trusted pointer event's report goes.
struct ProbeSlot {
    session: String,
    report: oneshot::Sender<Value>,
}

type PointerProbes = Arc<std::sync::Mutex<HashMap<u64, ProbeSlot>>>;

/// The first trusted pointer event of one armed realm: native input, not
/// this connection, produces it (`CdpClient::arm_pointer_probe`).
pub(crate) struct PointerProbe {
    token: u64,
    context: i64,
    generation: String,
    report: oneshot::Receiver<Value>,
    probes: PointerProbes,
    /// One armed token per connection at a time, as for pointer commands.
    _order: tokio::sync::OwnedMutexGuard<()>,
}

impl PointerProbe {
    /// Where the first trusted pointer event after arming landed, in page
    /// and screen coordinates, if one arrives within `wait`.
    pub(crate) async fn measured(
        mut self,
        wait: std::time::Duration,
    ) -> Option<activity::NativePointer> {
        let payload = tokio::time::timeout(wait, &mut self.report)
            .await
            .ok()?
            .ok()?;
        let coordinate = |field: &str, limit: f64| {
            payload[field]
                .as_f64()
                .filter(|value| value.is_finite() && value.abs() <= limit)
        };
        let geometry = payload["geometry"].clone();
        geometry["scale"]
            .as_f64()
            .filter(|scale| scale.is_finite() && *scale > 0.0)?;
        Some(activity::NativePointer {
            context: self.context,
            page_generation: self.generation.clone(),
            client_x: coordinate("clientX", 1e6)?,
            client_y: coordinate("clientY", 1e6)?,
            screen_x: coordinate("screenX", 32768.0)?,
            screen_y: coordinate("screenY", 32768.0)?,
            geometry,
            source_page: None,
        })
    }
}

impl Drop for PointerProbe {
    fn drop(&mut self) {
        self.probes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.token);
    }
}

/// Out-of-process frame sessions, each with the session it hangs under.
type FramePages = Arc<std::sync::Mutex<HashMap<String, String>>>;

/// The page session an out-of-process frame's session shows in, walking up
/// through nested frames; `None` for a session that is no frame.
fn frame_page(frames: &FramePages, session: &str) -> Option<String> {
    let frames = frames.lock().unwrap_or_else(|error| error.into_inner());
    let mut page = frames.get(session)?;
    // Frames nest only a few deep; the bound guards a malformed chain.
    for _ in 0..16 {
        match frames.get(page) {
            Some(parent) => page = parent,
            None => break,
        }
    }
    Some(page.clone())
}

/// Whether viewers of a frame's page can place an activity: it carries
/// screen coordinates, as every native sample does, or no point at all
/// (typing, scrolling). A point in the frame's own CSS pixels cannot be
/// placed on its page.
fn placeable_on_page(activity: &Value) -> bool {
    activity.get("screenX").is_some()
        || (activity.get("x").is_none() && activity.get("y").is_none())
}

fn page_generation(pages: &PageGenerations, session: &str) -> String {
    pages
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(GenerationKey::Page(session.into()))
        .or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

fn reset_page(pages: &PageGenerations, sender: &broadcast::Sender<CdpEvent>, session: &str) {
    let generation = uuid::Uuid::new_v4().to_string();
    pages
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(GenerationKey::Page(session.into()), generation.clone());
    let _ = sender.send(activity::reset(session, &generation));
}

/// Sessions whose events go to one private receiver instead of the broadcast.
type PrivateSessions = Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<CdpEvent>>>>;

/// Events buffered for a private session receiver that has fallen behind.
/// Newer events are dropped once it is full; a screencast consumer that
/// cannot keep up should lose frames, not stall the reader.
const PRIVATE_SESSION_BUFFER: usize = 16;

/// Interval between WebSocket ping frames sent to keep the connection alive
/// through intermediate proxies (reverse proxies, load balancers, service meshes).
const WS_KEEPALIVE_INTERVAL_SECS: u64 = 30;

/// Chrome serializes a lone UTF-16 surrogate in page-controlled text (a
/// title, an accessible name, a console argument) as a `\uD800`-style JSON
/// escape, and so does Node's `JSON.stringify` in a Playwright program's
/// result record. That is not a Unicode scalar value, so serde_json refuses
/// the whole message: the command awaiting a reply would time out, and an
/// event would go unseen. Every lone surrogate escape becomes U+FFFD before
/// the message is parsed; a surrogate pair is kept. JSON has no backslash
/// outside a string, so every backslash starts an escape, and a two-character
/// escape (`\\`, `\"`) is stepped over whole so its second character is never
/// read as the start of another. A message without a lone surrogate is
/// returned as it came, without a copy.
pub(crate) fn replace_lone_surrogate_escapes(text: String) -> String {
    let mut replaced = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find('\\') {
        let escape = cursor + offset;
        cursor = match hex_escape(&text, escape) {
            Some(0xD800..=0xDBFF)
                if hex_escape(&text, escape + 6)
                    .is_some_and(|low| (0xDC00..=0xDFFF).contains(&low)) =>
            {
                escape + 12
            }
            Some(0xD800..=0xDFFF) => {
                replaced.push_str(&text[copied..escape]);
                replaced.push_str("\\uFFFD");
                copied = escape + 6;
                copied
            }
            Some(_) => escape + 6,
            None => escape + 1 + text[escape + 1..].chars().next().map_or(0, char::len_utf8),
        };
    }
    if copied == 0 {
        return text;
    }
    replaced.push_str(&text[copied..]);
    replaced
}

/// The code unit of a `\uXXXX` escape at byte `at` of `text`.
fn hex_escape(text: &str, at: usize) -> Option<u16> {
    let digits = text.get(at..at + 6)?.strip_prefix("\\u")?;
    digits
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit())
        .then(|| u16::from_str_radix(digits, 16).ok())
        .flatten()
}

fn normalize_websocket_root_path(url: &str) -> String {
    let Some(scheme_end) = url.find("://").map(|index| index + 3) else {
        return url.to_string();
    };
    let authority = &url[scheme_end..];
    let Some(query_offset) = authority.find('?') else {
        return url.to_string();
    };
    if authority[..query_offset].contains('/') {
        return url.to_string();
    }
    let query_index = scheme_end + query_offset;
    format!("{}/{}", &url[..query_index], &url[query_index..])
}

/// Raw incoming CDP message (text) broadcast to all subscribers.
/// Used by the inspect proxy to forward responses and events to DevTools.
#[derive(Debug, Clone)]
pub struct RawCdpMessage {
    pub text: String,
    pub session_id: Option<String>,
}

pub struct CdpClient {
    site_profile: Arc<std::sync::RwLock<Arc<super::chrome::SiteProfileDisposition>>>,
    debugger_endpoint: Option<url::Url>,
    site_context: std::sync::RwLock<crate::native::site_sessions::Context>,
    site_custody:
        std::sync::RwLock<std::sync::Weak<crate::native::site_sessions::custody::Custody>>,
    ws_tx: Arc<
        Mutex<
            futures_util::stream::SplitSink<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
                Message,
            >,
        >,
    >,
    next_id: AtomicU64,
    pending: PendingMap,
    page_generations: PageGenerations,
    pub(crate) presentations: Arc<super::presentation::Presentations>,
    event_tx: broadcast::Sender<CdpEvent>,
    pub(crate) downloads: Arc<super::super::downloads::Downloads>,
    pub(crate) files: Arc<super::super::browser_files::FileDestinations>,
    error_pages: Arc<super::super::error_pages::ErrorPages>,
    raw_tx: broadcast::Sender<RawCdpMessage>,
    private_sessions: PrivateSessions,
    target_sessions: Arc<std::sync::Mutex<HashMap<String, String>>>,
    script_states: ScriptStates,
    /// Out-of-process frame sessions and the session each hangs under.
    frame_pages: FramePages,
    native_pointer_enabled: Arc<AtomicBool>,
    native_pointer_lock: Arc<Mutex<()>>,
    pointer_probes: PointerProbes,
    activity_owner: std::sync::OnceLock<ActivityOwner>,
    /// Becomes true, or loses its sender, once the reader has stopped.
    closed: tokio::sync::watch::Receiver<bool>,
    _reader_handle: tokio::task::JoinHandle<()>,
    _keepalive_handle: tokio::task::JoinHandle<()>,
}

/// The connection whose pages viewers observe. Another connection to the
/// same browser publishes acknowledged input as this owner's session for the
/// same target, with the owner's page generation at dispatch.
#[derive(Clone)]
struct ActivityOwner {
    events: broadcast::Sender<CdpEvent>,
    targets: Arc<std::sync::Mutex<HashMap<String, String>>>,
    pages: PageGenerations,
    frames: FramePages,
}

impl ActivityOwner {
    /// The owner's session and page generation for `target`: for an
    /// activity viewers can place, its frame's page.
    fn publication(&self, target: &str, placeable: bool) -> Option<(String, String)> {
        let session = self
            .targets
            .lock()
            .unwrap()
            .iter()
            .find_map(|(session, observed)| (observed == target).then(|| session.clone()))?;
        let session = placeable
            .then(|| frame_page(&self.frames, &session))
            .flatten()
            .unwrap_or(session);
        Some((page_generation(&self.pages, &session), session))
    }
}

/// How long an acknowledged pointer command waits for its trusted renderer
/// event before completing without a native measurement. The event is emitted
/// by the renderer while it handles the input, so it normally precedes the
/// acknowledgment; the window covers a loaded browser process reordering the
/// two channels, not a point the page never reports.
const NATIVE_POINTER_JOIN_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// Removes a pending entry if `send_command` is cancelled mid-await (e.g. an
/// outer timeout on the liveness probe), so a command whose response never
/// comes can't leak until the connection closes (#1528). Normal exits disarm
/// it via `done`.
struct PendingGuard {
    pending: PendingMap,
    id: u64,
    done: bool,
}

/// An enqueued command whose browser acknowledgment remains observable.
/// Stream input retains these receipts while continuing to enqueue events, so
/// taking control can drain prior input without adding a round trip per move.
pub(crate) struct PendingCommand {
    response: oneshot::Receiver<CdpMessage>,
    guard: PendingGuard,
    native_pointer: Arc<std::sync::Mutex<Option<activity::NativePointer>>>,
}

impl PendingCommand {
    pub(crate) fn native_pointer(&self) -> Option<activity::NativePointer> {
        self.native_pointer.lock().unwrap().clone()
    }
    pub(crate) async fn acknowledgment(&mut self) -> Result<CdpMessage, String> {
        let result = (&mut self.response)
            .await
            .map_err(|_| "CDP response channel closed".to_string());
        self.guard.done = true;
        result
    }

    pub(crate) fn try_acknowledgment(&mut self) -> Result<Option<CdpMessage>, String> {
        match self.response.try_recv() {
            Ok(response) => {
                self.guard.done = true;
                Ok(Some(response))
            }
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(oneshot::error::TryRecvError::Closed) => {
                self.guard.done = true;
                Err("CDP response channel closed".to_string())
            }
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let pending = self.pending.clone();
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pending.lock().await.remove(&id);
            });
        }
    }
}

impl CdpClient {
    pub(crate) fn bind_site_profile(
        &self,
        disposition: Arc<super::chrome::SiteProfileDisposition>,
    ) {
        *self
            .site_profile
            .write()
            .unwrap_or_else(|error| error.into_inner()) = disposition;
    }
    pub(crate) fn site_profile(&self) -> Arc<super::chrome::SiteProfileDisposition> {
        self.site_profile
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// End a temporary attachment without closing its browser. Its owner first
    /// settles request tasks; aborting the read/keepalive loops then releases
    /// the connection when its final handle is dropped.
    pub(crate) fn disconnect(&self) {
        for (session, target) in self
            .target_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            script_detached(&self.script_states, session, target);
        }
        self._reader_handle.abort();
        self._keepalive_handle.abort();
    }

    /// Resolves once the reader has stopped: the browser closed or broke the
    /// WebSocket, or this attachment was disconnected. Replies and events
    /// received before that remain available to raw subscribers.
    pub(crate) async fn closed(&self) {
        let mut closed = self.closed.clone();
        while !*closed.borrow_and_update() {
            if closed.changed().await.is_err() {
                return;
            }
        }
    }

    pub async fn connect(url: &str) -> Result<Self, String> {
        Self::connect_with_headers(url, None).await
    }

    pub async fn connect_with_headers(
        url: &str,
        headers: Option<Vec<(String, String)>>,
    ) -> Result<Self, String> {
        let normalized_url = normalize_websocket_root_path(url);
        let mut request = normalized_url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("Invalid WebSocket URL: {}", e))?;

        if let Some(hdrs) = headers {
            let req_headers = request.headers_mut();
            for (key, value) in hdrs {
                if let (Ok(name), Ok(val)) = (
                    key.parse::<tokio_tungstenite::tungstenite::http::header::HeaderName>(),
                    value.parse::<tokio_tungstenite::tungstenite::http::header::HeaderValue>(),
                ) {
                    req_headers.insert(name, val);
                }
            }
        }

        let ws_config = WebSocketConfig {
            max_message_size: None,
            max_frame_size: None,
            ..Default::default()
        };

        let (ws_stream, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false)
                .await
                .map_err(|e| format!("CDP WebSocket connect failed: {}", e))?;

        crate::native::socket::tune_dialed(ws_stream.get_ref());

        let (ws_tx, mut ws_rx) = ws_stream.split();
        let ws_tx = Arc::new(Mutex::new(ws_tx));

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let downloads = Arc::new(super::super::downloads::Downloads::default());
        let downloads_reader = downloads.clone();
        let files = Arc::new(super::super::browser_files::FileDestinations::default());
        let files_reader = files.clone();
        let error_pages = Arc::new(super::super::error_pages::ErrorPages::default());
        let error_pages_reader = error_pages.clone();
        let (event_tx, _) = broadcast::channel(4096);
        let (raw_tx, _) = broadcast::channel(4096);

        let private_sessions: PrivateSessions = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let target_sessions = Arc::new(std::sync::Mutex::new(HashMap::<String, String>::new()));
        let targets_clone = target_sessions.clone();
        let script_states: ScriptStates = Arc::default();
        let scripts_reader = script_states.clone();
        let frame_pages: FramePages = Arc::default();
        let frames_clone = frame_pages.clone();
        let site_profile = Arc::new(std::sync::RwLock::new(Arc::<
            super::chrome::SiteProfileDisposition,
        >::default()));
        let sites_reader = site_profile.clone();

        let page_generations: PageGenerations = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let pages_clone = page_generations.clone();
        let pointer_probes: PointerProbes = Arc::default();
        let probes_clone = pointer_probes.clone();
        let pending_clone = pending.clone();
        let event_tx_clone = event_tx.clone();
        let raw_tx_clone = raw_tx.clone();
        let private_clone = private_sessions.clone();

        // Notify used to stop the keepalive task when the reader loop exits.
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let closed = cancel_rx.clone();

        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = ws_rx.next().await {
                // Accept both Text and Binary frames — remote CDP proxies
                // (e.g. Browserless) may send responses as Binary frames.
                let msg = match msg {
                    Ok(Message::Text(text)) => text,
                    // Decoded leniently, like any page text: a frame that is
                    // not UTF-8 is not dropped. A valid one is not copied.
                    Ok(Message::Binary(data)) => String::from_utf8(data).unwrap_or_else(|error| {
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    }),
                    Ok(Message::Close(frame)) => {
                        if std::env::var("AGENT_BROWSER_DEBUG").is_ok() {
                            let reason = frame
                                .as_ref()
                                .map(|f| format!("code={}, reason={}", f.code, f.reason))
                                .unwrap_or_else(|| "no frame".to_string());
                            let _ =
                                writeln!(std::io::stderr(), "[cdp] WebSocket Close: {}", reason);
                        }
                        break;
                    }
                    Ok(Message::Pong(_)) => continue,
                    Ok(_) => continue,
                    Err(e) => {
                        if std::env::var("AGENT_BROWSER_DEBUG").is_ok() {
                            let _ = writeln!(std::io::stderr(), "[cdp] WebSocket Error: {}", e);
                        }
                        break;
                    }
                };
                // Decoded leniently once, so neither consumer below drops a
                // message over a lone surrogate in page-controlled text.
                let msg = replace_lone_surrogate_escapes(msg);

                // Broadcast raw message for inspect proxy subscribers before typed parse,
                // so messages with negative IDs (used by the inspect proxy) are still delivered.
                if raw_tx_clone.receiver_count() > 0 {
                    let session_id = serde_json::from_str::<serde_json::Value>(&msg)
                        .ok()
                        .and_then(|v| v.get("sessionId")?.as_str().map(String::from));
                    let _ = raw_tx_clone.send(RawCdpMessage {
                        text: msg.clone(),
                        session_id,
                    });
                }

                let parsed: CdpMessage = match serde_json::from_str(&msg) {
                    Ok(m) => m,
                    // Expected for inspect proxy messages with negative IDs
                    // (CdpMessage.id is u64); handled via raw broadcast above.
                    Err(_) => continue,
                };

                if let Some(id) = parsed.id {
                    // Response to a command
                    let mut pending = pending_clone.lock().await;
                    if let Some(mut request) = pending.remove(&id) {
                        if parsed.error.is_none() {
                            if request.created_target {
                                if let Some(target) = parsed
                                    .result
                                    .as_ref()
                                    .and_then(|result| result["targetId"].as_str())
                                {
                                    script_seed_enabled(&scripts_reader, target);
                                    sites_reader
                                        .read()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .documents
                                        .created(target);
                                }
                            }
                            if let Some(session) = request.detached_session.as_ref() {
                                if let Some(target) = targets_clone
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .remove(session)
                                {
                                    script_detached(&scripts_reader, session, &target);
                                }
                                frames_clone
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .remove(session);
                            }
                            if let (Some(target), Some(session)) = (
                                request.attached_target.as_ref(),
                                parsed
                                    .result
                                    .as_ref()
                                    .and_then(|result| result["sessionId"].as_str()),
                            ) {
                                if let Some(previous) = targets_clone
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .get(session)
                                    .filter(|previous| *previous != target)
                                    .cloned()
                                {
                                    script_detached(&scripts_reader, session, &previous);
                                }
                                targets_clone
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .insert(session.into(), target.clone());
                                script_attached(&scripts_reader, session, target);
                            }
                        }
                        if parsed.error.is_none()
                            && request
                                .activity
                                .as_ref()
                                .is_some_and(ActivityObservation::awaits_native_event)
                        {
                            // Chrome may acknowledge input just before the
                            // trusted renderer event reaches this connection.
                            // Join both receipts in the existing command entry;
                            // retain the pointer-order guard until it settles.
                            request.response = Some(parsed);
                            pending.insert(id, request);
                            let pending = pending_clone.clone();
                            let pages = pages_clone.clone();
                            let events = event_tx_clone.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(NATIVE_POINTER_JOIN_WINDOW).await;
                                if let Some(mut request) = pending.lock().await.remove(&id) {
                                    // The renderer event did not arrive in time
                                    // (or never fires here, e.g. the point is
                                    // owned by an out-of-process frame). This
                                    // command completes without a native
                                    // measurement; its token can no longer be
                                    // matched, so a late event is dropped. The
                                    // next command measures afresh.
                                    let response = request.response.take().unwrap();
                                    request.complete(response, &pages, &events);
                                }
                            });
                        } else {
                            request.complete(parsed, &pages_clone, &event_tx_clone);
                        }
                    }
                } else if let Some(ref method) = parsed.method {
                    if method == "Target.attachedToTarget" {
                        if let Some(params) = parsed.params.as_ref() {
                            if let (Some(session), Some(target)) = (
                                params["sessionId"].as_str(),
                                params["targetInfo"]["targetId"].as_str(),
                            ) {
                                if let Some(previous) = targets_clone
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .get(session)
                                    .filter(|previous| previous.as_str() != target)
                                    .cloned()
                                {
                                    script_detached(&scripts_reader, session, &previous);
                                }
                                targets_clone
                                    .lock()
                                    .unwrap()
                                    .insert(session.into(), target.into());
                                script_attached(&scripts_reader, session, target);
                                // An out-of-process frame hangs under the
                                // session it was attached through.
                                if let (Some(parent), "iframe") = (
                                    parsed.session_id.as_deref(),
                                    params["targetInfo"]["type"].as_str().unwrap_or_default(),
                                ) {
                                    frames_clone
                                        .lock()
                                        .unwrap()
                                        .insert(session.into(), parent.into());
                                }
                            }
                        }
                    } else if method == "Target.detachedFromTarget" {
                        if let Some(session) = parsed
                            .params
                            .as_ref()
                            .and_then(|params| params["sessionId"].as_str())
                        {
                            if let Some(target) = targets_clone.lock().unwrap().remove(session) {
                                script_detached(&scripts_reader, session, &target);
                            }
                            frames_clone.lock().unwrap().remove(session);
                        }
                    } else if method == "Target.targetDestroyed" {
                        if let Some(target) = parsed
                            .params
                            .as_ref()
                            .and_then(|params| params["targetId"].as_str())
                        {
                            let mut targets = targets_clone
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            let removed = targets
                                .iter()
                                .filter(|(_, held)| held.as_str() == target)
                                .map(|(session, _)| session.clone())
                                .collect::<std::collections::HashSet<_>>();
                            targets.retain(|_, held| held.as_str() != target);
                            for session in &removed {
                                script_detached(&scripts_reader, session, target);
                            }
                            frames_clone
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .retain(|session, parent| {
                                    !removed.contains(session) && !removed.contains(parent)
                                });
                        }
                    }
                    if method == "Runtime.bindingCalled" {
                        if let (Some(params), Some(session)) =
                            (parsed.params.as_ref(), parsed.session_id.as_deref())
                        {
                            if params["name"] == activity::POINTER_BINDING {
                                if let Some(payload) = params["payload"]
                                    .as_str()
                                    .filter(|payload| payload.len() <= 1024)
                                {
                                    if let Ok(payload) = serde_json::from_str::<Value>(payload) {
                                        if let Some(token) = payload["token"]
                                            .as_str()
                                            .and_then(|token| token.parse::<u64>().ok())
                                        {
                                            let mut pending = pending_clone.lock().await;
                                            let complete = if let Some(request) =
                                                pending.get_mut(&token)
                                            {
                                                if let Some(observation) = request.activity.as_mut()
                                                {
                                                    observation.native_event(session, &payload);
                                                    request.response.is_some()
                                                        && !observation.awaits_native_event()
                                                } else {
                                                    false
                                                }
                                            } else {
                                                // A realm armed without a
                                                // command reports its first
                                                // trusted event, from its own
                                                // session only.
                                                let mut probes = probes_clone
                                                    .lock()
                                                    .unwrap_or_else(|error| error.into_inner());
                                                if probes
                                                    .get(&token)
                                                    .is_some_and(|slot| slot.session == session)
                                                {
                                                    let slot = probes.remove(&token).unwrap();
                                                    let _ = slot.report.send(payload.clone());
                                                }
                                                false
                                            };
                                            if complete {
                                                let mut request = pending.remove(&token).unwrap();
                                                let response = request.response.take().unwrap();
                                                request.complete(
                                                    response,
                                                    &pages_clone,
                                                    &event_tx_clone,
                                                );
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    files_reader.observe(
                        method,
                        parsed.params.as_ref().unwrap_or(&Value::Null),
                        parsed.session_id.as_deref(),
                    );
                    // Input admitted after this event sees the page it shows.
                    error_pages_reader.observe(
                        method,
                        parsed.params.as_ref().unwrap_or(&Value::Null),
                        parsed.session_id.as_deref(),
                    );
                    let page = parsed.session_id.as_deref().map(|session| {
                        frame_page(&frames_clone, session).unwrap_or_else(|| session.to_owned())
                    });
                    let target = page.as_ref().and_then(|page| {
                        targets_clone
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get(page)
                            .cloned()
                    });
                    sites_reader
                        .read()
                        .unwrap_or_else(|error| error.into_inner())
                        .documents
                        .observe(
                            method,
                            parsed.params.as_ref().unwrap_or(&Value::Null),
                            target.as_deref(),
                        );
                    // Retain download truth before broadcasting it to consumers.
                    downloads_reader
                        .observe(method, parsed.params.as_ref().unwrap_or(&Value::Null));
                    // Event
                    let mut event = CdpEvent {
                        method: method.clone(),
                        params: parsed.params.clone().unwrap_or(Value::Null),
                        session_id: parsed.session_id.clone(),
                    };
                    if let Some(session) = event.session_id.as_deref() {
                        if method == "Page.frameNavigated" {
                            if let Some(frame) = event.params["frame"]["id"].as_str() {
                                pages_clone
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .insert(
                                        GenerationKey::Frame(session.into(), frame.into()),
                                        uuid::Uuid::new_v4().to_string(),
                                    );
                            }
                        }
                        if method == "Page.frameNavigated"
                            && event.params["frame"]["parentId"]
                                .as_str()
                                .is_none_or(str::is_empty)
                        {
                            reset_page(&pages_clone, &event_tx_clone, session);
                        }
                        if method == "Page.screencastFrame" {
                            event.params[activity::FRAME_GENERATION] =
                                serde_json::json!(page_generation(&pages_clone, session));
                        }
                    }
                    if method == "Target.detachedFromTarget" {
                        if let Some(session) = event.params["sessionId"].as_str() {
                            pages_clone
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .retain(|key, _| match key {
                                    GenerationKey::Page(page) | GenerationKey::Frame(page, _) => {
                                        page != session
                                    }
                                });
                        }
                    }
                    let routed = event.session_id.as_deref().is_some_and(|sid| {
                        let mut routes = private_clone.lock().unwrap_or_else(|e| e.into_inner());
                        match routes.get(sid) {
                            Some(tx) => match tx.try_send(event.clone()) {
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    routes.remove(sid);
                                    false
                                }
                            },
                            None => false,
                        }
                    });
                    if !routed {
                        let _ = event_tx_clone.send(event);
                    }
                }
            }

            // Reader loop exited (connection closed or error). Drop all pending
            // command senders so callers get an immediate channel-closed error
            // instead of waiting for the 30-second timeout.
            downloads_reader.closed();
            files_reader.end();
            for (session, target) in targets_clone
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
            {
                script_detached(&scripts_reader, session, target);
            }
            pending_clone.lock().await.clear();

            // Stop the keepalive task — the connection is gone.
            let _ = cancel_tx.send(true);
        });

        // Spawn a keepalive task that sends WebSocket Ping frames at a regular
        // interval. This prevents intermediate proxies (Envoy, nginx, OpenResty,
        // cloud load balancers) from closing idle WebSocket connections. If the
        // send fails, the connection is dead and we stop pinging.
        let keepalive_tx = ws_tx.clone();
        let keepalive_handle = tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(WS_KEEPALIVE_INTERVAL_SECS);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = cancel_rx.changed() => break,
                }
                let mut tx = keepalive_tx.lock().await;
                if tx.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            site_profile,
            debugger_endpoint: url::Url::parse(&normalized_url).ok(),
            site_context: std::sync::RwLock::default(),
            site_custody: std::sync::RwLock::default(),
            ws_tx,
            next_id: AtomicU64::new(1),
            pending,
            page_generations,
            presentations: Arc::default(),
            event_tx,
            downloads,
            files,
            error_pages,
            raw_tx,
            private_sessions,
            target_sessions,
            script_states,
            frame_pages,
            native_pointer_enabled: Arc::new(AtomicBool::new(false)),
            native_pointer_lock: Arc::new(Mutex::new(())),
            pointer_probes,
            activity_owner: std::sync::OnceLock::new(),
            closed,
            _reader_handle: reader_handle,
            _keepalive_handle: keepalive_handle,
        })
    }

    pub async fn send_command(
        &self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        self.send_command_from(method, params, session_id, InputSource::Agent)
            .await
    }

    pub(crate) async fn send_command_from(
        &self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
        source: InputSource,
    ) -> Result<Value, String> {
        let mut pending = self
            .enqueue_command_from(method, params, session_id, source)
            .await?;
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(30), pending.acknowledgment())
                .await
                .map_err(|_| format!("CDP command timed out: {}", method))??;
        if let Some(error) = response.error {
            return Err(format!("CDP error ({}): {}", method, error));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    // The wrappers return the one enqueue future directly: every CDP command
    // awaits it, and an async wrapper would keep another copy of its arguments.
    pub(crate) fn enqueue_command<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        session_id: Option<&'a str>,
    ) -> impl std::future::Future<Output = Result<PendingCommand, String>> + 'a {
        self.enqueue_command_from(method, params, session_id, InputSource::Agent)
    }

    pub(crate) fn enqueue_command_from<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        session_id: Option<&'a str>,
        source: InputSource,
    ) -> impl std::future::Future<Output = Result<PendingCommand, String>> + 'a {
        self.enqueue_with_id(
            self.reserve_command_id(),
            method,
            params,
            session_id,
            source,
        )
    }

    /// Allocate the id of a command before sending it, so a raw subscriber
    /// can correlate its reply with no race against the reply itself.
    pub(crate) fn reserve_command_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Send one command under an id from [`reserve_command_id`]. Every other
    /// effect of this connection (page generations, pointer measurement for
    /// input) applies exactly as for [`enqueue_command`].
    pub(crate) fn enqueue_reserved_command<'a>(
        &'a self,
        id: u64,
        method: &'a str,
        params: Option<Value>,
        session_id: Option<&'a str>,
    ) -> impl std::future::Future<Output = Result<PendingCommand, String>> + 'a {
        self.enqueue_with_id(id, method, params, session_id, InputSource::Agent)
    }

    /// Both acknowledged and fire-and-forget native commands share the
    /// preparation boundary before anything is written to Chrome.
    async fn prepare_command<'a>(
        &self,
        method: &'a str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<(&'a str, Option<Value>), String> {
        let mut params = params;
        let custody = self
            .site_custody
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .upgrade();
        let private = session_id
            .and_then(|session| self.target_for_session(session))
            .is_some_and(|target| self.site_context().private_target(&target));
        let guarded = custody.is_some() && !private && self.site_context().files.guarded();
        let method = if method == "Fetch.disable" && guarded {
            params = Some(json!({"patterns":[]}));
            "Fetch.enable"
        } else {
            method
        };
        if method == "Fetch.enable" && guarded {
            let params = params.get_or_insert_with(|| json!({}));
            if params.get("patterns").is_none() {
                params["patterns"] = json!([]);
            }
            if let Some(patterns) = params["patterns"].as_array_mut() {
                for pattern in [
                    json!({"urlPattern":"*","resourceType":"Document","requestStage":"Request"}),
                    json!({"urlPattern":"file://*","requestStage":"Request"}),
                ] {
                    if !patterns.contains(&pattern) {
                        patterns.push(pattern);
                    }
                }
                if let Some(pattern) = self.debugger_pattern() {
                    if !patterns.contains(&pattern) {
                        patterns.push(pattern);
                    }
                }
            }
        }
        if matches!(method, "Page.navigate" | "Runtime.runIfWaitingForDebugger") {
            if let (Some(custody), Some(session)) = (custody, session_id) {
                Box::pin(custody.prepare_session(self, session))
                    .await
                    .map_err(str::to_owned)?;
            }
        }
        Ok((method, params))
    }

    // The observation is built here rather than passed in: an async fn keeps
    // a moved-in argument twice, and every CDP command awaits this future.
    async fn enqueue_with_id(
        &self,
        id: u64,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
        source: InputSource,
    ) -> Result<PendingCommand, String> {
        let (method, params) = self.prepare_command(method, params, session_id).await?;
        let mut observation = session_id.and_then(|session| {
            activity::from_command(method, params.as_ref()?).map(|value| {
                self.observe_activity(value, session, self.page_generation(session), source)
            })
        });
        let reset_page = (method == "Emulation.setDeviceMetricsOverride"
            || method == "Emulation.clearDeviceMetricsOverride")
            .then(|| session_id.map(String::from))
            .flatten();
        let pointer_guard = if method == "Input.dispatchMouseEvent"
            && self.native_pointer_enabled.load(Ordering::Acquire)
        {
            let guard = self.native_pointer_lock.clone().lock_owned().await;
            if self.native_pointer_enabled.load(Ordering::Acquire) {
                if let Some(session) = session_id {
                    // A preparation that fails (a frame mid-navigation, a
                    // slow renderer) costs this one command its measurement.
                    // It is retried by the next pointer command; attribution
                    // is never switched off for the browser's lifetime.
                    if let Some((context, frame)) =
                        Box::pin(self.prepare_native_pointer(session, id, params.as_ref())).await
                    {
                        if let Some(observation) = observation.as_mut() {
                            observation.track_native();
                            observation.set_native_context(context);
                            if let Some((source, geometry)) = frame {
                                observation.set_native_frame(source, geometry);
                            }
                        }
                    }
                }
            }
            Some(guard)
        } else {
            None
        };

        let cmd = CdpCommand {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.filter(|s| !s.is_empty()).map(|s| s.to_string()),
        };

        let json = serde_json::to_string(&cmd)
            .map_err(|e| format!("Failed to serialize CDP command: {}", e))?;

        let (tx, rx) = oneshot::channel();
        let native_pointer = Arc::new(std::sync::Mutex::new(None));

        // This lock fixes the command's order at the actual writer, rather
        // than at id reservation or an asynchronously reordered response.
        let mut ws_tx = self.ws_tx.lock().await;
        let script_change = self.script_change(method, cmd.params.as_ref(), session_id);
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                id,
                PendingResponse {
                    created_target: method == "Target.createTarget",
                    detached_session: (method == "Target.detachFromTarget")
                        .then(|| {
                            cmd.params
                                .as_ref()
                                .and_then(|params| params["sessionId"].as_str())
                                .map(str::to_owned)
                        })
                        .flatten(),
                    attached_target: (method == "Target.attachToTarget")
                        .then(|| {
                            cmd.params
                                .as_ref()
                                .and_then(|params| params["targetId"].as_str())
                                .map(str::to_owned)
                        })
                        .flatten(),
                    sender: tx,
                    response: None,
                    activity: observation,
                    reset_page,
                    script_change,
                    _pointer_guard: pointer_guard,
                    native_pointer: native_pointer.clone(),
                },
            );
        }

        // Cleans up the pending entry if this future is cancelled mid-await (#1528).
        let guard = PendingGuard {
            pending: self.pending.clone(),
            id,
            done: false,
        };

        {
            ws_tx
                .send(Message::Text(json))
                .await
                .map_err(|e| format!("Failed to send CDP command: {}", e))?;
        }

        Ok(PendingCommand {
            response: rx,
            guard,
            native_pointer,
        })
    }

    pub(crate) fn page_generation(&self, session: &str) -> String {
        page_generation(&self.page_generations, session)
    }

    pub(crate) fn document_generation(&self, session: &str, frame: Option<&str>) -> String {
        match frame {
            None => self.page_generation(session),
            Some(frame) => self
                .page_generations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entry(GenerationKey::Frame(session.into(), frame.into()))
                .or_insert_with(|| uuid::Uuid::new_v4().to_string())
                .clone(),
        }
    }

    /// The page an out-of-process frame's session shows in, or the session
    /// itself when it is no frame.
    pub(crate) fn page_of(&self, session: &str) -> String {
        frame_page(&self.frame_pages, session).unwrap_or_else(|| session.to_owned())
    }

    /// The error page that `session`'s page shows, as the reader kept it
    /// from the events it has passed on (`error_pages`).
    pub(crate) fn error_page(
        &self,
        session: &str,
    ) -> Option<crate::native::error_pages::ErrorPage> {
        self.error_pages.showing(&self.page_of(session))
    }

    /// The target a session is attached to: for an out-of-process frame,
    /// its frame id.
    pub(crate) fn target_for_session(&self, session: &str) -> Option<String> {
        self.target_sessions.lock().unwrap().get(session).cloned()
    }

    pub(crate) fn session_for_target(&self, target: &str) -> Option<String> {
        self.target_sessions
            .lock()
            .unwrap()
            .iter()
            .find_map(|(session, observed)| (observed == target).then(|| session.clone()))
    }

    fn script_change(
        &self,
        method: &str,
        params: Option<&Value>,
        session: Option<&str>,
    ) -> Option<ScriptChange> {
        let disabled = match method {
            "Emulation.setScriptExecutionDisabled" => params?["value"].as_bool()?,
            "Emulation.disable" => false,
            _ => return None,
        };
        let session = session?;
        let target = self.target_for_session(session)?;
        let entry = script_target(&self.script_states, &target);
        let (order, previous_actor, previous_page) = {
            let mut state = entry.lock().unwrap_or_else(|e| e.into_inner());
            let previous_actor = state
                .sessions
                .get(session)
                .and_then(|(confirmed, disabled)| {
                    (!state
                        .pending
                        .iter()
                        .any(|(order, pending)| pending.session == session && order > confirmed))
                    .then_some(*disabled)
                });
            let previous_page = (!state.pending.keys().any(|order| *order > state.confirmed))
                .then_some(state.disabled)
                .flatten();
            state.issued += 1;
            let order = state.issued;
            if state
                .pending
                .values()
                .any(|pending| !Arc::ptr_eq(&pending.targets, &self.target_sessions))
            {
                // Separate sockets have no shared browser execution order.
                // Overlapping overrides stay unknown until a later settled
                // command; native writer order must not invent that fact.
                state.confirmed = order;
                state.disabled = None;
            }
            state.pending.insert(
                order,
                ScriptPending {
                    session: session.into(),
                    targets: self.target_sessions.clone(),
                },
            );
            (order, previous_actor, previous_page)
        };
        Some(ScriptChange {
            state: entry,
            targets: self.target_sessions.clone(),
            session: session.into(),
            target,
            order,
            disabled,
            previous_actor,
            previous_page,
        })
    }

    /// Only the fresh native process/new paused target owner establishes the
    /// initial enabled baseline. Adoption/reconnect never invents it.
    pub(crate) fn seed_script_enabled(&self, session: &str) {
        if let Some(target) = self.target_for_session(session) {
            script_seed_enabled(&self.script_states, &target);
        }
    }

    pub(crate) fn script_execution_disabled(&self, session: &str) -> Option<bool> {
        let target = self.target_for_session(session)?;
        let entry = script_target(&self.script_states, &target);
        let state = entry.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending.keys().any(|order| *order > state.confirmed) {
            return None;
        }
        state.disabled
    }

    pub(crate) fn enable_window_pointer(&self) {
        self.native_pointer_enabled.store(true, Ordering::Release);
    }

    /// The pointer realm of `session`'s main frame: an isolated world the
    /// page's own scripts cannot reach. Returns its frame and context.
    async fn pointer_realm(&self, session: &str) -> Result<(String, i64), String> {
        let tree = self
            .send_command_no_params("Page.getFrameTree", Some(session))
            .await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or("Page frame is unavailable")?;
        let world = self
            .send_command(
                "Page.createIsolatedWorld",
                Some(serde_json::json!({
                    "frameId": frame, "worldName": activity::POINTER_WORLD,
                })),
                Some(session),
            )
            .await?;
        let context = world["executionContextId"]
            .as_i64()
            .ok_or("Pointer observation realm is unavailable")?;
        Ok((frame.into(), context))
    }

    /// Arms the pointer realm of `session`'s main frame: the first trusted
    /// pointer event there after this returns reports where it landed, in
    /// page and screen coordinates, to the probe. Nothing is dispatched;
    /// native input produces the event.
    pub(crate) async fn arm_pointer_probe(&self, session: &str) -> Result<PointerProbe, String> {
        let order = self.native_pointer_lock.clone().lock_owned().await;
        let token = self.reserve_command_id();
        let (report_to, report) = oneshot::channel();
        self.pointer_probes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                token,
                ProbeSlot {
                    session: session.into(),
                    report: report_to,
                },
            );
        let mut probe = PointerProbe {
            token,
            context: 0,
            generation: self.page_generation(session),
            report,
            probes: self.pointer_probes.clone(),
            _order: order,
        };
        probe.context = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let (_, context) = self.pointer_realm(session).await?;
            if self.listen_for_pointer(session, context, token).await? {
                Ok(context)
            } else {
                Err("The page's pointer realm could not be armed.".to_string())
            }
        })
        .await
        .map_err(|_| "The page's pointer realm did not answer in time.".to_string())??;
        Ok(probe)
    }

    /// Reports each trusted pointer event of `context` (in `session`) with
    /// `token`. The listener survives a document replacement only by being
    /// installed again; installing it again rebinds the same handlers.
    async fn listen_for_pointer(
        &self,
        session: &str,
        context: i64,
        token: u64,
    ) -> Result<bool, String> {
        self.send_command(
            "Runtime.addBinding",
            Some(serde_json::json!({
                "name": activity::POINTER_BINDING, "executionContextName": activity::POINTER_WORLD,
            })),
            Some(session),
        )
        .await?;
        let token = serde_json::to_string(&token.to_string()).map_err(|error| error.to_string())?;
        let expression = format!(
            r#"(() => {{
            globalThis.__ambitPointerToken = {token};
            if (typeof globalThis.__ambitWindowPointer !== 'function') return false;
            const handlers = globalThis.__ambitPointerHandlers ||= Object.create(null);
            for (const [name, eventType] of [['pointermove','move'],['pointerdown','press'],['pointerup','release'],['wheel','scroll']]) {{
                const handler = handlers[name] ||= event => {{
                    if (!event.isTrusted) return;
                    globalThis.__ambitWindowPointer(JSON.stringify({{
                        token: globalThis.__ambitPointerToken, eventType,
                        clientX: event.clientX, clientY: event.clientY,
                        screenX: event.screenX, screenY: event.screenY,
                        geometry: {{scale:devicePixelRatio*(visualViewport?.scale??1),width:innerWidth,height:innerHeight,offsetX:visualViewport?.offsetLeft??0,offsetY:visualViewport?.offsetTop??0}},
                    }}));
                }};
                // Document replacement can discard listeners while this
                // isolated realm survives. Rebind the same function;
                // a remembered installation flag is not observation.
                removeEventListener(name, handler, true);
                addEventListener(name, handler, {{capture:true, passive:true}});
            }}
            return true;
        }})()"#
        );
        let value = self
            .send_command(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": expression, "contextId": context, "returnByValue": true,
                })),
                Some(session),
            )
            .await?;
        Ok(value["result"]["value"] == true)
    }

    async fn prepare_native_pointer(
        &self,
        session: &str,
        token: u64,
        params: Option<&Value>,
    ) -> Option<(i64, Option<(activity::NativePointerFrame, Value)>)> {
        let prepare = async {
            let (frame, context) = self.pointer_realm(session).await?;
            let params = params.ok_or("Pointer coordinates are unavailable")?;
            let source = super::pointer::locate(
                self,
                session,
                &frame,
                context,
                params["x"].as_f64().ok_or("Pointer x is unavailable")?,
                params["y"].as_f64().ok_or("Pointer y is unavailable")?,
            )
            .await?;
            let source_session = source.session;
            let source_context = source.context;
            let frame_observation = if source_session != session || source_context != context {
                let geometry = self.send_command("Runtime.evaluate", Some(serde_json::json!({
                    "expression": "({scale:devicePixelRatio*(visualViewport?.scale??1),width:innerWidth,height:innerHeight,offsetX:visualViewport?.offsetLeft??0,offsetY:visualViewport?.offsetTop??0})",
                    "contextId": context, "returnByValue": true,
                })), Some(session)).await?["result"]["value"].clone();
                Some((
                    activity::NativePointerFrame {
                        session: source_session.clone(),
                        context: source_context,
                        generation: self.page_generation(&source_session),
                        x: source.x,
                        y: source.y,
                    },
                    geometry,
                ))
            } else {
                None
            };
            let listening = self
                .listen_for_pointer(&source_session, source_context, token)
                .await?;
            Ok::<_, String>(listening.then_some((context, frame_observation)))
        };
        tokio::time::timeout(std::time::Duration::from_millis(500), prepare)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub(crate) fn rotate_page_generation(&self, session: &str) {
        reset_page(&self.page_generations, &self.event_tx, session);
    }

    pub(crate) fn rotate_all_page_generations(&self) {
        let sessions: Vec<_> = self
            .page_generations
            .lock()
            .unwrap()
            .keys()
            .filter_map(|key| match key {
                GenerationKey::Page(session) => Some(session.clone()),
                GenerationKey::Frame(_, _) => None,
            })
            .collect();
        for session in sessions {
            self.rotate_page_generation(&session);
        }
    }

    /// Observes input to publish once it is acknowledged. Viewers watch a
    /// page, not its out-of-process frames: an activity in a frame's session
    /// that they can place (`placeable_on_page`) is published as its page's,
    /// with the page's generation.
    pub(crate) fn observe_activity(
        &self,
        value: Value,
        session: &str,
        generation: String,
        source: InputSource,
    ) -> ActivityObservation {
        let placeable = placeable_on_page(&value);
        let (page, generation) = match placeable
            .then(|| frame_page(&self.frame_pages, session))
            .flatten()
        {
            Some(page) => {
                let generation = self.page_generation(&page);
                (page, generation)
            }
            None => (session.to_owned(), generation),
        };
        let observation =
            ActivityObservation::new(value, &page, generation, source, self.event_tx.clone());
        let Some(owner) = self.activity_owner.get() else {
            return observation;
        };
        let target = self.target_sessions.lock().unwrap().get(session).cloned();
        match target.and_then(|target| owner.publication(&target, placeable)) {
            Some((generation, session)) => {
                observation.published_as(owner.events.clone(), session, generation)
            }
            // The owner has no view of this page; nobody observes it there.
            None => observation,
        }
    }

    /// Publish this connection's acknowledged input as `owner`'s, for
    /// targets the owner is attached to. Set once, before any input.
    pub(crate) fn publish_activity_as(&self, owner: &CdpClient) {
        self.bind_site_profile(owner.site_profile());
        *self
            .script_states
            .write()
            .unwrap_or_else(|e| e.into_inner()) = owner
            .script_states
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        *self
            .site_context
            .write()
            .unwrap_or_else(|error| error.into_inner()) = owner.site_context();
        let _ = self.activity_owner.set(ActivityOwner {
            events: owner.event_tx.clone(),
            targets: owner.target_sessions.clone(),
            pages: owner.page_generations.clone(),
            frames: owner.frame_pages.clone(),
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.event_tx.subscribe()
    }

    pub(crate) fn site_context(&self) -> crate::native::site_sessions::Context {
        self.site_context
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The debugger's HTTP endpoints can mutate Chrome too. This matches
    /// this connection's real host/port and normalized loopback aliases,
    /// never every local service or a port retained from an earlier Chrome.
    pub(crate) fn debugger_resource(&self, value: &str) -> bool {
        let Some(endpoint) = &self.debugger_endpoint else {
            return false;
        };
        let mut value = value.trim_start();
        while value
            .get(..12)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("view-source:"))
        {
            value = value[12..].trim_start();
        }
        let Ok(candidate) = url::Url::parse(value) else {
            return false;
        };
        if !matches!(candidate.scheme(), "http" | "https" | "ws" | "wss")
            || candidate.port_or_known_default() != endpoint.port_or_known_default()
        {
            return false;
        }
        let local = |host: Option<url::Host<&str>>| match host {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
            Some(url::Host::Domain(host)) => {
                let host = host.trim_end_matches('.');
                host.eq_ignore_ascii_case("localhost")
                    || host.ends_with(".localhost")
                    || host.eq_ignore_ascii_case("localhost.localdomain")
            }
            None => false,
        };
        candidate.host() == endpoint.host() || (local(candidate.host()) && local(endpoint.host()))
    }

    pub(crate) fn debugger_pattern(&self) -> Option<Value> {
        let port = self.debugger_endpoint.as_ref()?.port_or_known_default()?;
        Some(json!({"urlPattern":format!("*://*:{port}/*"),"requestStage":"Request"}))
    }

    pub(crate) async fn site_request_ready(
        self: &Arc<Self>,
        session: String,
        params: Value,
    ) -> bool {
        let custody = self
            .site_custody
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .upgrade();
        if let Some(custody) = custody {
            Box::pin(custody.paused(self, session, params)).await
        } else {
            true
        }
    }

    pub(crate) fn site_preparation_required(&self) -> bool {
        self.site_custody
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .strong_count()
            > 0
    }

    pub(crate) async fn enable_browser_auto_attach(&self) -> Result<(), String> {
        self.send_command(
            "Target.setAutoAttach",
            Some(json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true})),
            None,
        )
        .await?;
        Ok(())
    }

    pub(crate) fn set_site_custody(
        &self,
        owner: std::sync::Weak<crate::native::site_sessions::custody::Custody>,
        context: crate::native::site_sessions::Context,
    ) {
        *self
            .site_custody
            .write()
            .unwrap_or_else(|error| error.into_inner()) = owner;
        *self
            .site_context
            .write()
            .unwrap_or_else(|error| error.into_inner()) = context;
    }

    /// Deliver every event carrying `session_id` to the returned receiver
    /// instead of the broadcast, so a high-volume session (a screencast) is
    /// neither cloned to every subscriber nor able to overflow their buffers.
    /// Events without a session id are unaffected. Call
    /// [`unsubscribe_session`](Self::unsubscribe_session) when done; a dropped
    /// receiver also ends the route on the next event for that session.
    pub fn subscribe_session(&self, session_id: &str) -> mpsc::Receiver<CdpEvent> {
        let (tx, rx) = mpsc::channel(PRIVATE_SESSION_BUFFER);
        self.private_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), tx);
        rx
    }

    pub fn unsubscribe_session(&self, session_id: &str) {
        self.private_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// Subscribe to all raw incoming CDP messages (responses + events).
    /// Used by the inspect proxy to forward traffic to the DevTools frontend.
    pub fn subscribe_raw(&self) -> broadcast::Receiver<RawCdpMessage> {
        self.raw_tx.subscribe()
    }

    /// Create a lightweight handle for the inspect WebSocket proxy.
    /// Contains only what's needed to forward messages bidirectionally.
    pub fn inspect_handle(&self) -> InspectProxyHandle {
        InspectProxyHandle {
            ws_tx: self.ws_tx.clone(),
            raw_tx: self.raw_tx.clone(),
            script_states: self.script_states.clone(),
            targets: self.target_sessions.clone(),
        }
    }

    pub async fn send_command_typed<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        session_id: Option<&str>,
    ) -> Result<R, String> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| format!("Failed to serialize params: {}", e))?;
        let result = self
            .send_command(method, Some(params_value), session_id)
            .await?;
        serde_json::from_value(result)
            .map_err(|e| format!("Failed to deserialize CDP response for {}: {}", method, e))
    }

    pub async fn send_command_no_params(
        &self,
        method: &str,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        self.send_command(method, None, session_id).await
    }

    /// Send a CDP command without waiting for its response.
    ///
    /// This is useful for best-effort commands where Chrome may not emit a
    /// response for every target session, but the command still needs to be
    /// written before the caller can continue processing events.
    pub async fn send_command_no_wait(
        &self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        let (method, params) = self.prepare_command(method, params, session_id).await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let cmd = CdpCommand {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.filter(|s| !s.is_empty()).map(|s| s.to_string()),
        };

        let json = serde_json::to_string(&cmd)
            .map_err(|e| format!("Failed to serialize CDP command: {}", e))?;

        let mut ws_tx = self.ws_tx.lock().await;
        // A fire-and-forget override has no accepted state receipt.
        let _ = self.script_change(method, cmd.params.as_ref(), session_id);
        ws_tx
            .send(Message::Text(json))
            .await
            .map_err(|e| format!("Failed to send CDP command: {}", e))
    }

    /// Send raw JSON through the WebSocket without tracking a response.
    /// Used by the inspect proxy to forward DevTools frontend messages.
    pub async fn send_raw(&self, json: String) -> Result<(), String> {
        let mut ws_tx = self.ws_tx.lock().await;
        script_unobserved(&self.script_states, &self.target_sessions, &json);
        ws_tx
            .send(Message::Text(json))
            .await
            .map_err(|e| format!("Failed to send raw CDP message: {}", e))
    }

    /// Test-only: count of in-flight commands still awaiting a response, so a
    /// test can assert a cancelled command left no orphaned entry (#1528).
    #[cfg(test)]
    pub(crate) async fn pending_len(&self) -> usize {
        self.pending.lock().await.len()
    }
}

type WsTx = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    >,
>;

/// Lightweight handle for the inspect WebSocket proxy, holding only
/// the cloneable parts of CdpClient needed for bidirectional message forwarding.
pub struct InspectProxyHandle {
    ws_tx: WsTx,
    raw_tx: broadcast::Sender<RawCdpMessage>,
    script_states: ScriptStates,
    targets: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl InspectProxyHandle {
    pub async fn send_raw(&self, json: String) -> Result<(), String> {
        let mut ws_tx = self.ws_tx.lock().await;
        script_unobserved(&self.script_states, &self.targets, &json);
        ws_tx
            .send(Message::Text(json))
            .await
            .map_err(|e| format!("Failed to send raw CDP message: {}", e))
    }

    pub fn subscribe_raw(&self) -> broadcast::Receiver<RawCdpMessage> {
        self.raw_tx.subscribe()
    }
}

/// Enable TCP SO_KEEPALIVE on the underlying socket of a WebSocket connection.
/// This is best-effort: failures are silently ignored since the WebSocket-level
/// Ping keepalive provides the primary connection liveness mechanism.
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn script_baseline_requires_exact_successful_created_target_not_a_paused_blank_attachment(
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let attached = |target: &str| json!({"method":"Target.attachedToTarget","params":{"sessionId":target,"waitingForDebugger":true,"targetInfo":{"targetId":target,"type":"page","url":"about:blank"}}});
            for target in ["adopted", "other"] {
                socket
                    .send(Message::Text(attached(target).to_string()))
                    .await
                    .unwrap();
            }
            for index in 0..3 {
                let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
                    panic!("a command");
                };
                let request: Value = serde_json::from_str(&body).unwrap();
                let reply = match index {
                    0 => {
                        assert_eq!(request["method"], "Browser.getVersion");
                        json!({"id":request["id"],"result":{}})
                    }
                    1 => {
                        assert_eq!(request["method"], "Target.createTarget");
                        socket
                            .send(Message::Text(attached("created").to_string()))
                            .await
                            .unwrap();
                        json!({"id":request["id"],"result":{"targetId":"created"}})
                    }
                    _ => {
                        assert_eq!(request["method"], "Target.createTarget");
                        socket
                            .send(Message::Text(attached("refused").to_string()))
                            .await
                            .unwrap();
                        json!({"id":request["id"],"error":{"code":-32000,"message":"refused"}})
                    }
                };
                socket.send(Message::Text(reply.to_string())).await.unwrap();
            }
            while let Some(Ok(_)) = socket.next().await {}
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        client
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        assert_eq!(client.script_execution_disabled("adopted"), None);
        assert_eq!(client.script_execution_disabled("other"), None);
        client
            .send_command(
                "Target.createTarget",
                Some(json!({"url":"about:blank"})),
                None,
            )
            .await
            .unwrap();
        assert_eq!(client.script_execution_disabled("created"), Some(false));
        assert_eq!(
            client.script_execution_disabled("other"),
            None,
            "another attachment does not borrow the created target's proof"
        );
        assert!(client
            .send_command(
                "Target.createTarget",
                Some(json!({"url":"about:blank"})),
                None
            )
            .await
            .is_err());
        assert_eq!(
            client.script_execution_disabled("refused"),
            None,
            "a failed creation grants no baseline"
        );
        client.disconnect();
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn script_state_tracks_writer_order_failed_replies_and_unknown_attachments() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(Message::Text(json!({"method":"Target.attachedToTarget","params":{"sessionId":"page","waitingForDebugger":false,"targetInfo":{"targetId":"A","type":"page","url":"https://private.example"}}}).to_string())).await.unwrap();
            let mut reversed = Vec::new();
            for index in 0..9 {
                let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
                    panic!("a command");
                };
                let command: Value = serde_json::from_str(&body).unwrap();
                if matches!(index, 3 | 4) {
                    reversed.push(command);
                    if index == 4 {
                        for command in reversed.iter().rev() {
                            socket
                                .send(Message::Text(
                                    json!({"id":command["id"],"result":{}}).to_string(),
                                ))
                                .await
                                .unwrap();
                        }
                    }
                    continue;
                }
                let reply = if index == 2 {
                    json!({"id":command["id"],"error":{"code":-32000,"message":"refused"}})
                } else {
                    json!({"id":command["id"],"result":{}})
                };
                socket.send(Message::Text(reply.to_string())).await.unwrap();
            }
            let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
                panic!("a barrier");
            };
            let command: Value = serde_json::from_str(&body).unwrap();
            socket.send(Message::Text(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"page","targetId":"A"}}).to_string())).await.unwrap();
            socket.send(Message::Text(json!({"method":"Target.attachedToTarget","params":{"sessionId":"page","waitingForDebugger":false,"targetInfo":{"targetId":"replacement","type":"page","url":"https://private.example"}}}).to_string())).await.unwrap();
            socket
                .send(Message::Text(
                    json!({"id":command["id"],"result":{}}).to_string(),
                ))
                .await
                .unwrap();
            while let Some(Ok(_)) = socket.next().await {}
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        client
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            None,
            "an adopted page has no enabled default"
        );
        client.seed_script_enabled("page");
        assert_eq!(client.script_execution_disabled("page"), Some(false));
        client
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some("page"),
            )
            .await
            .unwrap();
        assert_eq!(client.script_execution_disabled("page"), Some(true));
        assert!(client
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":false})),
                Some("page")
            )
            .await
            .is_err());
        assert_eq!(
            client.script_execution_disabled("page"),
            Some(true),
            "failure cannot advance confirmed state"
        );
        let early_id = client.reserve_command_id();
        let later_id = client.reserve_command_id();
        let mut first = client
            .enqueue_reserved_command(
                later_id,
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some("page"),
            )
            .await
            .unwrap();
        let mut second = client
            .enqueue_reserved_command(
                early_id,
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":false})),
                Some("page"),
            )
            .await
            .unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            None,
            "an outstanding later override is unknown"
        );
        second.acknowledgment().await.unwrap();
        first.acknowledgment().await.unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            None,
            "overlapping reordered mutations cannot invent the prior actor state"
        );
        client.inspect_handle().send_raw(json!({"id":-7,"method":"Emulation.setScriptExecutionDisabled","sessionId":"page","params":{"value":true}}).to_string()).await.unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            None,
            "untracked DevTools changes have no accepted receipt"
        );
        client
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        client
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some("page"),
            )
            .await
            .unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            None,
            "an unobserved actor value is not repaired by an equal-value acknowledgement"
        );
        client
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":false})),
                Some("page"),
            )
            .await
            .unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            Some(false),
            "a confirmed actual transition reproves shared state"
        );
        client
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        assert_eq!(
            client.script_execution_disabled("page"),
            None,
            "a replacement attachment inherits no prior state"
        );
        client.disconnect();
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn script_state_is_shared_with_operation_connection_and_detach_invalidates_it() {
        let owner_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let owner_address = owner_listener.local_addr().unwrap();
        let owner_server = tokio::spawn(async move {
            let (stream, _) = owner_listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for target in ["A", "B"] {
                socket.send(Message::Text(json!({"method":"Target.attachedToTarget","params":{"sessionId":target,"waitingForDebugger":true,"targetInfo":{"targetId":target,"type":"page","url":"about:blank"}}}).to_string())).await.unwrap();
            }
            while let Some(Ok(Message::Text(body))) = socket.next().await {
                let command: Value = serde_json::from_str(&body).unwrap();
                socket
                    .send(Message::Text(
                        json!({"id":command["id"],"result":{}}).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let owner = CdpClient::connect(&format!("ws://{owner_address}"))
            .await
            .unwrap();
        owner
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        owner.seed_script_enabled("A");
        owner.seed_script_enabled("B");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(Message::Text(json!({"method":"Target.attachedToTarget","params":{"sessionId":"operation","waitingForDebugger":false,"targetInfo":{"targetId":"A","type":"page","url":"https://private.example"}}}).to_string())).await.unwrap();
            while let Some(Ok(Message::Text(body))) = socket.next().await {
                let command: Value = serde_json::from_str(&body).unwrap();
                socket
                    .send(Message::Text(
                        json!({"id":command["id"],"result":{}}).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let operation = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        operation.publish_activity_as(&owner);
        operation
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        owner
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some("A"),
            )
            .await
            .unwrap();
        operation
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":false})),
                Some("operation"),
            )
            .await
            .unwrap();
        assert_eq!(owner.script_execution_disabled("A"), Some(true), "a new attachment's false-to-false no-op cannot overwrite the owner's shared true state");
        operation
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some("operation"),
            )
            .await
            .unwrap();
        assert_eq!(owner.script_execution_disabled("A"), Some(true));
        assert_eq!(owner.script_execution_disabled("B"), Some(false));
        operation.disconnect();
        assert_eq!(
            owner.script_execution_disabled("A"),
            None,
            "Blink may reset a detached operation's override"
        );
        assert_eq!(owner.script_execution_disabled("B"), Some(false));
        drop(operation);
        server.await.unwrap();
        owner.disconnect();
        drop(owner);
        owner_server.await.unwrap();
    }

    #[tokio::test]
    async fn no_wait_resume_prepares_custody_before_writing_the_resume() {
        use crate::native::agent_channel::frame::ChannelId;
        use crate::native::site_sessions::{custody::Custody, protocol::Request};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for expected in ["Fetch.enable", "Runtime.runIfWaitingForDebugger"] {
                let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                    panic!("a command");
                };
                let command: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(command["method"], expected);
                assert_eq!(command["sessionId"], "new-session");
                if expected == "Fetch.enable" {
                    socket
                        .send(Message::Text(
                            json!({"id":command["id"],"result":{}}).to_string(),
                        ))
                        .await
                        .unwrap();
                }
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let custody = Custody::new();
        custody
            .request(
                ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
                Request::read("site_sessions.offer", json!({"sites":[]})).unwrap(),
            )
            .await
            .unwrap();
        client.set_site_custody(Arc::downgrade(&custody), Default::default());
        client
            .send_command_no_wait("Runtime.runIfWaitingForDebugger", None, Some("new-session"))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            client.pending_len().await,
            0,
            "the resume awaits no CDP reply"
        );
        client.disconnect();
    }

    #[tokio::test]
    async fn cancelled_no_wait_preparation_never_writes_the_resume() {
        use crate::native::agent_channel::frame::ChannelId;
        use crate::native::site_sessions::{custody::Custody, protocol::Request};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (paused, reached) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!("a command");
            };
            let command: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(command["method"], "Fetch.enable");
            paused.send(()).unwrap();
            released.await.unwrap();
            socket
                .send(Message::Text(
                    json!({"id":command["id"],"result":{}}).to_string(),
                ))
                .await
                .unwrap();
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!("a command");
            };
            let command: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                command["method"], "Runtime.evaluate",
                "no cancelled resume precedes the barrier"
            );
            socket
                .send(Message::Text(
                    json!({"id":command["id"],"result":{}}).to_string(),
                ))
                .await
                .unwrap();
        });
        let client = Arc::new(
            CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        let custody = Custody::new();
        custody
            .request(
                ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
                Request::read("site_sessions.offer", json!({"sites":[]})).unwrap(),
            )
            .await
            .unwrap();
        client.set_site_custody(Arc::downgrade(&custody), Default::default());
        let pending = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .send_command_no_wait(
                        "Runtime.runIfWaitingForDebugger",
                        None,
                        Some("new-session"),
                    )
                    .await
            })
        };
        reached.await.unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        client
            .send_command_no_params("Runtime.evaluate", None)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(client.pending_len().await, 0);
        client.disconnect();
    }

    #[tokio::test]
    async fn explicit_attach_replies_share_the_target_session_association_writer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for succeeds in [true, false] {
                let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                    panic!("a command");
                };
                let command: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(command["method"], "Target.attachToTarget");
                let response = if succeeds {
                    serde_json::json!({"id":command["id"],"result":{"sessionId":"explicit-session"}})
                } else {
                    serde_json::json!({"id":command["id"],"error":{"code":-32000,"message":"refused"}})
                };
                socket
                    .send(Message::Text(response.to_string()))
                    .await
                    .unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        client
            .send_command(
                "Target.attachToTarget",
                Some(serde_json::json!({"targetId":"native-target","flatten":true})),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            client.target_for_session("explicit-session").as_deref(),
            Some("native-target")
        );
        assert_eq!(
            client.session_for_target("native-target").as_deref(),
            Some("explicit-session")
        );
        assert!(client
            .send_command(
                "Target.attachToTarget",
                Some(serde_json::json!({"targetId":"refused-target","flatten":true})),
                None
            )
            .await
            .is_err());
        assert!(client.session_for_target("refused-target").is_none());
        client.disconnect();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_detach_reattach_and_target_destruction_leave_no_stale_association() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for index in 0..4 {
                let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                    panic!("a command");
                };
                let command: Value = serde_json::from_str(&text).unwrap();
                let result = match index {
                    0 => serde_json::json!({"sessionId":"first-session"}),
                    1 => {
                        assert_eq!(command["method"], "Target.detachFromTarget");
                        serde_json::json!({})
                    }
                    2 => serde_json::json!({"sessionId":"second-session"}),
                    _ => {
                        socket.send(Message::Text(serde_json::json!({"method":"Target.targetDestroyed","params":{"targetId":"native-target"}}).to_string())).await.unwrap();
                        serde_json::json!({})
                    }
                };
                socket
                    .send(Message::Text(
                        serde_json::json!({"id":command["id"],"result":result}).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        client
            .send_command(
                "Target.attachToTarget",
                Some(serde_json::json!({"targetId":"native-target","flatten":true})),
                None,
            )
            .await
            .unwrap();
        client
            .send_command(
                "Target.detachFromTarget",
                Some(serde_json::json!({"sessionId":"first-session"})),
                None,
            )
            .await
            .unwrap();
        assert!(client.target_for_session("first-session").is_none());
        client
            .send_command(
                "Target.attachToTarget",
                Some(serde_json::json!({"targetId":"native-target","flatten":true})),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            client.session_for_target("native-target").as_deref(),
            Some("second-session")
        );
        client
            .send_command("Target.getTargets", Some(serde_json::json!({})), None)
            .await
            .unwrap();
        assert!(client.session_for_target("native-target").is_none());
        assert!(client.target_for_session("second-session").is_none());
        client.disconnect();
        server.await.unwrap();
    }

    /// Viewers watch a page, not its out-of-process frames: a frame's
    /// activity that carries screen coordinates, or no point at all, is
    /// published as the page's with the page's generation; a point in the
    /// frame's own CSS pixels stays with the frame, where it belongs.
    #[tokio::test]
    async fn a_frames_placeable_activity_is_published_as_its_pages() {
        use serde_json::json;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (attached_tx, attached) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            for (session, child, kind) in
                [("page", "frame", "iframe"), ("frame", "inner", "iframe")]
            {
                ws.send(Message::Text(
                    json!({"method":"Target.attachedToTarget","sessionId":session,"params":{
                        "sessionId":child,"targetInfo":{"targetId":format!("{child}-target"),"type":kind}}})
                    .to_string(),
                ))
                .await
                .unwrap();
            }
            let _ = attached_tx.send(());
            while ws.next().await.is_some() {}
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        attached.await.unwrap();
        // The reader handles the attachments in order: wait for the last.
        for _ in 0..100 {
            if client.page_of("inner") == "page" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(client.page_of("inner"), "page");
        assert_eq!(client.page_of("page"), "page");
        let mut events = client.subscribe();
        let page_generation = client.page_generation("page");
        let frame_generation = client.page_generation("inner");
        for activity in [
            json!({"type":"pointer","eventType":"move","x":5.0,"y":6.0,"screenX":300.0,"screenY":200.0}),
            json!({"type":"activity","kind":"typing"}),
            json!({"type":"pointer","eventType":"move","x":5.0,"y":6.0}),
        ] {
            client
                .observe_activity(
                    activity,
                    "inner",
                    frame_generation.clone(),
                    InputSource::Agent,
                )
                .acknowledged();
        }
        let mut published = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.method == activity::EVENT {
                published.push((
                    event.session_id.unwrap(),
                    event.params["pageGeneration"].as_str().unwrap().to_owned(),
                ));
            }
        }
        assert_eq!(
            published,
            [
                ("page".to_owned(), page_generation.clone()),
                ("page".to_owned(), page_generation),
                ("inner".to_owned(), frame_generation),
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn native_pointer_joins_a_later_event_and_never_relabels_an_unmatched_event() {
        use serde_json::json;
        for matched in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut first_input = None;
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let command: Value = serde_json::from_str(&text).unwrap();
                    let result = match command["method"].as_str().unwrap() {
                        "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"frame"}}}),
                        "Page.createIsolatedWorld" => json!({"executionContextId":1}),
                        "Runtime.evaluate"
                            if command["params"]["expression"] == "({scrollX,scrollY})" =>
                        {
                            json!({"result":{"value":{"scrollX":0,"scrollY":0}}})
                        }
                        "Runtime.evaluate" => json!({"result":{"value":true}}),
                        _ => json!({}),
                    };
                    ws.send(Message::Text(
                        json!({"id":command["id"],"result":result}).to_string(),
                    ))
                    .await
                    .unwrap();
                    if command["method"] == "Input.dispatchMouseEvent" {
                        let previous = first_input.replace(command["id"].as_u64().unwrap());
                        if matched || previous.is_some() {
                            // The binding follows the primary acknowledgment.
                            // In the unmatched case, deliver the OLD command's
                            // late event only after the next input is admitted.
                            let token = previous.unwrap_or(first_input.unwrap());
                            ws.send(Message::Text(json!({"method":"Runtime.bindingCalled","sessionId":"page","params":{
                                "name":activity::POINTER_BINDING,"payload":json!({"token":token.to_string(),"eventType":"move",
                                    "clientX":12,"clientY":34,"screenX":56,"screenY":78}).to_string()
                            }}).to_string())).await.unwrap();
                        }
                    }
                }
            });
            let client = CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap();
            client.enable_window_pointer();
            let mut events = client.subscribe();
            for index in 0..if matched { 1 } else { 2 } {
                client
                    .send_command(
                        "Input.dispatchMouseEvent",
                        Some(json!({"type":"mouseMoved","x":12,"y":34})),
                        Some("page"),
                    )
                    .await
                    .unwrap();
                let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(event.method, activity::EVENT);
                assert_eq!(event.params.get("screenX"), matched.then_some(&json!(56)));
                assert!(
                    client.native_pointer_enabled.load(Ordering::Acquire),
                    "command {index}: one unmatched event must not end native attribution"
                );
            }
            assert!(client.pending.lock().await.is_empty());
            server.abort();
        }
    }
    use tokio::sync::oneshot;

    #[test]
    fn normalizes_only_root_websocket_queries() {
        assert_eq!(
            normalize_websocket_root_path("wss://browser.example?token=a%2Fb"),
            "wss://browser.example/?token=a%2Fb"
        );
        assert_eq!(
            normalize_websocket_root_path("ws://[::1]:9222?token=test"),
            "ws://[::1]:9222/?token=test"
        );
        assert_eq!(
            normalize_websocket_root_path("wss://user:pass@browser.example?token=test"),
            "wss://user:pass@browser.example/?token=test"
        );
        assert_eq!(
            normalize_websocket_root_path("wss://browser.example?"),
            "wss://browser.example/?"
        );
        assert_eq!(
            normalize_websocket_root_path("wss://browser.example/?token=a%2Fb"),
            "wss://browser.example/?token=a%2Fb"
        );
        assert_eq!(
            normalize_websocket_root_path("wss://browser.example/cdp?token=a%2Fb"),
            "wss://browser.example/cdp?token=a%2Fb"
        );
    }

    #[tokio::test]
    async fn root_websocket_url_with_query_sends_slash_request_target() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (path_tx, path_rx) = oneshot::channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut path_tx = Some(path_tx);
            let _ = tokio_tungstenite::accept_hdr_async(
                stream,
                move |
                    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                    response: tokio_tungstenite::tungstenite::handshake::server::Response,
                | {
                    if let Some(tx) = path_tx.take() {
                        let path = request
                            .uri()
                            .path_and_query()
                            .map(|value| value.as_str().to_string())
                            .unwrap_or_default();
                        let _ = tx.send(path);
                    }
                    Ok(response)
                },
            )
            .await
            .unwrap();
        });

        let url = format!("ws://127.0.0.1:{}?token=a%2Fb&scope=browser%20test", port);
        let _client = CdpClient::connect(&url).await.unwrap();

        assert_eq!(path_rx.await.unwrap(), "/?token=a%2Fb&scope=browser%20test");
        server.await.unwrap();
    }
    /// Events on a privately subscribed session reach only that receiver;
    /// everything else still reaches broadcast subscribers.
    #[tokio::test]
    async fn private_session_events_bypass_the_broadcast() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ready_rx.await.unwrap();
            for (method, session) in [
                ("Page.screencastFrame", Some("S-REC")),
                ("Page.screencastFrame", Some("S-PAGE")),
                ("Target.detachedFromTarget", None),
            ] {
                let mut msg = serde_json::json!({ "method": method, "params": {} });
                if let Some(sid) = session {
                    msg["sessionId"] = serde_json::json!(sid);
                }
                ws.send(Message::Text(msg.to_string())).await.unwrap();
            }
            // Keep the connection open until the client is done reading.
            let _ = ws.next().await;
        });

        let client = CdpClient::connect(&format!("ws://127.0.0.1:{}", port))
            .await
            .unwrap();
        let mut broadcast_rx = client.subscribe();
        let mut private_rx = client.subscribe_session("S-REC");
        ready_tx.send(()).unwrap();

        let wait = std::time::Duration::from_secs(2);
        let first = tokio::time::timeout(wait, broadcast_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.session_id.as_deref(), Some("S-PAGE"));
        let second = tokio::time::timeout(wait, broadcast_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.method, "Target.detachedFromTarget");

        let private = tokio::time::timeout(wait, private_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(private.session_id.as_deref(), Some("S-REC"));
        assert!(
            private_rx.try_recv().is_err(),
            "only S-REC events are routed privately"
        );

        client.unsubscribe_session("S-REC");
        drop(client);
        server.abort();
    }

    /// The wire form of a lone surrogate is its `\u` escape, which serde_json
    /// refuses. A pair, an escaped backslash and every other escape are kept,
    /// and a clean message is returned without a copy.
    #[test]
    fn lone_surrogate_escapes_become_the_replacement_character() {
        let unchanged = [
            r#"{"title":"caf\u00e9 \ud83d\ude00 \uD83D\uDE00"}"#,
            r#""\\ud800 \"\n\t\/ \u0041 \\""#,
            r#""\ud80"#,
            "\"\\é\"",
            "\\",
            "",
        ];
        for text in unchanged {
            let owned = text.to_string();
            let pointer = owned.as_ptr();
            let kept = replace_lone_surrogate_escapes(owned);
            assert_eq!(kept, text);
            assert_eq!(kept.as_ptr(), pointer, "a clean message is not copied");
        }
        let replaced = [
            (r#""\ud800""#, r#""\uFFFD""#),
            (r#""a\uDC00b""#, r#""a\uFFFDb""#),
            (r#""\ud800\u0041""#, r#""\uFFFD\u0041""#),
            (r#""\ud800\ud800\ude00""#, r#""\uFFFD\ud800\ude00""#),
            (r#""\ude00\ud800""#, r#""\uFFFD\uFFFD""#),
            (r#""\"\ud800\\""#, r#""\"\uFFFD\\""#),
            (r#""é\ud800é""#, r#""é\uFFFDé""#),
        ];
        for (text, expected) in replaced {
            assert_eq!(replace_lone_surrogate_escapes(text.to_string()), expected);
        }
        let parsed: Value = serde_json::from_str(&replace_lone_surrogate_escapes(
            r#"{"id":7,"result":{"value":"Checkout \ud800 x"}}"#.to_string(),
        ))
        .unwrap();
        assert_eq!(parsed["result"]["value"], "Checkout \u{FFFD} x");
    }

    /// A reply and an event whose page text holds a lone surrogate reach the
    /// awaiting command and the event subscribers instead of being dropped.
    #[tokio::test]
    async fn messages_with_lone_surrogates_are_decoded_not_dropped() {
        use serde_json::json;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                // Chrome's wire form of a retitled page and of a title read.
                ws.send(Message::Text(
                    r#"{"method":"Target.targetInfoChanged","params":{"targetInfo":{"targetId":"T1","type":"page","title":"Checkout \ud800","url":"https://shop.example/","attached":true}}}"#.to_string(),
                ))
                .await
                .unwrap();
                ws.send(Message::Text(format!(
                    r#"{{"id":{},"result":{{"result":{{"type":"string","value":"Total \udc00 42"}}}}}}"#,
                    command["id"]
                )))
                .await
                .unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let mut events = client.subscribe();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.send_command(
                "Runtime.evaluate",
                Some(json!({"expression":"document.title"})),
                Some("page"),
            ),
        )
        .await
        .expect("the reply is decoded, not dropped")
        .unwrap();
        assert_eq!(result["result"]["value"], "Total \u{FFFD} 42");
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.method, "Target.targetInfoChanged");
        assert_eq!(event.params["targetInfo"]["title"], "Checkout \u{FFFD}");
        assert!(client.pending.lock().await.is_empty());
        server.abort();
    }

    /// A reply in a binary frame (as remote CDP proxies send them) whose bytes
    /// are not UTF-8 reaches the awaiting command, decoded leniently.
    #[tokio::test]
    async fn binary_frames_that_are_not_utf8_are_decoded_not_dropped() {
        use serde_json::json;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let mut reply = format!(
                    r#"{{"id":{},"result":{{"result":{{"type":"string","value":"Total "#,
                    command["id"]
                )
                .into_bytes();
                reply.push(0xFF);
                reply.extend_from_slice(br#" 42"}}}"#);
                ws.send(Message::Binary(reply)).await.unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.send_command(
                "Runtime.evaluate",
                Some(json!({"expression":"document.title"})),
                Some("page"),
            ),
        )
        .await
        .expect("the reply is decoded, not dropped")
        .unwrap();
        assert_eq!(result["result"]["value"], "Total \u{FFFD} 42");
        server.abort();
    }
}
