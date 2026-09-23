use serde_json::{json, Value};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch, Mutex, Notify, RwLock};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::native::browser_control::{custody_active, BrowserControl};
use crate::native::cdp::client::CdpClient;
#[cfg(test)]
use crate::native::input::keyboard_params;

use super::http::handle_http_request;
use super::presentation::{Presentation, PresentationConfig};
use super::{is_allowed_origin, timestamp_ms, IdleActivity, StreamFrame};

/// Highest per-client frame rate a client may request via the `config` message.
const MAX_CONFIGURABLE_FPS: u32 = 120;

// One bounded drain for all viewers, not a delay per connection. Responsive
// viewers receive the terminal record; a stalled socket cannot hold shutdown.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-connection delivery settings, set by the client's `config` message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ClientConfig {
    /// Frames per second ceiling. 0 means uncapped.
    max_fps: u32,
    /// Bound in-flight frames until the client acknowledges painting them.
    ///
    /// Latest-frame-wins applies before delivery. Already delivered frames
    /// remain ordered and counted until a cumulative acknowledgment releases
    /// their prefix. The default window preserves one-frame CDP-style pacing.
    ack_pacing: bool,
    presentation: Option<PresentationConfig>,
    /// The client composites damage patches over its last whole frame.
    patches: bool,
    /// JPEG payloads travel as bytes after a bounded metadata header.
    binary: bool,
    /// Maximum outstanding frames. One preserves the existing ack protocol;
    /// a negotiated window covers network delay without an unbounded queue.
    frame_window: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_fps: 0,
            ack_pacing: false,
            presentation: None,
            patches: false,
            binary: false,
            frame_window: 1,
        }
    }
}

const MAX_FRAME_WINDOW: usize = 8;
const MAX_WINDOW_BYTES: usize = 12 * 1024 * 1024;
/// How long a delivery-rate sample informs the in-flight bound. Longer than
/// a typing pause, so patches between scroll bursts do not erase what the
/// path delivered moments ago.
const DELIVERY_RATE_WINDOW: Duration = Duration::from_secs(5);
/// How long the shortest round trip stands, like a transport's minimum RTT:
/// long enough that a standing queue cannot become the new floor.
const ROUND_TRIP_WINDOW: Duration = Duration::from_secs(10);

struct SentFrame {
    seq: u64,
    bytes: usize,
    sent_at: Instant,
    /// Bytes acknowledged when this frame left, for its delivery-rate sample.
    delivered_before: u64,
    /// A whole frame, which the delivery bound governs and so measures.
    whole: bool,
}

/// Frames delivered but not yet acknowledged as painted, and what their
/// acknowledgements say about the path. A viewer's newest pixels wait behind
/// every older frame still in flight, so beyond the negotiated count and byte
/// bounds a whole frame leaves only while the bytes ahead of it are within
/// what the path delivers in its shortest round trip. At most about one
/// frame then queues behind another, which keeps the path full (a frame
/// travels while the previous one is painted) without a standing backlog.
/// Patches are exempt: they are small, and holding one back would strand the
/// next patch without its base and force a whole-frame rebase. For the same
/// reason they do not measure the path: a patch travels alone, so its paint
/// measures what one patch needed, not what the path can deliver, and while
/// the reader types, patch samples alone would shrink the bound to one patch.
/// A frame alone on the path is always admitted.
#[derive(Default)]
struct InFlightFrames {
    frames: VecDeque<SentFrame>,
    bytes: usize,
    delivered: u64,
    /// (acknowledged at, bytes per second) samples, newest last.
    rates: VecDeque<(Instant, f64)>,
    /// (acknowledged at, send-to-paint round trip) samples, newest last.
    round_trips: VecDeque<(Instant, Duration)>,
}

impl InFlightFrames {
    fn has_slot(&self, window: usize) -> bool {
        self.frames.len() < window
    }
    fn has_bytes(&self, bytes: usize) -> bool {
        self.bytes
            .checked_add(bytes)
            .is_some_and(|total| total <= MAX_WINDOW_BYTES)
    }
    /// Whether a frame of `bytes` may leave now: within the hard byte bound,
    /// and, for a whole frame, either alone on the path or behind no more
    /// than the delivery bound. Before any acknowledgement only the
    /// negotiated bounds apply.
    fn admits(&self, bytes: usize, whole: bool) -> bool {
        self.has_bytes(bytes)
            && (self.frames.is_empty()
                || !whole
                || self.bound().is_none_or(|bound| self.bytes <= bound))
    }
    /// The best recent delivery rate over the shortest recent round trip.
    fn bound(&self) -> Option<usize> {
        let rate = self.rates.iter().map(|(_, rate)| *rate).reduce(f64::max)?;
        let round_trip = self.round_trips.iter().map(|(_, rtt)| *rtt).min()?;
        Some((rate * round_trip.as_secs_f64()) as usize)
    }
    fn sent(&mut self, seq: u64, bytes: usize, now: Instant, whole: bool) {
        self.frames.push_back(SentFrame {
            seq,
            bytes,
            sent_at: now,
            delivered_before: self.delivered,
            whole,
        });
        self.bytes += bytes;
    }
    /// Releases the acknowledged prefix. `sample` is false when the watermark
    /// is re-applied at send time: that is bookkeeping, not a measurement.
    fn acknowledge(&mut self, seq: u64, now: Instant, sample: bool) {
        let mut newest_whole = None;
        while self.frames.front().is_some_and(|frame| frame.seq <= seq) {
            let frame = self.frames.pop_front().unwrap();
            self.bytes -= frame.bytes;
            self.delivered += frame.bytes as u64;
            if frame.whole {
                newest_whole = Some(frame);
            }
        }
        let Some(frame) = newest_whole.filter(|_| sample) else {
            return;
        };
        // The paint of the newest released whole frame closes one round trip;
        // every byte acknowledged since it left crossed the path in that time.
        let round_trip = now.saturating_duration_since(frame.sent_at);
        if round_trip.is_zero() {
            return;
        }
        let rate = (self.delivered - frame.delivered_before) as f64 / round_trip.as_secs_f64();
        Self::record(&mut self.rates, now, rate, DELIVERY_RATE_WINDOW);
        Self::record(&mut self.round_trips, now, round_trip, ROUND_TRIP_WINDOW);
    }
    /// Keeps samples within `window` of the newest, so an idle viewer keeps
    /// its last estimate instead of starting over unbounded.
    fn record<T>(samples: &mut VecDeque<(Instant, T)>, now: Instant, value: T, window: Duration) {
        samples.push_back((now, value));
        while samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > window)
        {
            samples.pop_front();
        }
    }
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Parse a client `config` message into the settings it changes, leaving the
/// rest of `current` untouched. Returns `None` when the message carries no
/// recognized setting, so an unknown or malformed config never disturbs a
/// working connection.
fn apply_config(current: ClientConfig, parsed: &Value) -> Option<ClientConfig> {
    let mut next = current;
    let mut changed = false;

    // Clamped on u64 before narrowing so oversized values cap at the maximum
    // instead of wrapping.
    if let Some(fps) = parsed.get("maxFps").and_then(|v| v.as_u64()) {
        next.max_fps = fps.min(MAX_CONFIGURABLE_FPS as u64) as u32;
        changed = true;
    }
    if let Some(pacing) = parsed.get("pacing").and_then(|v| v.as_str()) {
        match pacing {
            "ack" => {
                next.ack_pacing = true;
                changed = true;
            }
            "push" => {
                next.ack_pacing = false;
                changed = true;
            }
            _ => {}
        }
    }

    if changed {
        Some(next)
    } else {
        None
    }
}

/// Settings declared on the WebSocket URL, applied before the first frame.
///
/// Invariant: only the URL can govern the opening frame, since the cached frame
/// is written before a `config` message can arrive. An unparsable value leaves
/// the default; a later `config` still wins.
fn config_from_upgrade(request: &str) -> ClientConfig {
    let mut cfg = ClientConfig::default();
    let Some(query) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|target| target.split_once('?'))
        .map(|(_, query)| query)
    else {
        return cfg;
    };

    let mut width = None;
    let mut height = None;
    for (key, value) in query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
    {
        match key {
            "width" => width = Some(value),
            "height" => height = Some(value),
            "maxFps" => {
                if let Ok(fps) = value.parse::<u64>() {
                    cfg.max_fps = fps.min(MAX_CONFIGURABLE_FPS as u64) as u32;
                }
            }
            "pacing" => match value {
                "ack" => cfg.ack_pacing = true,
                "push" => cfg.ack_pacing = false,
                _ => {}
            },
            "patches" => cfg.patches = value == "1",
            "frames" => cfg.binary = value == "binary",
            "frameWindow" => {
                if let Some(window) = value
                    .parse::<usize>()
                    .ok()
                    .filter(|window| (1..=MAX_FRAME_WINDOW).contains(window))
                {
                    cfg.frame_window = window;
                }
            }
            _ => {}
        }
    }
    let viewer = request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("X-Ambit-Browser-Viewer"))
        .map(|(_, value)| value.trim());
    cfg.presentation = match (viewer, width, height) {
        (Some(viewer), Some(width), Some(height)) => {
            PresentationConfig::parse(viewer, width, height)
        }
        _ => None,
    };
    cfg
}

/// Only an upgrade-bound presenter may change its requested geometry. The
/// message cannot select another viewer or introduce presentation ownership.
fn updated_presentation(mut config: ClientConfig, message: &Value) -> Option<ClientConfig> {
    let current = config.presentation?;
    let width = message.get("width")?.as_u64()?;
    let height = message.get("height")?.as_u64()?;
    if !(1..=2048).contains(&width) || !(1..=2048).contains(&height) {
        return None;
    }
    config.presentation = Some(PresentationConfig {
        viewer: current.viewer,
        width: width as u32,
        height: height as u32,
    });
    Some(config)
}

/// Read the acknowledged frame id from a client `ack` message. Acks are
/// cumulative: acknowledging the newest painted frame also releases the
/// preceding delivered frames represented by that composed image.
fn parse_ack_seq(parsed: &Value) -> Option<u64> {
    parsed.get("seq").and_then(|v| v.as_u64())
}

/// Earliest instant the next frame may be sent. `None` (nothing delivered yet)
/// and `fps == 0` (uncapped) both mean "now": a cap bounds the interval between
/// deliveries and never delays the first one.
fn deadline_from(last_sent: Option<Instant>, fps: u32) -> Instant {
    match last_sent {
        Some(sent) if fps > 0 => sent + Duration::from_micros(1_000_000 / fps as u64),
        Some(sent) => sent,
        None => Instant::now(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn accept_loop(
    listener: TcpListener,
    frame_tx: broadcast::Sender<String>,
    frame_watch: watch::Receiver<Option<Arc<StreamFrame>>>,
    client_count: Arc<Mutex<usize>>,
    patch_clients: Arc<AtomicUsize>,
    client_slot: Arc<RwLock<Option<Arc<CdpClient>>>>,
    client_notify: Arc<Notify>,
    idle_activity: Arc<IdleActivity>,
    browser_control: Arc<Mutex<BrowserControl>>,
    presentation: Arc<Presentation>,
    screencasting: Arc<Mutex<bool>>,
    cdp_session_id: Arc<RwLock<Option<String>>>,
    viewport_width: Arc<Mutex<u32>>,
    viewport_height: Arc<Mutex<u32>>,
    last_tabs: Arc<RwLock<Vec<Value>>>,
    last_engine: Arc<RwLock<String>>,
    recording: Arc<Mutex<bool>>,
    mut shutdown_rx: watch::Receiver<bool>,
    session_name: String,
) {
    let session_name: Arc<str> = Arc::from(session_name);
    // Invariant: every per-connection task is owned here, never detached, so
    // awaiting this loop proves none survives. Teardown first lets responsive
    // writers report the stream's explicit end, then aborts any blocked peer.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            // Reap finished connections so the set does not grow without bound.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accept_result = listener.accept() => {
                let Ok((stream, addr)) = accept_result else {
                    break;
                };
                let frame_tx = frame_tx.clone();
                let frame_watch = frame_watch.clone();
                let client_count = client_count.clone();
                let patch_clients = patch_clients.clone();
                let client_slot = client_slot.clone();
                let client_notify = client_notify.clone();
                let idle_activity = idle_activity.clone();
                let browser_control = browser_control.clone();
                let presentation = presentation.clone();
                let screencasting = screencasting.clone();
                let cdp_session_id = cdp_session_id.clone();
                let vw = viewport_width.clone();
                let vh = viewport_height.clone();
                let lt = last_tabs.clone();
                let le = last_engine.clone();
                let rec = recording.clone();
                let shutdown_rx = shutdown_rx.clone();
                let sn = session_name.clone();

                connections.spawn(async move {
                    handle_connection(
                        stream,
                        addr,
                        frame_tx,
                        frame_watch,
                        client_count,
                        patch_clients,
                        client_slot,
                        client_notify,
                        idle_activity,
                        browser_control,
                        presentation,
                        screencasting,
                        cdp_session_id,
                        vw,
                        vh,
                        lt,
                        le,
                        rec,
                        shutdown_rx,
                        sn,
                    )
                    .await;
                });
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.shutdown().await;
    }
}

fn is_websocket_upgrade(request: &str) -> bool {
    request.lines().any(|line| {
        if let Some((name, value)) = line.split_once(':') {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.trim().eq_ignore_ascii_case("websocket")
        } else {
            false
        }
    })
}

/// Peek at the TCP stream to dispatch between WebSocket upgrade and plain HTTP.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    frame_tx: broadcast::Sender<String>,
    frame_watch: watch::Receiver<Option<Arc<StreamFrame>>>,
    client_count: Arc<Mutex<usize>>,
    patch_clients: Arc<AtomicUsize>,
    client_slot: Arc<RwLock<Option<Arc<CdpClient>>>>,
    client_notify: Arc<Notify>,
    idle_activity: Arc<IdleActivity>,
    browser_control: Arc<Mutex<BrowserControl>>,
    presentation: Arc<Presentation>,
    screencasting: Arc<Mutex<bool>>,
    cdp_session_id: Arc<RwLock<Option<String>>>,
    viewport_width: Arc<Mutex<u32>>,
    viewport_height: Arc<Mutex<u32>>,
    last_tabs: Arc<RwLock<Vec<Value>>>,
    last_engine: Arc<RwLock<String>>,
    recording: Arc<Mutex<bool>>,
    shutdown_rx: watch::Receiver<bool>,
    session_name: Arc<str>,
) {
    let mut buf = [0u8; 4096];
    let n = match stream.peek(&mut buf).await {
        Ok(n) => n,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);

    if is_websocket_upgrade(&request) {
        let frame_rx = frame_tx.subscribe();
        let initial_config = config_from_upgrade(&request);
        handle_ws_client(
            stream,
            addr,
            initial_config,
            frame_rx,
            frame_watch,
            client_count,
            patch_clients,
            client_slot,
            client_notify,
            idle_activity,
            browser_control,
            presentation,
            screencasting,
            cdp_session_id,
            viewport_width,
            viewport_height,
            last_tabs,
            last_engine,
            recording,
            shutdown_rx,
        )
        .await;
    } else {
        handle_http_request(stream, &buf[..n], &last_tabs, &last_engine, &session_name).await;
    }
}

/// One WebSocket client, split in two halves: a reader task dispatching input
/// to CDP, and a writer loop delivering frames latest-first under an optional
/// per-client cap. Invariant: the halves never wait on each other, which is
/// what keeps input responsive while a frame is mid-write.
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
async fn handle_ws_client(
    stream: TcpStream,
    _addr: SocketAddr,
    initial_config: ClientConfig,
    mut broadcast_rx: broadcast::Receiver<String>,
    mut frame_watch: watch::Receiver<Option<Arc<StreamFrame>>>,
    client_count: Arc<Mutex<usize>>,
    patch_clients: Arc<AtomicUsize>,
    client_slot: Arc<RwLock<Option<Arc<CdpClient>>>>,
    client_notify: Arc<Notify>,
    idle_activity: Arc<IdleActivity>,
    browser_control: Arc<Mutex<BrowserControl>>,
    presentation: Arc<Presentation>,
    screencasting: Arc<Mutex<bool>>,
    cdp_session_id: Arc<RwLock<Option<String>>>,
    viewport_width: Arc<Mutex<u32>>,
    viewport_height: Arc<Mutex<u32>>,
    last_tabs: Arc<RwLock<Vec<Value>>>,
    last_engine: Arc<RwLock<String>>,
    recording: Arc<Mutex<bool>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let callback =
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
            let origin = req
                .headers()
                .get("origin")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            if !is_allowed_origin(origin.as_deref()) {
                let mut reject =
                    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some(
                        "Origin not allowed".to_string(),
                    ));
                *reject.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
                return Err(reject);
            }
            Ok(resp)
        };

    let ws_stream = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(ws) => ws,
        Err(_) => return,
    };

    let connection_id = uuid::Uuid::new_v4();
    let _presentation_connection = presentation.connection(connection_id);
    let mut presentation_rx = presentation.subscribe();
    let mut custody_rx = browser_control.lock().await.custody();
    if let Some(config) = initial_config.presentation {
        presentation.configure(connection_id, config);
        idle_activity.mark();
    }

    {
        let mut count = client_count.lock().await;
        *count += 1;
        if initial_config.patches {
            patch_clients.fetch_add(1, Ordering::AcqRel);
        }
    }

    let (mut ws_tx, ws_rx) = ws_stream.split();

    // Watch channels, not atomics, so a mid-stream change wakes the writer's
    // select! below instead of leaving it asleep on a stale deadline.
    let (config_tx, mut config_rx) = watch::channel::<ClientConfig>(initial_config);
    let (ack_tx, mut ack_rx) = watch::channel::<u64>(0);
    let (input_error_tx, mut input_error_rx) = watch::channel::<Option<Value>>(None);
    // Spawned before the status, tabs and seed writes below: those are
    // unbounded sends, and a full receive queue would otherwise hold input
    // behind the whole handshake.
    let mut reader_task = AbortOnDrop(tokio::spawn(reader_loop(
        ws_rx,
        client_slot.clone(),
        cdp_session_id.clone(),
        config_tx,
        ack_tx,
        idle_activity.clone(),
        browser_control,
        input_error_tx,
        presentation.clone(),
        connection_id,
    )));

    if let Some(config) = initial_config.presentation {
        let _ = ws_tx
            .send(Message::Text(
                presentation
                    .acknowledgment(connection_id, config)
                    .to_string(),
            ))
            .await;
        presentation_rx.borrow_and_update();
    }

    {
        let guard = client_slot.read().await;
        let connected = guard.is_some();
        let sc = *screencasting.lock().await;
        let vw = *viewport_width.lock().await;
        let vh = *viewport_height.lock().await;
        let eng = last_engine.read().await.clone();
        let rec = *recording.lock().await;
        let status = json!({
            "type": "status",
            "connected": connected,
            "screencasting": sc,
            "viewportWidth": vw,
            "viewportHeight": vh,
            "engine": eng,
            "recording": rec,
        });
        let _ = ws_tx.send(Message::Text(status.to_string())).await;

        let tabs = last_tabs.read().await;
        if !tabs.is_empty() {
            let tabs_msg = json!({
                "type": "tabs",
                "tabs": *tabs,
                "timestamp": timestamp_ms(),
            });
            let _ = ws_tx.send(Message::Text(tabs_msg.to_string())).await;
        }
    }

    // Invariant: only a successful send writes `last_sent`. `None` means
    // nothing was delivered, so the cap owes this connection no wait.
    let mut last_sent: Option<Instant> = None;
    // Only sequence and byte counts are retained here, never another payload
    // queue. Undelivered whole frames still replace one another in the watch.
    let mut in_flight = InFlightFrames::default();
    let mut byte_blocked = false;
    let mut delivered_seq: Option<u64> = None;

    // Seed with the newest frame, marked seen so the writer does not re-send
    // it. Charged against the cap, so a URL-declared cap governs the gap after.
    // A patch is meaningless without the whole frame it amends; a client that
    // does not composite gets the next whole frame instead.
    let initial_frame = frame_watch
        .borrow_and_update()
        .clone()
        .filter(|frame| !frame.patch);
    if let Some(frame) = initial_frame {
        if let Some(message) = frame.message(initial_config.binary) {
            let message_bytes = message.len();
            if message_bytes > MAX_WINDOW_BYTES {
                let _ = ws_tx.send(Message::Close(None)).await;
                drop(reader_task);
                retire_viewer(
                    &client_count,
                    &patch_clients,
                    initial_config.patches,
                    &client_notify,
                )
                .await;
                return;
            }
            if ws_tx.send(message).await.is_ok() {
                last_sent = Some(Instant::now());
                delivered_seq = frame.seq;
                if initial_config.ack_pacing {
                    if let Some(seq) = frame.seq {
                        in_flight.sent(seq, message_bytes, Instant::now(), true);
                    }
                }
            }
        }
    }

    client_notify.notify_one();

    // The delivery cap follows the same custody the capture loop follows.
    let mut controlled = custody_active(*custody_rx.borrow_and_update());
    let mut next_allowed = deadline_from(
        last_sent,
        presentation.client_fps(connection_id, initial_config.max_fps, controlled),
    );
    let mut pending_frame = false;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    // Shutdown is an explicit lifecycle event. An unlabelled
                    // EOF also occurs on transport loss and is not equivalent.
                    let _ = ws_tx.send(Message::Text(r#"{"type":"finished"}"#.into())).await;
                    let _ = ws_tx.send(Message::Close(None)).await;
                    break;
                }
            }
            _ = &mut reader_task.0 => {
                break;
            }
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(data) => {
                        if ws_tx.send(Message::Text(data)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            changed = frame_watch.changed(), if !pending_frame => {
                if changed.is_err() {
                    break;
                }
                pending_frame = true;
            }
            changed = presentation_rx.changed(), if initial_config.presentation.is_some() => {
                if changed.is_err() { break; }
                let config = config_rx.borrow().presentation.unwrap();
                presentation.claim_if_available(connection_id, config);
                presentation_rx.borrow_and_update();
                next_allowed = deadline_from(last_sent, presentation.client_fps(connection_id, config_rx.borrow().max_fps, controlled));
                if ws_tx.send(Message::Text(presentation.acknowledgment(connection_id, config).to_string())).await.is_err() { break; }
            }
            changed = config_rx.changed() => {
                // The sender lives as long as the reader, so an error here
                // means the reader ended. Break and let cleanup run.
                if changed.is_err() {
                    break;
                }
                let cfg = *config_rx.borrow_and_update();
                // Loosening the cap pulls the deadline into the past, so a
                // pending frame goes out at once.
                next_allowed = deadline_from(last_sent, presentation.client_fps(connection_id, cfg.max_fps, controlled));
                // Leaving ack pacing releases a frame that is still waiting on
                // an acknowledgement the client will now never send.
                if !cfg.ack_pacing {
                    in_flight.clear();
                    byte_blocked = false;
                }
            }
            changed = custody_rx.changed() => {
                if changed.is_err() { break; }
                controlled = custody_active(*custody_rx.borrow_and_update());
                next_allowed = deadline_from(last_sent, presentation.client_fps(connection_id, config_rx.borrow().max_fps, controlled));
            }
            changed = input_error_rx.changed() => {
                if changed.is_err() { break; }
                let error = input_error_rx.borrow_and_update().clone();
                if let Some(error) = error {
                    if ws_tx.send(Message::Text(error.to_string())).await.is_err() { break; }
                }
            }
            changed = ack_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let acked = *ack_rx.borrow_and_update();
                // The native relay accepts only a sequence it actually sent.
                // Painting it also settles every earlier delivered frame.
                in_flight.acknowledge(acked, Instant::now(), true);
                byte_blocked = false;
            }
            _ = tokio::time::sleep_until(next_allowed), if pending_frame && !byte_blocked && (!config_rx.borrow().ack_pacing || in_flight.has_slot(config_rx.borrow().frame_window)) => {
                // Invariant: read at send time, not arrival time. That is what
                // makes this latest-frame-wins; anything that arrived while the
                // writer waited is skipped rather than queued.
                let frame = frame_watch
                    .borrow_and_update()
                    .clone()
                    .filter(|frame| initial_config.patches || !frame.patch);
                pending_frame = false;
                let cfg = *config_rx.borrow();
                if let Some(frame) = frame {
                    // Latest-frame-wins is safe for whole frames only. A
                    // delta can travel only after its exact base reached
                    // this connection; otherwise rebase with a whole frame.
                    if frame.patch && (frame.base_seq.is_none() || frame.base_seq != delivered_seq) {
                        client_notify.notify_one();
                        continue;
                    }
                    let Some(message) = frame.message(initial_config.binary) else { break; };
                    let bytes = message.len();
                    if bytes > MAX_WINDOW_BYTES { break; }
                    if cfg.ack_pacing && !in_flight.admits(bytes, !frame.patch) {
                        pending_frame = true;
                        byte_blocked = true;
                        continue;
                    }
                    if ws_tx.send(message).await.is_err() {
                        break;
                    }
                    if cfg.ack_pacing {
                        if let Some(seq) = frame.seq { in_flight.sent(seq, bytes, Instant::now(), !frame.patch); }
                        // Preserve the existing native cumulative watermark.
                        in_flight.acknowledge(*ack_rx.borrow(), Instant::now(), false);
                    }
                    last_sent = Some(Instant::now());
                    delivered_seq = frame.seq;
                }
                controlled = custody_active(*custody_rx.borrow());
                next_allowed = deadline_from(last_sent, presentation.client_fps(connection_id, cfg.max_fps, controlled));
            }
        }
    }

    drop(reader_task);

    retire_viewer(
        &client_count,
        &patch_clients,
        initial_config.patches,
        &client_notify,
    )
    .await;
}

async fn retire_viewer(
    client_count: &Mutex<usize>,
    patch_clients: &AtomicUsize,
    patches: bool,
    client_notify: &Notify,
) {
    let mut count = client_count.lock().await;
    *count = count.saturating_sub(1);
    if patches {
        patch_clients.fetch_sub(1, Ordering::AcqRel);
    }
    drop(count);
    client_notify.notify_one();
}

/// Reads client messages and dispatches them without waiting on frame
/// delivery. Input reaches CDP sequentially: mouse move, press and release
/// must not be reordered.
#[allow(clippy::too_many_arguments)]
async fn reader_loop(
    mut ws_rx: SplitStream<WebSocketStream<TcpStream>>,
    client_slot: Arc<RwLock<Option<Arc<CdpClient>>>>,
    cdp_session_id: Arc<RwLock<Option<String>>>,
    config: watch::Sender<ClientConfig>,
    ack: watch::Sender<u64>,
    idle_activity: Arc<IdleActivity>,
    browser_control: Arc<Mutex<BrowserControl>>,
    input_errors: watch::Sender<Option<Value>>,
    presentation: Arc<Presentation>,
    connection_id: uuid::Uuid,
) {
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let msg_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if msg_type == "presentation" {
                    let current = *config.borrow();
                    if let Some(next) = updated_presentation(current, &parsed) {
                        // Publish the connection's new dimensions first so an
                        // acknowledgment cannot reclaim its previous size.
                        config.send_replace(next);
                        presentation.configure(connection_id, next.presentation.unwrap());
                        idle_activity.mark();
                    }
                    continue;
                }
                if msg_type == "config" {
                    // Bound to a local first: a `watch` Ref taken in the
                    // `if let` scrutinee lives for the whole body, and
                    // `send_replace` would deadlock against it.
                    let current = *config.borrow();
                    if let Some(next) = apply_config(current, &parsed) {
                        // send_replace, not send, so an unchanged value still
                        // wakes the writer to re-derive its deadline.
                        let _ = config.send_replace(next);
                    }
                    continue;
                }
                if msg_type == "ack" {
                    if let Some(seq) = parse_ack_seq(&parsed) {
                        // send_if_modified keeps the acknowledged id monotonic:
                        // a late ack for an older frame must not walk it back
                        // and re-block the writer.
                        ack.send_if_modified(|current| {
                            if seq > *current {
                                *current = seq;
                                true
                            } else {
                                false
                            }
                        });
                    }
                    continue;
                }
                if !is_user_input_message_type(msg_type) {
                    continue;
                }
                let mut control = browser_control.lock().await;
                let guard = client_slot.read().await;
                if let Some(ref client) = *guard {
                    let sid = cdp_session_id.read().await;
                    match control
                        .stream_input(msg_type, &parsed, client.as_ref(), sid.as_deref())
                        .await
                    {
                        Ok(()) => idle_activity.mark(),
                        Err(error) => {
                            input_errors.send_replace(Some(json!({
                                "type": "input_error", "code": error.code, "error": error.message,
                            })));
                        }
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}

/// Aborts its task on drop.
///
/// Invariant: the reader is spawned inside the connection task, so aborting the
/// connection does not reach it. Without this guard the reader outlives
/// teardown and keeps dispatching input.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn is_user_input_message_type(msg_type: &str) -> bool {
    matches!(msg_type, "input_mouse" | "input_keyboard" | "input_touch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_input_messages_count_as_user_activity() {
        for msg_type in ["input_mouse", "input_keyboard", "input_touch"] {
            assert!(is_user_input_message_type(msg_type));
        }
        assert!(!is_user_input_message_type("status"));
        assert!(!is_user_input_message_type("frame"));
    }

    fn fps_of(msg: serde_json::Value) -> Option<u32> {
        apply_config(ClientConfig::default(), &msg).map(|c| c.max_fps)
    }

    #[test]
    fn test_parse_config_max_fps_valid() {
        assert_eq!(fps_of(json!({"type": "config", "maxFps": 10})), Some(10));
        assert_eq!(fps_of(json!({"type": "config", "maxFps": 0})), Some(0));
        assert_eq!(fps_of(json!({"type": "config", "maxFps": 120})), Some(120));
    }

    #[test]
    fn test_parse_config_max_fps_clamps_without_wrapping() {
        assert_eq!(
            fps_of(json!({"type": "config", "maxFps": 500})),
            Some(MAX_CONFIGURABLE_FPS)
        );
        // u32::MAX + 2 would wrap to 1 if narrowed before clamping.
        assert_eq!(
            fps_of(json!({"type": "config", "maxFps": 4294967297u64})),
            Some(MAX_CONFIGURABLE_FPS)
        );
    }

    #[test]
    fn test_parse_config_max_fps_invalid() {
        assert_eq!(
            apply_config(ClientConfig::default(), &json!({"type": "config"})),
            None
        );
        assert_eq!(fps_of(json!({"type": "config", "maxFps": -5})), None);
        assert_eq!(fps_of(json!({"type": "config", "maxFps": "fast"})), None);
    }

    #[test]
    fn test_config_pacing_opt_in_and_out() {
        let acked = apply_config(
            ClientConfig::default(),
            &json!({"type": "config", "pacing": "ack"}),
        )
        .expect("pacing is a recognized setting");
        assert!(acked.ack_pacing);

        let back_to_push = apply_config(acked, &json!({"type": "config", "pacing": "push"}))
            .expect("push is a recognized setting");
        assert!(!back_to_push.ack_pacing);

        // An unknown pacing value leaves the connection as it was.
        assert_eq!(
            apply_config(acked, &json!({"type": "config", "pacing": "turbo"})),
            None
        );
    }

    #[test]
    fn test_config_settings_are_independent() {
        // Changing the cap must not drop the client out of ack pacing.
        let acked = apply_config(
            ClientConfig::default(),
            &json!({"type": "config", "pacing": "ack"}),
        )
        .unwrap();
        let recapped = apply_config(acked, &json!({"type": "config", "maxFps": 15})).unwrap();
        assert_eq!(recapped.max_fps, 15);
        assert!(recapped.ack_pacing);

        let repaced = apply_config(recapped, &json!({"type": "config", "pacing": "push"})).unwrap();
        assert_eq!(repaced.max_fps, 15);
        assert!(!repaced.ack_pacing);
    }

    fn upgrade(target: &str) -> String {
        format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\r\n",
            target
        )
    }

    #[test]
    fn test_config_from_upgrade_reads_url_settings() {
        let cfg = config_from_upgrade(&upgrade("/?pacing=ack&maxFps=10"));
        assert!(cfg.ack_pacing);
        assert_eq!(cfg.max_fps, 10);

        let cfg = config_from_upgrade(&upgrade("/?maxFps=5"));
        assert!(!cfg.ack_pacing);
        assert_eq!(cfg.max_fps, 5);
    }

    #[test]
    fn test_config_from_upgrade_defaults_when_absent_or_unparsable() {
        for target in [
            "/",
            "/?",
            "/?pacing=turbo",
            "/?maxFps=fast",
            "/?maxFps=-1",
            "/?other=1",
        ] {
            assert_eq!(
                config_from_upgrade(&upgrade(target)),
                ClientConfig::default(),
                "target {} should leave the defaults alone",
                target
            );
        }
    }

    #[test]
    fn test_config_from_upgrade_reads_patch_compositing() {
        assert!(config_from_upgrade(&upgrade("/?patches=1")).patches);
        assert!(!config_from_upgrade(&upgrade("/?patches=0")).patches);
        assert!(!config_from_upgrade(&upgrade("/")).patches);
    }

    #[test]
    fn frame_window_is_bounded_by_count_and_bytes_and_released_cumulatively() {
        let mut flight = InFlightFrames::default();
        let now = Instant::now();
        flight.sent(10, MAX_WINDOW_BYTES / 2, now, true);
        flight.sent(12, MAX_WINDOW_BYTES / 2, now, true);
        assert!(!flight.has_slot(2));
        assert!(flight.has_slot(8));
        assert!(!flight.has_bytes(1));
        flight.acknowledge(10, now, false);
        assert!(flight.has_bytes(MAX_WINDOW_BYTES / 2));
        assert!(!flight.has_bytes(MAX_WINDOW_BYTES / 2 + 1));
        flight.acknowledge(12, now, false);
        assert!(flight.frames.is_empty());
        assert_eq!(flight.bytes, 0);
        assert_eq!(
            config_from_upgrade(&upgrade("/?frameWindow=8")).frame_window,
            8
        );
        for invalid in ["0", "9", "-1", "bad"] {
            assert_eq!(
                config_from_upgrade(&upgrade(&format!("/?frameWindow={invalid}"))).frame_window,
                1
            );
        }
    }

    /// What the viewer does before a run of whole frames.
    #[derive(Clone, Copy)]
    enum Prelude {
        None,
        /// Six seconds of typing: a 2 KB patch every 50 ms, each alone on the path.
        Typing,
    }

    /// One viewer behind a bottleneck: whole frames are produced at 60 fps
    /// (latest wins), the link serializes at `rate` bytes/ms and a paint ACK
    /// returns `transit` ms after a frame arrives. Returns delivered fps and
    /// the median age (capture to paint) of the whole frames painted from
    /// 4 s to 20 s after the prelude.
    fn simulate_whole_frames(
        rate: f64,
        transit: f64,
        bytes: usize,
        bounded: bool,
        prelude: Prelude,
    ) -> (f64, f64) {
        let origin = Instant::now();
        let at = |ms: f64| origin + Duration::from_micros((ms * 1000.0) as u64);
        let start = match prelude {
            Prelude::None => 0.0,
            Prelude::Typing => 6_000.0,
        };
        let mut flight = InFlightFrames::default();
        // (seq, captured at, painted/acknowledged at, whole)
        let mut travelling: VecDeque<(u64, f64, f64, bool)> = VecDeque::new();
        let (mut link_free, mut seq) = (0.0_f64, 0_u64);
        let (mut next_capture, mut next_patch) = (start, 0.0_f64);
        let mut pending: Option<(u64, f64)> = None;
        let mut ages = Vec::new();
        let mut t = 0.0;
        while t < start + 20_000.0 {
            while travelling.front().is_some_and(|(_, _, done, _)| *done <= t) {
                let (acked, captured, done, whole) = travelling.pop_front().unwrap();
                flight.acknowledge(acked, at(done), true);
                if whole && t > start + 4_000.0 {
                    ages.push(done - captured);
                }
            }
            // (seq, captured at, bytes): a patch leaves at once while typing;
            // then the newest whole frame leaves once admitted.
            let mut outgoing = None;
            if t < start {
                if t >= next_patch {
                    seq += 1;
                    outgoing = Some((seq, t, 2_000));
                    next_patch += 50.0;
                }
            } else {
                if t >= next_capture {
                    seq += 1;
                    pending = Some((seq, t));
                    next_capture += 1000.0 / 60.0;
                }
                if let Some((frame, captured)) = pending {
                    let admitted = flight.has_slot(MAX_FRAME_WINDOW)
                        && if bounded {
                            flight.admits(bytes, true)
                        } else {
                            flight.has_bytes(bytes)
                        };
                    if admitted {
                        outgoing = Some((frame, captured, bytes));
                        pending = None;
                    }
                }
            }
            if let Some((frame, captured, size)) = outgoing {
                let whole = size == bytes;
                link_free = link_free.max(t) + size as f64 / rate;
                flight.sent(frame, size, at(t), whole);
                travelling.push_back((frame, captured, link_free + transit, whole));
            }
            t += 0.25;
        }
        let painted = ages.len() as f64 / 16.0;
        ages.sort_by(f64::total_cmp);
        (painted, ages[ages.len() / 2])
    }

    #[test]
    fn delivery_bound_keeps_throughput_and_removes_the_standing_queue() {
        // 20 Mbit/s and 50 Mbit/s paths, 40 ms transit, 150 KB whole frames:
        // the negotiated window alone lets eight frames queue ahead of the
        // newest; the delivery bound keeps about one without losing a frame.
        // After typing too: patches travel alone, and had their paints
        // measured the path, a scroll right after would have been held to
        // about one frame per round trip (14 of 16.6 fps at 20 Mbit/s, 32 of
        // 42 at 50 Mbit/s) until the patch samples aged out.
        for prelude in [Prelude::None, Prelude::Typing] {
            for (rate, window_age) in [(2_500.0, 400.0), (6_250.0, 180.0)] {
                let (fps, age) = simulate_whole_frames(rate, 40.0, 150_000, false, prelude);
                let (bounded_fps, bounded_age) =
                    simulate_whole_frames(rate, 40.0, 150_000, true, prelude);
                assert!(age >= window_age, "window-only age {age} ms at {rate} B/ms");
                assert!(
                    bounded_fps >= fps * 0.97,
                    "bounded {bounded_fps} fps vs window-only {fps} fps at {rate} B/ms"
                );
                assert!(
                    bounded_age <= age * 0.55,
                    "bounded age {bounded_age} ms vs window-only {age} ms at {rate} B/ms"
                );
            }
        }
        // A path faster than the producer never waits on the bound.
        let (fps, age) = simulate_whole_frames(25_000.0, 40.0, 150_000, false, Prelude::None);
        let (bounded_fps, bounded_age) =
            simulate_whole_frames(25_000.0, 40.0, 150_000, true, Prelude::None);
        assert!(bounded_fps >= fps * 0.99 && bounded_age <= age + 1.0);
    }

    #[test]
    fn delivery_bound_spares_patches_and_a_lone_frame() {
        let origin = Instant::now();
        let mut flight = InFlightFrames::default();
        // No acknowledgement yet: only the negotiated bounds apply.
        flight.sent(1, 100_000, origin, true);
        assert!(flight.admits(100_000, true));
        // A 100 KB frame painted 100 ms after it left: 1 KB/ms over 100 ms.
        flight.acknowledge(1, origin + Duration::from_millis(100), true);
        assert_eq!(flight.bound(), Some(100_000));
        let later = origin + Duration::from_millis(200);
        flight.sent(2, 120_000, later, true);
        // 120 KB ahead exceeds the 100 KB bound for a whole frame...
        assert!(!flight.admits(150_000, true));
        // ...but a patch still leaves, so its successor keeps its base.
        assert!(flight.admits(8_000, false));
        // Alone on the path, any frame within the hard bound leaves.
        flight.acknowledge(2, later + Duration::from_millis(150), true);
        assert!(flight.frames.is_empty());
        assert!(flight.admits(MAX_WINDOW_BYTES, true));
        assert!(!flight.admits(MAX_WINDOW_BYTES + 1, true));
    }

    #[test]
    fn only_paint_acknowledgements_are_delivery_samples() {
        let origin = Instant::now();
        let mut flight = InFlightFrames::default();
        flight.sent(4, 50_000, origin, true);
        // Re-applying a watermark at send time releases without measuring.
        flight.acknowledge(4, origin + Duration::from_millis(3), false);
        assert!(flight.bound().is_none());
        // An acknowledgement at the send instant carries no round trip.
        flight.sent(5, 50_000, origin, true);
        flight.acknowledge(5, origin, true);
        assert!(flight.bound().is_none());
        // A patch's paint measures only what that patch needed.
        flight.sent(6, 2_000, origin, false);
        flight.acknowledge(6, origin + Duration::from_millis(40), true);
        assert!(flight.bound().is_none());
        // A cumulative paint that also releases patches measures the newest
        // whole frame it released.
        flight.sent(7, 50_000, origin, true);
        flight.sent(8, 2_000, origin + Duration::from_millis(10), false);
        flight.acknowledge(8, origin + Duration::from_millis(50), true);
        assert_eq!(flight.round_trips.len(), 1);
        assert_eq!(flight.round_trips[0].1, Duration::from_millis(50));
        // Stale samples age out relative to the newest, never to nothing.
        let late = origin + ROUND_TRIP_WINDOW + Duration::from_secs(1);
        flight.sent(9, 10_000, late, true);
        flight.acknowledge(9, late + Duration::from_millis(80), true);
        assert_eq!(flight.round_trips.len(), 1);
        assert_eq!(flight.round_trips[0].1, Duration::from_millis(80));
        assert!(flight.bound().is_some());
    }

    #[test]
    fn presentation_updates_keep_upgrade_identity_and_bounds() {
        let viewer = uuid::Uuid::new_v4();
        let original = ClientConfig {
            presentation: Some(PresentationConfig {
                viewer,
                width: 800,
                height: 600,
            }),
            binary: true,
            ..ClientConfig::default()
        };
        let changed = updated_presentation(
            original,
            &json!({"type":"presentation","width":390,"height":844,"viewer":"different"}),
        )
        .unwrap();
        assert_eq!(
            changed.presentation.unwrap(),
            PresentationConfig {
                viewer,
                width: 390,
                height: 844
            }
        );
        assert!(changed.binary);
        assert!(
            updated_presentation(ClientConfig::default(), &json!({"width":390,"height":844}))
                .is_none()
        );
        for message in [
            json!({"width":0,"height":844}),
            json!({"width":2049,"height":844}),
            json!({"width":390,"height":-1}),
            json!({"width":"390","height":844}),
        ] {
            assert!(updated_presentation(original, &message).is_none());
        }
        assert!(config_from_upgrade(&upgrade("/?frames=binary")).binary);
        assert!(!config_from_upgrade(&upgrade("/?frames=other")).binary);
    }

    #[test]
    fn test_config_from_upgrade_clamps_max_fps() {
        assert_eq!(
            config_from_upgrade(&upgrade("/?maxFps=100000")).max_fps,
            MAX_CONFIGURABLE_FPS
        );
        // Wider than u32: must clamp rather than wrap or fall back to the default.
        assert_eq!(
            config_from_upgrade(&upgrade("/?maxFps=4294967297")).max_fps,
            MAX_CONFIGURABLE_FPS
        );
    }

    #[test]
    fn test_parse_ack_seq() {
        assert_eq!(parse_ack_seq(&json!({"type": "ack", "seq": 42})), Some(42));
        assert_eq!(parse_ack_seq(&json!({"type": "ack"})), None);
        assert_eq!(parse_ack_seq(&json!({"type": "ack", "seq": -1})), None);
        assert_eq!(parse_ack_seq(&json!({"type": "ack", "seq": "42"})), None);
    }

    /// Guards the omit-never-null rule: a null string makes CDP reject the
    /// whole command, which silently drops the keystroke.
    /// A CDP endpoint that records every command and never replies, so a
    /// dispatcher that waits for a response hangs on the first event.
    async fn silent_cdp_server() -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        use futures_util::StreamExt;
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/devtools/browser/silent",
            listener.local_addr().unwrap().port()
        );
        let recorded = seen.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (_tx, mut rx) = ws.split();
            while let Some(Ok(Message::Text(text))) = rx.next().await {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                        recorded.lock().await.push(m.to_string());
                    }
                }
            }
        });
        (url, seen)
    }

    /// Guards the no-await dispatch, and unlike the e2e it runs on every PR.
    ///
    /// Regression: awaiting Chrome's reply per event serialized the reader, so a
    /// click waited one round trip per queued move. Against a server that never
    /// replies, the awaiting version cannot get past the first event at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_input_dispatch_does_not_wait_for_a_cdp_reply() {
        let (url, seen) = silent_cdp_server().await;
        let client = CdpClient::connect(&url).await.expect("mock cdp connect");

        let events = [
            (
                "input_mouse",
                json!({ "eventType": "mouseMoved", "x": 1, "y": 1 }),
            ),
            (
                "input_mouse",
                json!({ "eventType": "mousePressed", "x": 1, "y": 1 }),
            ),
            (
                "input_keyboard",
                json!({ "eventType": "keyDown", "key": "a", "text": "a" }),
            ),
        ];
        let mut control = BrowserControl::default();
        let dispatched = tokio::time::timeout(Duration::from_secs(5), async {
            for (kind, payload) in &events {
                control
                    .stream_input(kind, payload, &client, None)
                    .await
                    .unwrap();
            }
        })
        .await;
        assert!(
            dispatched.is_ok(),
            "dispatch blocked on a CDP reply that never comes"
        );

        // The commands still reach CDP, in the order they were dispatched.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if seen.lock().await.len() >= 3 || tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            *seen.lock().await,
            vec![
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
                "Input.dispatchKeyEvent"
            ],
            "input should reach CDP in dispatch order"
        );

        // Bounded receipts retain custody evidence without slowing the sender.
        assert_eq!(client.pending_len().await, 3);
        drop(control);
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.pending_len().await != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped receipts must clear the pending map");
        assert_eq!(client.pending_len().await, 0);
    }

    #[test]
    fn test_keyboard_params_omit_absent_strings_instead_of_sending_null() {
        // The dashboard's keyUp shape carries no text at all.
        let up = keyboard_params(&json!({
            "eventType": "keyUp", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65
        }));
        assert_eq!(up["type"], "keyUp");
        assert_eq!(up["key"], "a");
        assert!(
            up.get("text").is_none(),
            "absent text must be omitted: {}",
            up
        );

        // The documented `char` shape carries only text.
        let ch = keyboard_params(&json!({ "eventType": "char", "text": "z" }));
        assert_eq!(ch["text"], "z");
        assert!(ch.get("key").is_none() && ch.get("code").is_none());

        // A non-string value is dropped rather than forwarded.
        let bad = keyboard_params(&json!({ "eventType": "keyDown", "key": 5, "code": null }));
        assert!(bad.get("key").is_none() && bad.get("code").is_none());

        // Numeric fields keep defaults so CDP always receives them.
        assert_eq!(bad["windowsVirtualKeyCode"], 0);
        assert_eq!(bad["modifiers"], 0);
        assert_eq!(keyboard_params(&json!({}))["type"], "keyDown");
    }

    /// A cap bounds the gap between deliveries and never postpones the first.
    #[test]
    fn test_deadline_charges_only_real_deliveries() {
        let now = Instant::now();
        assert!(deadline_from(None, 1) <= Instant::now());
        assert!(deadline_from(None, 0) <= Instant::now());
        assert_eq!(deadline_from(Some(now), 0), now);
        assert_eq!(
            deadline_from(Some(now), 2),
            now + Duration::from_millis(500)
        );
        assert_eq!(
            deadline_from(Some(now), 10),
            now + Duration::from_millis(100)
        );
    }
}
