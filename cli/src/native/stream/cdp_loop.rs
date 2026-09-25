use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;
use tokio::sync::{broadcast, watch, Mutex, RwLock};

use crate::native::browser_control::custody_active;
use crate::native::cdp::client::CdpClient;
use crate::native::cdp::types::CdpEvent;
use crate::native::display::CaptureRequest;
use crate::native::network;

use super::timestamp_ms;

/// Capture time of a screencast frame, in epoch milliseconds.
///
/// CDP sends `Network.TimeSinceEpoch`, a float in seconds; reading it as an
/// integer yields 0 for every frame. Milliseconds match this protocol's other
/// timestamps.
fn frame_timestamp_ms(meta: Option<&Value>) -> u64 {
    meta.and_then(|m| m.get("timestamp"))
        .and_then(|v| v.as_f64())
        .filter(|s| *s > 0.0)
        .map(|s| (s * 1000.0) as u64)
        .unwrap_or(0)
}

fn session_matches(active_session: Option<&str>, event_session: Option<&str>) -> bool {
    match active_session {
        Some("") => event_session.is_none_or(str::is_empty),
        Some(active) => event_session == Some(active),
        None => false,
    }
}

fn main_frame_id(frame_tree: &Value) -> Option<String> {
    frame_tree
        .get("frameTree")
        .and_then(|tree| tree.get("frame"))
        .and_then(|frame| frame.get("id"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// The active main frame, observed through `client`; never without one.
async fn seed_main_frame_id(
    client: Option<Arc<CdpClient>>,
    session_id: Option<String>,
    delay: std::time::Duration,
) -> (Arc<CdpClient>, Option<String>) {
    let Some(client) = client else {
        return std::future::pending().await;
    };
    tokio::time::sleep(delay).await;
    let frame_id = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.send_command_no_params("Page.getFrameTree", session_id.as_deref()),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .and_then(|tree| main_frame_id(&tree));
    (client, frame_id)
}

/// The CDP half of a stream session: its client and that client's events.
type CdpEvents = (Arc<CdpClient>, broadcast::Receiver<CdpEvent>);

/// The next event of this session's CDP client, with that client; never for
/// a window shown without DevTools.
async fn next_cdp_event(
    cdp: &mut Option<CdpEvents>,
) -> (
    Arc<CdpClient>,
    Result<CdpEvent, broadcast::error::RecvError>,
) {
    match cdp {
        Some((client, events)) => (Arc::clone(client), events.recv().await),
        None => std::future::pending().await,
    }
}

/// The next change of an optional revision; never while there is none. A
/// closed revision ends: its owner was replaced with the client.
async fn revision_changed(revision: &mut Option<watch::Receiver<u64>>) {
    match revision {
        Some(receiver) => {
            if receiver.changed().await.is_err() {
                *revision = None;
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

/// A file picker opened or ended, or a download settled: the controller
/// asks for the current `files` once instead of polling. It names nothing.
fn files_doorbell() -> String {
    json!({ "type": "files", "ts": super::monotonic_us() }).to_string()
}

/// Whether a stream source slot now holds another value than `current`.
pub(super) fn replaced<T>(current: &Option<Arc<T>>, next: &Option<Arc<T>>) -> bool {
    match (current, next) {
        (Some(current), Some(next)) => !Arc::ptr_eq(current, next),
        (None, None) => false,
        _ => true,
    }
}

async fn publish_url(
    client: &CdpClient,
    frame_tx: &broadcast::Sender<String>,
    last_tabs: &RwLock<Vec<Value>>,
    cdp_session_id: &RwLock<Option<String>>,
    event_session_id: Option<&str>,
    url: &str,
) {
    let history = super::history_availability(client, event_session_id).await;
    let active_session = cdp_session_id.read().await;
    if !session_matches(active_session.as_deref(), event_session_id) {
        return;
    }
    {
        let mut tabs = last_tabs.write().await;
        for tab in tabs.iter_mut() {
            if tab.get("active").and_then(Value::as_bool).unwrap_or(false) {
                if let Some(tab) = tab.as_object_mut() {
                    tab.insert("url".to_string(), json!(url));
                    tab.remove("canGoBack");
                    tab.remove("canGoForward");
                    if let Some((back, forward)) = history {
                        tab.insert("canGoBack".into(), json!(back));
                        tab.insert("canGoForward".into(), json!(forward));
                    }
                }
            }
        }
    }
    let mut message = json!({
        "type": "url",
        "url": url,
        "timestamp": timestamp_ms(),
    });
    if let Some((back, forward)) = history {
        message["canGoBack"] = json!(back);
        message["canGoForward"] = json!(forward);
    }
    let _ = frame_tx.send(message.to_string());
}

/// Streams the current source to its viewers: the owned window's display,
/// the active page's CDP events, or both. A browser running without DevTools
/// streams its window alone.
///
/// Frames use `frame_watch` so the latest value wins. Other messages stay on
/// the ordered `frame_tx` channel. URL updates follow only the active main
/// frame. Chrome also includes History API and fragment navigation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn cdp_event_loop(
    frame_tx: broadcast::Sender<String>,
    frame_watch: watch::Sender<Option<Arc<super::StreamFrame>>>,
    screencast_config: Arc<super::ScreencastConfig>,
    client_slot: Arc<RwLock<Option<Arc<CdpClient>>>>,
    display_slot: Arc<RwLock<Option<Arc<crate::native::display::DisplayClient>>>>,
    presentation: Arc<super::presentation::Presentation>,
    mut custody: watch::Receiver<Option<Instant>>,
    media: Arc<super::StreamMedia>,
    client_notify: Arc<tokio::sync::Notify>,
    screencasting: Arc<Mutex<bool>>,
    client_count: Arc<Mutex<usize>>,
    patch_clients: Arc<std::sync::atomic::AtomicUsize>,
    cdp_session_id: Arc<RwLock<Option<String>>>,
    viewport_width: Arc<Mutex<u32>>,
    viewport_height: Arc<Mutex<u32>>,
    last_tabs: Arc<RwLock<Vec<Value>>>,
    last_engine: Arc<RwLock<String>>,
    recording: Arc<Mutex<bool>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    let session_id = cdp_session_id.read().await.clone();
                    if *screencasting.lock().await {
                        if let Some(ref client) = *client_slot.read().await {
                            let _ = client
                                .send_command_no_params("Page.stopScreencast", session_id.as_deref())
                                .await;
                        }
                        let mut sc = screencasting.lock().await;
                        *sc = false;
                    }
                    return;
                }
            }
            _ = client_notify.notified() => {}
        }

        let count = *client_count.lock().await;
        let client = client_slot.read().await.clone();
        let display = display_slot.read().await.clone();

        if count > 0 && (client.is_some() || display.is_some()) {
            let mut cdp = client
                .as_ref()
                .map(|client| (Arc::clone(client), client.subscribe()));
            let mut file_revision = client.as_ref().map(|client| client.files.subscribe());
            let mut download_revision = client
                .as_ref()
                .map(|client| client.downloads.subscribe_settled());

            let session_id = cdp_session_id.read().await.clone();

            let vw = *viewport_width.lock().await;
            let vh = *viewport_height.lock().await;

            let eng = last_engine.read().await.clone();
            let is_chrome = eng == "chrome";
            // A page screencast runs only when no owned window is shown.
            let screencast = client.clone().filter(|_| is_chrome && display.is_none());
            let supports_screencast = screencast.is_some();
            let supports_same_document_navigation = is_chrome && client.is_some();

            if let Some(client) = &screencast {
                let _ = client
                    .send_command(
                        "Page.startScreencast",
                        Some(json!({
                            "format": "jpeg",
                            "quality": screencast_config.quality,
                            "maxWidth": screencast_config.max_width.unwrap_or(vw),
                            "maxHeight": screencast_config.max_height.unwrap_or(vh),
                            "everyNthFrame": 1,
                        })),
                        session_id.as_deref(),
                    )
                    .await;
            }

            {
                let mut sc = screencasting.lock().await;
                *sc = supports_screencast || display.is_some();
            }

            let rec = *recording.lock().await;
            let status = json!({
                "type": "status",
                "connected": true,
                "screencasting": supports_screencast || display.is_some(),
                "viewportWidth": vw,
                "viewportHeight": vh,
                "engine": eng,
                "recording": rec,
            });
            let _ = frame_tx.send(status.to_string());

            let frame_tree_seed = seed_main_frame_id(
                client.clone().filter(|_| supports_same_document_navigation),
                session_id.clone(),
                std::time::Duration::ZERO,
            )
            .fuse();
            tokio::pin!(frame_tree_seed);
            let mut seed_in_flight = supports_same_document_navigation;
            let mut active_main_frame_id = None;
            let mut pending_same_document = VecDeque::<(Option<String>, String, String)>::new();
            let mut presentation_rx = presentation.subscribe();
            // Pacing follows the viewing situation: the presenter roster
            // and the human lease. Both are re-read at every tick, so a
            // lease that lapses without a message still slows capture.
            let mut controlled = custody_active(*custody.borrow_and_update());
            let mut pacing = presentation.capture_pacing(controlled);
            let mut display_tick = tokio::time::interval(pacing.period());
            display_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut display_failed = false;
            // The generation the newest published frame carries. A rotated
            // generation is republished on unchanged pixels so viewers can
            // name the current surface in their next input.
            let mut published_generation: Option<String> = None;
            // The newest published frame was a patch; a new or changed
            // viewer roster then needs a whole frame first.
            let mut published_patches = false;
            let mut published_seq = None;
            macro_rules! repace {
                () => {{
                    let next = presentation.capture_pacing(controlled);
                    if next != pacing {
                        pacing = next;
                        display_tick = tokio::time::interval_at(
                            tokio::time::Instant::now() + pacing.period(),
                            pacing.period(),
                        );
                        display_tick
                            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                }};
            }

            loop {
                tokio::select! {
                    changed = presentation_rx.changed(), if display.is_some() => {
                        if changed.is_err() { break; }
                        repace!();
                        // A viewer's layout just changed or was applied: the
                        // first frame of the new geometry must not wait out
                        // the remainder of the current capture interval.
                        display_tick.reset_immediately();
                    }
                    changed = custody.changed(), if display.is_some() => {
                        if changed.is_err() { break; }
                        controlled = custody_active(*custody.borrow_and_update());
                        repace!();
                    }
                    _ = revision_changed(&mut file_revision) => {
                        let _ = frame_tx.send(files_doorbell());
                    }
                    _ = revision_changed(&mut download_revision) => {
                        let _ = frame_tx.send(files_doorbell());
                    }
                    _ = display_tick.tick(), if display.is_some() && !display_failed => {
                        let now_controlled = custody_active(*custody.borrow());
                        if now_controlled != controlled {
                            controlled = now_controlled;
                            repace!();
                        }
                        let display = display.as_ref().unwrap();
                        // Patches amend a whole frame every viewer holds;
                        // one viewer that does not composite them makes
                        // the next frame whole for everyone.
                        let patches_allowed = patch_clients.load(std::sync::atomic::Ordering::Acquire) == *client_count.lock().await;
                        if !patches_allowed && published_patches {
                            published_generation = None;
                        }
                        let request = CaptureRequest {
                            // A controlling client renders its own pointer.
                            cursor: !controlled,
                            budget_bytes: pacing.budget_bytes,
                            force: published_generation.as_deref() != Some(display.surface().generation.as_str()),
                            patches: patches_allowed,
                        };
                        // Read before the request leaves: input acknowledged
                        // by then is in every pixel the helper fetches.
                        let ts = super::monotonic_us();
                        let input_seq = media.applied_input();
                        match display.capture(request).await {
                            Ok(Some((capture, mut surface))) => {
                                let seq = super::next_frame_seq();
                                let patch = capture.data.is_none();
                                surface.cursor_included = capture.cursor_included;
                                let visible = capture.visible.unwrap_or(crate::native::display::Rect {
                                    x: 0, y: 0, width: surface.width, height: surface.height,
                                });
                                let mut message = json!({
                                    "type": "frame", "seq": seq, "encoding": capture.encoding,
                                    "surface": surface, "ts": ts, "visible": visible,
                                });
                                if let Some(input_seq) = input_seq {
                                    message["inputSeq"] = json!(input_seq);
                                }
                                if let Some(data) = capture.data {
                                    message["data"] = json!(data);
                                } else {
                                    message["patches"] = json!(capture.patches);
                                    message["baseSeq"] = json!(published_seq);
                                }
                                published_generation = Some(surface.generation);
                                published_patches = patch;
                                frame_watch.send_replace(Some(Arc::new(super::StreamFrame {
                                    seq: Some(seq), json: message.to_string(), patch,
                                    base_seq: if patch { published_seq } else { None },
                                    binary: std::sync::OnceLock::new(),
                                })));
                                published_seq = Some(seq);
                            }
                            Ok(None) => {}
                            Err(error) if error.is_transient() => {}
                            Err(_) => {
                                display_failed = true;
                                frame_watch.send_replace(None);
                                let _ = frame_tx.send(json!({"type":"error", "code":"display_unavailable"}).to_string());
                            }
                        }
                    }
                    (client_arc, seeded_frame_id) = &mut frame_tree_seed => {
                        seed_in_flight = false;
                        if active_main_frame_id.is_none() {
                            active_main_frame_id = seeded_frame_id;
                        }
                        if let Some(main_frame_id) = active_main_frame_id.as_deref() {
                            for (event_session_id, frame_id, url) in
                                pending_same_document.drain(..)
                            {
                                if frame_id == main_frame_id {
                                    publish_url(
                                        &client_arc,
                                        &frame_tx,
                                        &last_tabs,
                                        &cdp_session_id,
                                        event_session_id.as_deref(),
                                        &url,
                                    )
                                    .await;
                                }
                            }
                        } else if !pending_same_document.is_empty() {
                            frame_tree_seed.set(
                                seed_main_frame_id(
                                    Some(Arc::clone(&client_arc)),
                                    session_id.clone(),
                                    std::time::Duration::from_millis(250),
                                )
                                .fuse(),
                            );
                            seed_in_flight = true;
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            if let Some(client) = &screencast {
                                let session_id = cdp_session_id.read().await.clone();
                                let _ = client
                                    .send_command_no_params("Page.stopScreencast", session_id.as_deref())
                                    .await;
                            }
                            let mut sc = screencasting.lock().await;
                            *sc = false;
                            return;
                        }
                    }
                    (client_arc, event) = next_cdp_event(&mut cdp) => {
                        match event {
                            Ok(evt) => {
                                if evt.method == crate::native::activity::EVENT {
                                    if session_matches(session_id.as_deref(), evt.session_id.as_deref()) {
                                        let mut activity = evt.params;
                                        if display.is_some() {
                                            activity["coordinateSpace"] = json!("viewport-css");
                                        }
                                        if let (Some(display), Some(screen_x), Some(screen_y), Some(page)) = (
                                            display.as_ref(), activity["screenX"].as_f64(), activity["screenY"].as_f64(), evt.session_id.as_deref()
                                        ) {
                                            let surface = display.surface();
                                            let x = screen_x * f64::from(surface.device_scale_factor);
                                            let y = screen_y * f64::from(surface.device_scale_factor);
                                            if activity["source"] == "agent"
                                                && activity["pageGeneration"] == client_arc.page_generation(page)
                                                && x >= 0.0 && y >= 0.0 && x < f64::from(surface.width) && y < f64::from(surface.height) {
                                                activity["coordinateSpace"] = json!("display-pixels");
                                                activity["surfaceGeneration"] = json!(surface.generation);
                                                activity["x"] = json!(x);
                                                activity["y"] = json!(y);
                                            }
                                        }
                                        if let Some(object) = activity.as_object_mut() {
                                            object.remove("screenX");
                                            object.remove("screenY");
                                        }
                                        let _ = frame_tx.send(activity.to_string());
                                    }
                                } else if evt.method == "Page.frameNavigated" {
                                    if let Some(frame) = evt.params.get("frame") {
                                        let is_main = frame
                                            .get("parentId")
                                            .and_then(|v| v.as_str())
                                            .is_none_or(|s| s.is_empty());
                                        let is_active_session = session_matches(
                                            session_id.as_deref(),
                                            evt.session_id.as_deref(),
                                        );
                                        if is_main && is_active_session {
                                            if supports_screencast {
                                                pending_same_document.clear();
                                                active_main_frame_id = frame
                                                    .get("id")
                                                    .and_then(Value::as_str)
                                                    .map(String::from);
                                            }
                                            if let Some(url) = frame.get("url").and_then(|v| v.as_str()) {
                                                publish_url(
                                                    &client_arc,
                                                    &frame_tx,
                                                    &last_tabs,
                                                    &cdp_session_id,
                                                    evt.session_id.as_deref(),
                                                    url,
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                } else if evt.method == "Page.navigatedWithinDocument" {
                                    let is_active_session = supports_same_document_navigation
                                        && session_matches(
                                            session_id.as_deref(),
                                            evt.session_id.as_deref(),
                                        );
                                    if is_active_session {
                                        if let (Some(frame_id), Some(url)) = (
                                            evt.params.get("frameId").and_then(Value::as_str),
                                            evt.params.get("url").and_then(Value::as_str),
                                        ) {
                                            if active_main_frame_id.is_none() {
                                                if pending_same_document.len() == 64 {
                                                    pending_same_document.pop_front();
                                                }
                                                pending_same_document
                                                    .push_back((
                                                        evt.session_id.clone(),
                                                        frame_id.to_string(),
                                                        url.to_string(),
                                                    ));
                                                if !seed_in_flight {
                                                    frame_tree_seed.set(
                                                        seed_main_frame_id(
                                                            Some(Arc::clone(&client_arc)),
                                                            session_id.clone(),
                                                            std::time::Duration::ZERO,
                                                        )
                                                        .fuse(),
                                                    );
                                                    seed_in_flight = true;
                                                }
                                            } else if Some(frame_id)
                                                == active_main_frame_id.as_deref()
                                            {
                                                publish_url(
                                                    &client_arc,
                                                    &frame_tx,
                                                    &last_tabs,
                                                    &cdp_session_id,
                                                    evt.session_id.as_deref(),
                                                    url,
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                } else if evt.method == "Page.screencastFrame" {
                                    if let Some(sid) = evt.params.get("sessionId").and_then(|v| v.as_i64()) {
                                        let _ = client_arc.send_command(
                                            "Page.screencastFrameAck",
                                            Some(json!({ "sessionId": sid })),
                                            evt.session_id.as_deref(),
                                        ).await;
                                    }

                                    if display.is_some() {
                                        continue;
                                    }

                                    if let Some(data) = evt.params.get("data").and_then(|v| v.as_str()) {
                                        let meta = evt.params.get("metadata");
                                        let seq = super::next_frame_seq();
                                        let msg = json!({
                                            "type": "frame",
                                            "seq": seq,
                                            "ts": super::monotonic_us(),
                                            "data": data,
                                            "pageGeneration": evt.params[crate::native::activity::FRAME_GENERATION],
                                            "metadata": {
                                                "offsetTop": meta.and_then(|m| m.get("offsetTop")).and_then(|v| v.as_f64()).unwrap_or(0.0),
                                                "pageScaleFactor": meta.and_then(|m| m.get("pageScaleFactor")).and_then(|v| v.as_f64()).unwrap_or(1.0),
                                                "deviceWidth": vw,
                                                "deviceHeight": vh,
                                                "scrollOffsetX": meta.and_then(|m| m.get("scrollOffsetX")).and_then(|v| v.as_f64()).unwrap_or(0.0),
                                                "scrollOffsetY": meta.and_then(|m| m.get("scrollOffsetY")).and_then(|v| v.as_f64()).unwrap_or(0.0),
                                                "timestamp": frame_timestamp_ms(meta),
                                            }
                                        });
                                        frame_watch.send_replace(Some(Arc::new(
                                            super::StreamFrame {
                                                seq: Some(seq),
                                                json: msg.to_string(),
                                                patch: false,
                                                base_seq: None,
                                                binary: std::sync::OnceLock::new(),
                                            },
                                        )));
                                    }
                                } else if evt.method == "Runtime.consoleAPICalled" {
                                    let level = evt.params.get("type")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("log");
                                    let raw_args = evt.params.get("args")
                                        .and_then(|v| v.as_array())
                                        .cloned()
                                        .unwrap_or_default();
                                    let text = network::format_console_args(&raw_args);
                                    if !text.is_empty() {
                                        let mut msg = json!({
                                            "type": "console",
                                            "level": level,
                                            "text": text,
                                            "timestamp": timestamp_ms(),
                                        });
                                        if !raw_args.is_empty() {
                                            msg.as_object_mut().unwrap().insert(
                                                "args".to_string(),
                                                Value::Array(raw_args),
                                            );
                                        }
                                        let _ = frame_tx.send(msg.to_string());
                                    }
                                } else if evt.method == "Runtime.exceptionThrown" {
                                    let text = evt.params.get("exceptionDetails")
                                        .and_then(|d| {
                                            d.get("exception")
                                                .and_then(|e| e.get("description").and_then(|v| v.as_str()))
                                                .or_else(|| d.get("text").and_then(|v| v.as_str()))
                                        })
                                        .unwrap_or("Unknown error");
                                    let line = evt.params.get("exceptionDetails")
                                        .and_then(|d| d.get("lineNumber").and_then(|v| v.as_i64()));
                                    let column = evt.params.get("exceptionDetails")
                                        .and_then(|d| d.get("columnNumber").and_then(|v| v.as_i64()));
                                    let msg = json!({
                                        "type": "page_error",
                                        "text": text,
                                        "line": line,
                                        "column": column,
                                        "timestamp": timestamp_ms(),
                                    });
                                    let _ = frame_tx.send(msg.to_string());
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = client_notify.notified() => {
                        let count = *client_count.lock().await;
                        // A new viewer or a writer that skipped a delta
                        // needs a whole frame. This uses the existing
                        // wakeup without rotating input coordinates.
                        published_generation = None;
                        display_tick.reset_immediately();
                        let new_session_id = cdp_session_id.read().await.clone();
                        if count == 0 {
                            if let Some(client) = &screencast {
                                let _ = client
                                    .send_command_no_params("Page.stopScreencast", session_id.as_deref())
                                    .await;
                            }
                            let mut sc = screencasting.lock().await;
                            *sc = false;
                            break;
                        }
                        let client_changed = replaced(&client, &*client_slot.read().await);
                        let session_changed = new_session_id != session_id;
                        let new_vw = *viewport_width.lock().await;
                        let new_vh = *viewport_height.lock().await;
                        let viewport_changed = new_vw != vw || new_vh != vh;
                        let display_changed = replaced(&display, &*display_slot.read().await);
                        if client_changed || session_changed || viewport_changed || display_changed {
                            if let Some(client) = &screencast {
                                let _ = client
                                    .send_command_no_params("Page.stopScreencast", session_id.as_deref())
                                    .await;
                            }
                            // The next session publishes its own status. A
                            // replaced source never reads as a stopped stream;
                            // only no source or no viewer stops it.
                            client_notify.notify_one();
                            break;
                        }
                    }
                }
            }
        } else {
            let was_screencasting = *screencasting.lock().await;
            if was_screencasting {
                if let Some(ref client) = client {
                    let session_id = cdp_session_id.read().await.clone();
                    let _ = client
                        .send_command_no_params("Page.stopScreencast", session_id.as_deref())
                        .await;
                }
                let mut sc = screencasting.lock().await;
                *sc = false;
            }
        }
    }
}

pub async fn start_screencast(
    client: &CdpClient,
    session_id: &str,
    format: &str,
    quality: i32,
    max_width: i32,
    max_height: i32,
) -> Result<(), String> {
    client
        .send_command(
            "Page.startScreencast",
            Some(json!({
                "format": format,
                "quality": quality,
                "maxWidth": max_width,
                "maxHeight": max_height,
                "everyNthFrame": 1,
            })),
            Some(session_id),
        )
        .await?;
    Ok(())
}

pub async fn stop_screencast(client: &CdpClient, session_id: &str) -> Result<(), String> {
    client
        .send_command_no_params("Page.stopScreencast", Some(session_id))
        .await?;
    Ok(())
}

pub async fn ack_screencast_frame(
    client: &CdpClient,
    session_id: &str,
    screencast_session_id: i64,
) -> Result<(), String> {
    client
        .send_command(
            "Page.screencastFrameAck",
            Some(json!({ "sessionId": screencast_session_id })),
            Some(session_id),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    async fn mock_cdp_with_seed_delay(
        main_frame_id: &str,
        seed_delay: std::time::Duration,
    ) -> (
        Arc<CdpClient>,
        mpsc::UnboundedSender<Value>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/devtools/browser/mock",
            listener.local_addr().unwrap().port()
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = methods.clone();
        let frame_id = main_frame_id.to_string();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();

            loop {
                tokio::select! {
                    message = rx.next() => {
                        let Some(Ok(Message::Text(text))) = message else {
                            break;
                        };
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap();
                        recorded.lock().await.push(method.to_string());
                        let result = if method == "Page.getFrameTree" {
                            let delay = tokio::time::sleep(seed_delay);
                            tokio::pin!(delay);
                            loop {
                                tokio::select! {
                                    _ = &mut delay => break,
                                    event = event_rx.recv() => {
                                        let Some(event) = event else {
                                            break;
                                        };
                                        tx.send(Message::Text(event.to_string())).await.unwrap();
                                    }
                                }
                            }
                            json!({ "frameTree": { "frame": { "id": frame_id } } })
                        } else {
                            json!({})
                        };
                        tx.send(Message::Text(json!({ "id": id, "result": result }).to_string()))
                            .await
                            .unwrap();
                    }
                    event = event_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        tx.send(Message::Text(event.to_string())).await.unwrap();
                    }
                }
            }
        });

        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        (client, event_tx, methods)
    }

    async fn mock_cdp(
        main_frame_id: &str,
    ) -> (
        Arc<CdpClient>,
        mpsc::UnboundedSender<Value>,
        Arc<Mutex<Vec<String>>>,
    ) {
        mock_cdp_with_seed_delay(main_frame_id, std::time::Duration::ZERO).await
    }

    async fn mock_cdp_with_first_seed_timeout(
        main_frame_id: &str,
    ) -> (
        Arc<CdpClient>,
        mpsc::UnboundedSender<Value>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/devtools/browser/retry",
            listener.local_addr().unwrap().port()
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = methods.clone();
        let frame_id = main_frame_id.to_string();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            let mut frame_tree_requests = 0;

            loop {
                tokio::select! {
                    message = rx.next() => {
                        let Some(Ok(Message::Text(text))) = message else {
                            break;
                        };
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap();
                        recorded.lock().await.push(method.to_string());
                        if method == "Page.getFrameTree" {
                            frame_tree_requests += 1;
                            if frame_tree_requests == 1 {
                                continue;
                            }
                            tx.send(Message::Text(json!({
                                "id": id,
                                "result": { "frameTree": { "frame": { "id": frame_id } } }
                            }).to_string()))
                            .await
                            .unwrap();
                        } else {
                            tx.send(Message::Text(json!({ "id": id, "result": {} }).to_string()))
                                .await
                                .unwrap();
                        }
                    }
                    event = event_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        tx.send(Message::Text(event.to_string())).await.unwrap();
                    }
                }
            }
        });

        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        (client, event_tx, methods)
    }

    async fn silent_frame_tree_cdp() -> Arc<CdpClient> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/devtools/browser/silent",
            listener.local_addr().unwrap().port()
        );
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            while let Some(Ok(message)) = rx.next().await {
                match message {
                    Message::Text(text) => {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let method = command["method"].as_str().unwrap();
                        if method != "Page.getFrameTree" {
                            let id = command["id"].as_u64().unwrap();
                            tx.send(Message::Text(json!({ "id": id, "result": {} }).to_string()))
                                .await
                                .unwrap();
                        }
                    }
                    Message::Ping(payload) => {
                        tx.send(Message::Pong(payload)).await.unwrap();
                    }
                    _ => {}
                }
            }
        });
        Arc::new(CdpClient::connect(&url).await.unwrap())
    }

    struct LoopHarness {
        events: mpsc::UnboundedSender<Value>,
        messages: broadcast::Receiver<String>,
        last_tabs: Arc<RwLock<Vec<Value>>>,
        cdp_session_id: Arc<RwLock<Option<String>>>,
        shutdown: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
        methods: Arc<Mutex<Vec<String>>>,
    }

    async fn start_loop_with_seed_delay(
        active_session: Option<&str>,
        main_frame_id: &str,
        seed_delay: std::time::Duration,
    ) -> LoopHarness {
        let (client, events, methods) = mock_cdp_with_seed_delay(main_frame_id, seed_delay).await;
        start_loop_with_client(active_session, client, events, methods).await
    }

    async fn start_loop_with_client(
        active_session: Option<&str>,
        client: Arc<CdpClient>,
        events: mpsc::UnboundedSender<Value>,
        methods: Arc<Mutex<Vec<String>>>,
    ) -> LoopHarness {
        start_loop_with_client_and_engine(active_session, client, events, methods, "chrome").await
    }

    async fn start_loop_with_client_and_engine(
        active_session: Option<&str>,
        client: Arc<CdpClient>,
        events: mpsc::UnboundedSender<Value>,
        methods: Arc<Mutex<Vec<String>>>,
        engine: &str,
    ) -> LoopHarness {
        let (frame_tx, messages) = broadcast::channel(64);
        let (frame_watch, _) = watch::channel(None);
        let client_slot = Arc::new(RwLock::new(Some(client)));
        let client_notify = Arc::new(tokio::sync::Notify::new());
        let client_count = Arc::new(Mutex::new(1));
        let cdp_session_id = Arc::new(RwLock::new(active_session.map(String::from)));
        let last_tabs = Arc::new(RwLock::new(vec![
            json!({ "tabId": "t1", "url": "https://active.test/", "active": true }),
            json!({ "tabId": "t2", "url": "https://background.test/", "active": false }),
        ]));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(cdp_event_loop(
            frame_tx,
            frame_watch,
            Arc::new(super::super::ScreencastConfig::default()),
            client_slot,
            Arc::new(RwLock::new(None)),
            Arc::new(super::super::presentation::Presentation::new()),
            watch::channel(None).1,
            Arc::new(super::super::StreamMedia::new(Default::default())),
            client_notify.clone(),
            Arc::new(Mutex::new(false)),
            client_count,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            cdp_session_id.clone(),
            Arc::new(Mutex::new(1280)),
            Arc::new(Mutex::new(720)),
            last_tabs.clone(),
            Arc::new(RwLock::new(engine.to_string())),
            Arc::new(Mutex::new(false)),
            shutdown_rx,
        ));
        client_notify.notify_one();

        LoopHarness {
            events,
            messages,
            last_tabs,
            cdp_session_id,
            shutdown,
            task,
            methods,
        }
    }

    async fn start_loop(active_session: Option<&str>, main_frame_id: &str) -> LoopHarness {
        start_loop_with_seed_delay(active_session, main_frame_id, std::time::Duration::ZERO).await
    }

    async fn next_message_of_type(messages: &mut broadcast::Receiver<String>, kind: &str) -> Value {
        loop {
            let text = tokio::time::timeout(std::time::Duration::from_secs(5), messages.recv())
                .await
                .expect("timed out waiting for stream message")
                .expect("stream channel closed");
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["type"] == kind {
                return message;
            }
        }
    }

    async fn expect_no_url(messages: &mut broadcast::Receiver<String>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let result = tokio::time::timeout(remaining, messages.recv()).await;
            let Ok(Ok(text)) = result else {
                return;
            };
            let message: Value = serde_json::from_str(&text).unwrap();
            assert_ne!(message["type"], "url", "unexpected URL message: {message}");
        }
    }

    async fn stop_loop(harness: LoopHarness) {
        let _ = harness.shutdown.send(true);
        harness.task.await.unwrap();
    }

    async fn wait_for_method(methods: &Mutex<Vec<String>>, expected: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if methods.lock().await.iter().any(|method| method == expected) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for CDP method");
    }

    /// A presentation change (a viewer's new layout, or its applied surface)
    /// wakes the capture loop at once instead of waiting for the next tick of
    /// the current interval, so the first frame of a new geometry is captured
    /// as soon as the layout is ready.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_presentation_change_wakes_capture_before_the_next_tick() {
        use crate::native::display::DisplayClient;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (display, _control, frames) = DisplayClient::test_channel();
        // The event sender is the mock browser's connection: dropping it
        // closes CDP and ends the loop's attachment before any capture.
        let (client, _events, methods) = mock_cdp("F-MAIN").await;
        let (frame_tx, _messages) = broadcast::channel(64);
        let (frame_watch, _) = watch::channel(None);
        let client_notify = Arc::new(tokio::sync::Notify::new());
        let presentation = Arc::new(super::super::presentation::Presentation::new());
        let connection = uuid::Uuid::new_v4();
        let config = super::super::presentation::PresentationConfig {
            viewer: uuid::Uuid::new_v4(),
            width: 800,
            height: 600,
        };
        presentation.configure(connection, config);
        let (shutdown, shutdown_rx) = watch::channel(false);
        // A closed custody channel ends a display attachment at once, so the
        // test holds its sender like the daemon's browser control does.
        let (_custody, custody) = watch::channel(None);
        let task = tokio::spawn(cdp_event_loop(
            frame_tx,
            frame_watch,
            Arc::new(super::super::ScreencastConfig::default()),
            Arc::new(RwLock::new(Some(client))),
            Arc::new(RwLock::new(Some(display))),
            presentation.clone(),
            custody,
            Arc::new(super::super::StreamMedia::new(Default::default())),
            client_notify.clone(),
            Arc::new(Mutex::new(false)),
            Arc::new(Mutex::new(1)),
            Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            Arc::new(RwLock::new(Some("S-ACTIVE".to_string()))),
            Arc::new(Mutex::new(1280)),
            Arc::new(Mutex::new(720)),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new("chrome".to_string())),
            Arc::new(Mutex::new(false)),
            shutdown_rx,
        ));
        client_notify.notify_one();
        let mut frames = BufReader::new(frames);
        let mut line = String::new();
        // The first tick captures at once; answer it unchanged after a short
        // hold so the next natural tick is still most of a period away.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            frames.read_line(&mut line),
        )
        .await
        .expect("a first capture request")
        .unwrap();
        let first: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(first["op"], "capture");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let reply = json!({"id": first["id"], "success": true, "data": {"changed": false}})
            .to_string()
            + "\n";
        frames.get_mut().write_all(reply.as_bytes()).await.unwrap();
        // Same viewer, same pacing tier, new requested size: only the wake
        // can explain a capture request arriving before the interval elapses.
        let triggered = std::time::Instant::now();
        presentation.configure(
            connection,
            super::super::presentation::PresentationConfig {
                width: 640,
                ..config
            },
        );
        line.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            frames.read_line(&mut line),
        )
        .await
        .expect("a second capture request")
        .unwrap();
        let waited = triggered.elapsed();
        let period = super::super::presentation::FramePacing::PRESENTED.period();
        assert!(
            waited < period / 2,
            "capture waited {waited:?} after the presentation changed (period {period:?})"
        );
        let second: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(second["op"], "capture");
        let reply = json!({"id": second["id"], "success": true, "data": {"changed": false}})
            .to_string()
            + "\n";
        frames.get_mut().write_all(reply.as_bytes()).await.unwrap();
        let _ = shutdown.send(true);
        task.await.unwrap();
        drop(methods);
    }

    /// A picker that opens or ends, and a download that settles, ring the
    /// files doorbell once each; download progress and unrelated pages do not.
    #[tokio::test]
    async fn test_file_pickers_and_settled_downloads_ring_the_files_doorbell() {
        let (client, events, methods) = mock_cdp("F-MAIN").await;
        client.files.begin("aabbccdd-1111-4222-8333-123456789abc");
        client.files.intercepted("S-ACTIVE");
        let mut harness =
            start_loop_with_client(Some("S-ACTIVE"), client.clone(), events, methods).await;
        let send = |event: Value| harness.events.send(event).unwrap();
        // A picker in a page without interception is not a destination.
        send(
            json!({"method":"Page.fileChooserOpened","sessionId":"S-OTHER",
            "params":{"frameId":"F-MAIN","mode":"selectSingle","backendNodeId":7}}),
        );
        send(
            json!({"method":"Page.fileChooserOpened","sessionId":"S-ACTIVE",
            "params":{"frameId":"F-MAIN","mode":"selectSingle","backendNodeId":7}}),
        );
        let opened = next_message_of_type(&mut harness.messages, "files").await;
        assert!(opened["ts"].as_u64().is_some_and(|ts| ts > 0), "{opened}");
        assert_eq!(
            opened.as_object().unwrap().len(),
            2,
            "names nothing: {opened}"
        );
        // Navigating the picker's document ends it.
        send(
            json!({"method":"Page.frameNavigated","sessionId":"S-ACTIVE",
            "params":{"frame":{"id":"F-MAIN","url":"https://next.test/"}}}),
        );
        let ended = next_message_of_type(&mut harness.messages, "files").await;
        assert!(ended["ts"].as_u64() >= opened["ts"].as_u64());
        let guid = uuid::Uuid::new_v4().to_string();
        send(json!({"method":"Browser.downloadWillBegin",
            "params":{"guid":guid,"frameId":"F-MAIN","suggestedFilename":"a.txt","url":"https://next.test/a.txt"}}));
        send(json!({"method":"Browser.downloadProgress",
            "params":{"guid":guid,"state":"inProgress","receivedBytes":1,"totalBytes":2}}));
        send(json!({"method":"Browser.downloadProgress",
            "params":{"guid":guid,"state":"completed","receivedBytes":2,"totalBytes":2}}));
        next_message_of_type(&mut harness.messages, "files").await;
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                let text = harness.messages.recv().await.unwrap();
                let message: Value = serde_json::from_str(&text).unwrap();
                if message["type"] == "files" {
                    return message;
                }
            }
        })
        .await;
        assert!(quiet.is_err(), "one doorbell per change: {quiet:?}");
        stop_loop(harness).await;
    }

    /// Every window frame carries the media clock at its capture request, the
    /// window rectangle within it, and, while a lease has applied input, the
    /// last input sequence the display acknowledged before that request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_window_frames_carry_ts_visible_and_applied_input() {
        use crate::native::display::DisplayClient;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (display, _control, frames) = DisplayClient::test_channel();
        let (frame_tx, _messages) = broadcast::channel(64);
        let (frame_watch, mut published) = watch::channel(None);
        let client_notify = Arc::new(tokio::sync::Notify::new());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (_custody, custody) = watch::channel(None);
        let applied = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let task = tokio::spawn(cdp_event_loop(
            frame_tx,
            frame_watch,
            Arc::new(super::super::ScreencastConfig::default()),
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(Some(display))),
            Arc::new(super::super::presentation::Presentation::new()),
            custody,
            Arc::new(super::super::StreamMedia::new(applied.clone())),
            client_notify.clone(),
            Arc::new(Mutex::new(false)),
            Arc::new(Mutex::new(1)),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::new(RwLock::new(None)),
            Arc::new(Mutex::new(1280)),
            Arc::new(Mutex::new(720)),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new("chrome".to_string())),
            Arc::new(Mutex::new(false)),
            shutdown_rx,
        ));
        client_notify.notify_one();
        let mut frames = BufReader::new(frames);
        let mut answer = |visible: Option<Value>| {
            let mut frame = json!({"changed":true,"width":2560,"height":1440,"encoding":"jpeg",
                "data":"AA==","cursorIncluded":true,"quality":85});
            if let Some(visible) = visible {
                frame["visible"] = visible;
            }
            frame
        };
        let mut next_frame = async |data: Value| {
            let mut line = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                frames.read_line(&mut line),
            )
            .await
            .expect("a capture request")
            .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let reply =
                json!({"id": request["id"], "success": true, "data": data}).to_string() + "\n";
            frames.get_mut().write_all(reply.as_bytes()).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), published.changed())
                .await
                .unwrap()
                .unwrap();
            let frame = published.borrow_and_update().clone().unwrap();
            serde_json::from_str::<Value>(&frame.json).unwrap()
        };
        let before = super::super::monotonic_us();
        let first = next_frame(answer(None)).await;
        assert!((before..=super::super::monotonic_us()).contains(&first["ts"].as_u64().unwrap()));
        assert_eq!(
            first["visible"],
            json!({"x":0,"y":0,"width":2560,"height":1440})
        );
        assert!(first.get("inputSeq").is_none(), "{first}");
        applied.store(7, std::sync::atomic::Ordering::Release);
        let window = json!({"x":0,"y":0,"width":1418,"height":1888});
        let second = next_frame(answer(Some(window.clone()))).await;
        assert_eq!(second["inputSeq"], 7);
        assert_eq!(second["visible"], window);
        assert!(second["ts"].as_u64() > first["ts"].as_u64());
        let _ = shutdown.send(true);
        task.await.unwrap();
    }

    /// Reading the float as an integer stamps every frame 0, so no client can
    /// measure frame age.
    #[test]
    fn test_frame_timestamp_converts_cdp_seconds_to_millis() {
        let meta = json!({ "timestamp": 1785038682.238_f64 });
        assert_eq!(frame_timestamp_ms(Some(&meta)), 1785038682238);
    }

    #[test]
    fn test_frame_timestamp_absent_or_zero_stays_zero() {
        assert_eq!(frame_timestamp_ms(None), 0);
        assert_eq!(frame_timestamp_ms(Some(&json!({}))), 0);
        assert_eq!(frame_timestamp_ms(Some(&json!({ "timestamp": 0 }))), 0);
        assert_eq!(frame_timestamp_ms(Some(&json!({ "timestamp": "nope" }))), 0);
    }

    #[tokio::test]
    async fn test_same_document_navigation_tracks_active_main_frame_and_ignores_child() {
        let mut harness = start_loop(Some("S-ACTIVE"), "F-MAIN").await;
        next_message_of_type(&mut harness.messages, "status").await;

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frameId": "F-MAIN",
                    "url": "https://active.test/spa",
                    "navigationType": "historyApi"
                }
            }))
            .unwrap();
        let history = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(history["url"], "https://active.test/spa");

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frameId": "F-MAIN",
                    "url": "https://active.test/spa#section",
                    "navigationType": "fragment"
                }
            }))
            .unwrap();
        let fragment = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(fragment["url"], "https://active.test/spa#section");

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frameId": "F-CHILD",
                    "url": "https://active.test/child",
                    "navigationType": "historyApi"
                }
            }))
            .unwrap();
        expect_no_url(&mut harness.messages).await;

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "sessionId": "S-BACKGROUND",
                "params": {
                    "frameId": "F-BACKGROUND",
                    "url": "https://background.test/spa",
                    "navigationType": "historyApi"
                }
            }))
            .unwrap();
        expect_no_url(&mut harness.messages).await;
        assert_eq!(
            harness.last_tabs.read().await[0]["url"],
            "https://active.test/spa#section"
        );
        assert!(harness
            .methods
            .lock()
            .await
            .iter()
            .any(|method| method == "Page.getFrameTree"));

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_lightpanda_background_full_navigation_does_not_replace_active_url() {
        let (client, events, methods) = mock_cdp("F-MAIN").await;
        let mut harness = start_loop_with_client_and_engine(
            Some("S-ACTIVE"),
            client,
            events,
            methods,
            "lightpanda",
        )
        .await;
        next_message_of_type(&mut harness.messages, "status").await;

        harness
            .events
            .send(json!({
                "method": "Page.frameNavigated",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frame": {
                        "id": "F-MAIN",
                        "url": "https://active.test/full"
                    }
                }
            }))
            .unwrap();
        let active = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(active["url"], "https://active.test/full");

        harness
            .events
            .send(json!({
                "method": "Page.frameNavigated",
                "sessionId": "S-BACKGROUND",
                "params": {
                    "frame": {
                        "id": "F-BACKGROUND",
                        "url": "https://background.test/changed"
                    }
                }
            }))
            .unwrap();
        expect_no_url(&mut harness.messages).await;
        assert_eq!(
            harness.last_tabs.read().await[0]["url"],
            "https://active.test/full"
        );

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_old_session_event_cannot_replace_new_active_tab_url() {
        let mut harness = start_loop(Some("S-OLD"), "F-OLD").await;
        next_message_of_type(&mut harness.messages, "status").await;

        {
            let mut session = harness.cdp_session_id.write().await;
            let mut tabs = harness.last_tabs.write().await;
            *session = Some("S-NEW".to_string());
            *tabs = vec![
                json!({ "tabId": "t1", "url": "https://old.test/", "active": false }),
                json!({ "tabId": "t2", "url": "https://new.test/", "active": true }),
            ];
        }

        harness
            .events
            .send(json!({
                "method": "Page.frameNavigated",
                "sessionId": "S-OLD",
                "params": {
                    "frame": {
                        "id": "F-OLD",
                        "url": "https://old.test/late"
                    }
                }
            }))
            .unwrap();
        expect_no_url(&mut harness.messages).await;
        assert_eq!(
            harness.last_tabs.read().await[1]["url"],
            "https://new.test/"
        );

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_same_document_navigation_retries_after_seed_timeout() {
        let (client, events, methods) = mock_cdp_with_first_seed_timeout("F-MAIN").await;
        let mut harness = start_loop_with_client(Some("S-ACTIVE"), client, events, methods).await;
        next_message_of_type(&mut harness.messages, "status").await;
        wait_for_method(&harness.methods, "Page.getFrameTree").await;

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frameId": "F-MAIN",
                    "url": "https://active.test/recovered",
                    "navigationType": "historyApi"
                }
            }))
            .unwrap();

        let message = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(message["url"], "https://active.test/recovered");
        assert_eq!(
            harness
                .methods
                .lock()
                .await
                .iter()
                .filter(|method| method.as_str() == "Page.getFrameTree")
                .count(),
            2
        );

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_same_document_navigation_waits_for_main_frame_seed() {
        let mut harness = start_loop_with_seed_delay(
            Some("S-ACTIVE"),
            "F-MAIN",
            std::time::Duration::from_millis(100),
        )
        .await;
        next_message_of_type(&mut harness.messages, "status").await;
        wait_for_method(&harness.methods, "Page.getFrameTree").await;

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frameId": "F-MAIN",
                    "url": "https://active.test/immediate",
                    "navigationType": "historyApi"
                }
            }))
            .unwrap();
        let message = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(message["url"], "https://active.test/immediate");

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_same_document_seed_buffer_discards_oldest_event_at_capacity() {
        let mut harness = start_loop_with_seed_delay(
            Some("S-ACTIVE"),
            "F-MAIN",
            std::time::Duration::from_millis(100),
        )
        .await;
        next_message_of_type(&mut harness.messages, "status").await;
        wait_for_method(&harness.methods, "Page.getFrameTree").await;

        for index in 0..65 {
            harness
                .events
                .send(json!({
                    "method": "Page.navigatedWithinDocument",
                    "sessionId": "S-ACTIVE",
                    "params": {
                        "frameId": "F-MAIN",
                        "url": format!("https://active.test/{index}"),
                        "navigationType": "historyApi"
                    }
                }))
                .unwrap();
        }

        let first = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(first["url"], "https://active.test/1");

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_background_full_navigation_does_not_replace_active_url() {
        let mut harness = start_loop(Some("S-ACTIVE"), "F-MAIN").await;
        next_message_of_type(&mut harness.messages, "status").await;

        harness
            .events
            .send(json!({
                "method": "Page.frameNavigated",
                "sessionId": "S-ACTIVE",
                "params": {
                    "frame": {
                        "id": "F-MAIN",
                        "url": "https://active.test/full"
                    }
                }
            }))
            .unwrap();
        let active = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(active["url"], "https://active.test/full");

        harness
            .events
            .send(json!({
                "method": "Page.frameNavigated",
                "sessionId": "S-BACKGROUND",
                "params": {
                    "frame": {
                        "id": "F-BACKGROUND",
                        "url": "https://background.test/changed"
                    }
                }
            }))
            .unwrap();
        expect_no_url(&mut harness.messages).await;
        assert_eq!(
            harness.last_tabs.read().await[0]["url"],
            "https://active.test/full"
        );

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_direct_page_navigation_matches_empty_active_session() {
        let mut harness = start_loop(Some(""), "F-DIRECT").await;
        next_message_of_type(&mut harness.messages, "status").await;

        harness
            .events
            .send(json!({
                "method": "Page.navigatedWithinDocument",
                "params": {
                    "frameId": "F-DIRECT",
                    "url": "https://provider.test/spa",
                    "navigationType": "historyApi"
                }
            }))
            .unwrap();
        let message = next_message_of_type(&mut harness.messages, "url").await;
        assert_eq!(message["url"], "https://provider.test/spa");

        stop_loop(harness).await;
    }

    #[tokio::test]
    async fn test_unanswered_frame_tree_seed_does_not_block_shutdown() {
        let client = silent_frame_tree_cdp().await;
        let (frame_tx, mut messages) = broadcast::channel(64);
        let (frame_watch, _) = watch::channel(None);
        let client_notify = Arc::new(tokio::sync::Notify::new());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(cdp_event_loop(
            frame_tx,
            frame_watch,
            Arc::new(super::super::ScreencastConfig::default()),
            Arc::new(RwLock::new(Some(client.clone()))),
            Arc::new(RwLock::new(None)),
            Arc::new(super::super::presentation::Presentation::new()),
            watch::channel(None).1,
            Arc::new(super::super::StreamMedia::new(Default::default())),
            client_notify.clone(),
            Arc::new(Mutex::new(false)),
            Arc::new(Mutex::new(1)),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::new(RwLock::new(Some("S-ACTIVE".to_string()))),
            Arc::new(Mutex::new(1280)),
            Arc::new(Mutex::new(720)),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new("chrome".to_string())),
            Arc::new(Mutex::new(false)),
            shutdown_rx,
        ));
        client_notify.notify_one();

        next_message_of_type(&mut messages, "status").await;
        let started = tokio::time::Instant::now();
        let _ = shutdown.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("frame tree seed should not hold shutdown for the CDP timeout")
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "shutdown waited for the default CDP command timeout"
        );
        assert_eq!(client.pending_len().await, 0);
    }
}
