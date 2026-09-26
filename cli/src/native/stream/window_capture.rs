//! The owned window's frames, captured when its display changes.
//!
//! With a helper that waits for damage (`captureWait`), a capture is asked
//! for once the pacing allows and answers as soon as the browser paints, so
//! a frame leaves ≈0.2 ms after its damage instead of on the next tick; the
//! pacing only caps the rate. An older helper is polled at the pacing rate.
//! Every answer may also carry the displayed cursor's identity
//! (`cursorIdentity`), which viewers that draw the pointer receive as a
//! `cursor` message (`cursor_identity`). A capture in flight is never
//! cancelled: that would end the helper link. Stopping takes effect when it
//! answers.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{broadcast, watch, Mutex, Notify};

use super::cursor_identity::CursorIdentities;
use super::presentation::Presentation;
use super::{StreamFrame, StreamMedia};
use crate::native::browser_control::custody_active;
use crate::native::display::{CaptureRequest, DisplayClient, Rect, Surface};

/// How long one capture waits in the helper for damage, a cursor identity
/// or a finished layout before it answers unchanged.
const CAPTURE_WAIT_MS: u32 = 100;
/// The least time between two captures when the first found nothing, so a
/// helper that answers unchanged early never spins the loop.
const IDLE_SPACING: Duration = Duration::from_millis(4);

/// What the capture loop shares with the stream.
#[derive(Clone)]
pub(super) struct Sinks {
    pub frame_tx: broadcast::Sender<String>,
    pub frame_watch: watch::Sender<Option<Arc<StreamFrame>>>,
    pub presentation: Arc<Presentation>,
    pub custody: watch::Receiver<Option<Instant>>,
    pub media: Arc<StreamMedia>,
    pub client_count: Arc<Mutex<usize>>,
    pub patch_clients: Arc<AtomicUsize>,
    pub cursors: CursorIdentities,
}

/// A viewer needs a whole frame: a new viewer, or a writer that skipped a
/// delta.
#[derive(Default)]
struct Refresh {
    wanted: AtomicBool,
    notify: Notify,
}

/// The running capture loop of one display.
pub(super) struct WindowCapture {
    display: Arc<DisplayClient>,
    refresh: Arc<Refresh>,
    stop: watch::Sender<bool>,
}

impl WindowCapture {
    pub(super) fn start(display: Arc<DisplayClient>, sinks: Sinks) -> Self {
        let refresh = Arc::new(Refresh::default());
        let (stop, stopped) = watch::channel(false);
        tokio::spawn(run(display.clone(), sinks, refresh.clone(), stopped));
        Self {
            display,
            refresh,
            stop,
        }
    }

    pub(super) fn captures(&self, display: &Arc<DisplayClient>) -> bool {
        Arc::ptr_eq(&self.display, display)
    }

    /// The next frame is whole and goes out without waiting for the pacing.
    pub(super) fn refresh(&self) {
        self.refresh.wanted.store(true, Ordering::Release);
        self.refresh.notify.notify_one();
    }
}

impl Drop for WindowCapture {
    /// Ends the loop once its capture in flight answers; nothing it
    /// captured after this is published.
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// The frame the viewers hold, which the next patch amends.
#[derive(Default)]
struct Published {
    generation: Option<String>,
    size: (u32, u32),
    patches: bool,
    seq: Option<u64>,
}

impl Published {
    /// Whether viewers hold a whole frame of this surface to amend.
    fn holds(&self, surface: &Surface) -> bool {
        self.generation.as_deref() == Some(surface.generation.as_str())
            && self.size == (surface.width, surface.height)
    }
}

async fn run(
    display: Arc<DisplayClient>,
    sinks: Sinks,
    refresh: Arc<Refresh>,
    mut stop: watch::Receiver<bool>,
) {
    let waits = display.has("captureWait");
    let identity = display.has("cursorIdentity");
    let mut presentation = sinks.presentation.subscribe();
    let mut custody = sinks.custody.clone();
    let mut published = Published::default();
    let mut next = tokio::time::Instant::now();
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { return; }
            }
            _ = tokio::time::sleep_until(next) => {}
            _ = refresh.notify.notified() => {}
            // A layout was applied or the person's custody changed: the
            // next frame shows it at once.
            changed = presentation.changed() => {
                if changed.is_err() { return; }
            }
            changed = custody.changed() => {
                if changed.is_err() { return; }
            }
        }
        if *stop.borrow() {
            return;
        }
        let controlled = custody_active(*custody.borrow_and_update());
        let pacing = sinks.presentation.capture_pacing(controlled);
        let viewers = *sinks.client_count.lock().await;
        // Patches amend a whole frame every viewer holds; one viewer that
        // does not composite them makes the next frame whole for everyone.
        let patches = sinks.patch_clients.load(Ordering::Acquire) == viewers;
        if refresh.wanted.swap(false, Ordering::AcqRel) || !patches && published.patches {
            published.generation = None;
        }
        let request = CaptureRequest {
            // Only a viewer that does not draw the pointer itself needs it in
            // frames, and never while a person controls (their own cursor is
            // it).
            cursor: sinks.media.composites_cursor(controlled),
            budget_bytes: pacing.budget_bytes,
            force: !published.holds(&display.surface()),
            patches,
            wait_ms: if waits { CAPTURE_WAIT_MS } else { 0 },
            cursor_identity: identity,
        };
        let requested = super::monotonic_us();
        let started = tokio::time::Instant::now();
        let captured = display.capture(request).await;
        if *stop.borrow() {
            return;
        }
        match captured {
            Ok(captured) => {
                let answered = super::monotonic_us();
                let waited = captured
                    .frame
                    .as_ref()
                    .map_or(0, |(capture, _)| capture.wait_us());
                // The frame was fetched when its damage ended the wait.
                let ts = requested + waited;
                if let Some(cursor) = captured.cursor {
                    let at = if captured.frame.is_some() {
                        ts
                    } else {
                        answered
                    };
                    sinks.cursors.observe(cursor, at);
                }
                next = match captured.frame {
                    Some((capture, surface)) => {
                        publish(&sinks, &mut published, capture, surface, ts);
                        started + Duration::from_micros(waited) + pacing.period()
                    }
                    None if waits => started + IDLE_SPACING,
                    None => started + pacing.period(),
                };
            }
            Err(error) if error.is_transient() => {
                // A frame taken across a layout was dropped: the helper's
                // next patch would amend a frame no viewer holds.
                published.generation = None;
                next = started + pacing.period();
            }
            Err(_) => {
                sinks.frame_watch.send_replace(None);
                let _ = sinks
                    .frame_tx
                    .send(json!({"type":"error", "code":"display_unavailable"}).to_string());
                return;
            }
        }
    }
}

fn publish(
    sinks: &Sinks,
    published: &mut Published,
    capture: crate::native::display::Capture,
    mut surface: Surface,
    ts: u64,
) {
    let seq = super::next_frame_seq();
    let patch = capture.data.is_none();
    surface.cursor_included = capture.cursor_included;
    let visible = capture.visible.unwrap_or(Rect {
        x: 0,
        y: 0,
        width: surface.width,
        height: surface.height,
    });
    let mut message = json!({
        "type": "frame", "seq": seq, "encoding": capture.encoding,
        "surface": surface, "ts": ts, "visible": visible,
    });
    if let Some(input_seq) = sinks.media.applied_input_at(ts) {
        message["inputSeq"] = json!(input_seq);
    }
    if let Some(data) = capture.data {
        message["data"] = json!(data);
    } else {
        message["patches"] = json!(capture.patches);
        message["baseSeq"] = json!(published.seq);
    }
    let base_seq = if patch { published.seq } else { None };
    published.size = (surface.width, surface.height);
    published.generation = Some(surface.generation);
    published.patches = patch;
    published.seq = Some(seq);
    sinks.frame_watch.send_replace(Some(Arc::new(StreamFrame {
        seq: Some(seq),
        json: message.to_string(),
        patch,
        base_seq,
        binary: std::sync::OnceLock::new(),
    })));
}

// The display helper is a private X11 process: Linux only.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    struct Harness {
        display: Arc<DisplayClient>,
        frames: BufReader<UnixStream>,
        messages: broadcast::Receiver<String>,
        published: watch::Receiver<Option<Arc<StreamFrame>>>,
        applied: Arc<super::super::AppliedInput>,
        media: Arc<StreamMedia>,
        sinks: Sinks,
        _control: UnixStream,
        _custody: watch::Sender<Option<Instant>>,
    }

    /// One viewer that composites neither patches nor the pointer, and no
    /// presenter: passive pacing (15 fps, a 66 ms period).
    fn harness(features: &[&str]) -> Harness {
        let (display, control, frames) = DisplayClient::test_channel();
        display.advertise(features);
        let (frame_tx, messages) = broadcast::channel(64);
        let (frame_watch, published) = watch::channel(None);
        let (custody_sender, custody) = watch::channel(None);
        let applied = Arc::new(super::super::AppliedInput::default());
        let media = Arc::new(StreamMedia::new(applied.clone()));
        media.viewer_joined(false, false);
        let sinks = Sinks {
            cursors: CursorIdentities::new(
                frame_tx.clone(),
                media.clone(),
                Arc::new(tokio::sync::RwLock::new(None)),
                Arc::new(tokio::sync::RwLock::new(None)),
            ),
            frame_tx,
            frame_watch,
            presentation: Arc::new(Presentation::new()),
            custody,
            media: media.clone(),
            client_count: Arc::new(Mutex::new(1)),
            patch_clients: Arc::new(AtomicUsize::new(0)),
        };
        Harness {
            display,
            frames: BufReader::new(frames),
            messages,
            published,
            applied,
            media,
            sinks,
            _control: control,
            _custody: custody_sender,
        }
    }

    impl Harness {
        async fn request(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(2), self.frames.read_line(&mut line))
                .await
                .expect("a capture request")
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "capture");
            request
        }

        async fn answer(&mut self, request: &Value, data: Value) {
            let reply = json!({"id": request["id"], "success": true, "data": data}).to_string();
            self.frames
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn frame(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(2), self.published.changed())
                .await
                .expect("a published frame")
                .unwrap();
            let frame = self.published.borrow_and_update().clone().unwrap();
            serde_json::from_str(&frame.json).unwrap()
        }

        async fn message(&mut self, kind: &str) -> Value {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let message: Value =
                        serde_json::from_str(&self.messages.recv().await.unwrap()).unwrap();
                    if message["type"] == kind {
                        return message;
                    }
                }
            })
            .await
            .expect("a message")
        }
    }

    fn whole(wait_us: u64) -> Value {
        json!({"changed":true,"width":2560,"height":1440,"encoding":"jpeg","data":"AA==",
            "cursorIncluded":true,"quality":85,"timings":{"waitUs":wait_us}})
    }

    /// With a helper that waits for damage, an unchanged answer is followed
    /// by the next capture at once instead of a tick later, and a frame is
    /// stamped when its damage ended the wait: `inputSeq` is the input
    /// acknowledged before that moment, even when acknowledged during the
    /// wait, and not an input acknowledged after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_capture_is_asked_again_at_once_and_stamped_when_its_damage_arrived() {
        let mut h = harness(&["captureWait", "cursorIdentity"]);
        let origin = super::super::monotonic_us();
        let _capture = WindowCapture::start(h.display.clone(), h.sinks.clone());
        let first = h.request().await;
        assert_eq!(first["waitMs"], CAPTURE_WAIT_MS);
        assert_eq!(first["cursorIdentity"], true);
        assert_eq!(first["force"], true);
        h.answer(&first, json!({"changed": false})).await;
        let answered = std::time::Instant::now();
        let second = h.request().await;
        let gap = answered.elapsed();
        let period = super::super::presentation::FramePacing::PASSIVE.period();
        assert!(
            gap < period / 4,
            "an unchanged waited capture was asked again after {gap:?} (period {period:?})"
        );
        // Input acknowledged while the helper waited, before the damage.
        h.applied.record(3);
        let waited = super::super::monotonic_us() - origin + 1_000;
        h.answer(&second, whole(waited)).await;
        let frame = h.frame().await;
        assert!(frame["ts"].as_u64().unwrap() >= origin + waited, "{frame}");
        assert_eq!(frame["inputSeq"], 3, "{frame}");
        // Acknowledged after the next capture began: not in its pixels.
        let third = h.request().await;
        h.applied.record(4);
        h.answer(&third, whole(0)).await;
        let frame = h.frame().await;
        assert_eq!(frame["inputSeq"], 3, "{frame}");
    }

    /// An older helper, which neither waits nor reports identities, is asked
    /// for neither and polled at the pacing rate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_helper_is_polled_at_the_pacing_rate_without_new_fields() {
        let mut h = harness(&[]);
        let _capture = WindowCapture::start(h.display.clone(), h.sinks.clone());
        let first = h.request().await;
        assert!(first.get("waitMs").is_none(), "{first}");
        assert!(first.get("cursorIdentity").is_none(), "{first}");
        h.answer(&first, json!({"changed": false})).await;
        let answered = std::time::Instant::now();
        let second = h.request().await;
        let period = super::super::presentation::FramePacing::PASSIVE.period();
        assert!(
            answered.elapsed() >= period / 2,
            "polled after {:?}",
            answered.elapsed()
        );
        h.answer(&second, json!({"changed": false})).await;
    }

    /// The displayed cursor's identity reaches every viewer once per change
    /// as a `cursor` message on the media clock, and a later viewer is given
    /// the newest one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cursor_identities_reach_viewers_and_are_kept_for_later_ones() {
        let mut h = harness(&["captureWait", "cursorIdentity"]);
        let before = super::super::monotonic_us();
        let _capture = WindowCapture::start(h.display.clone(), h.sinks.clone());
        let first = h.request().await;
        h.answer(
            &first,
            json!({"changed": false, "cursor": {"serial": 5, "css": "text"}}),
        )
        .await;
        let cursor = h.message("cursor").await;
        assert_eq!(cursor["serial"], 5);
        assert_eq!(cursor["css"], "text");
        assert!(cursor["ts"].as_u64().unwrap() >= before);
        assert_eq!(
            serde_json::from_str::<Value>(&h.media.cursor().unwrap()).unwrap(),
            cursor
        );
        let image = json!({"hash":"0123456789abcdef","width":32,"height":32,"hotX":1,"hotY":2,"scale":2,"png":"iVBORw0KGgo="});
        let second = h.request().await;
        let mut data = whole(0);
        data["cursor"] = json!({"serial": 6, "css": null, "image": image});
        h.answer(&second, data).await;
        let frame = h.frame().await;
        let cursor = h.message("cursor").await;
        assert_eq!(cursor["css"], Value::Null);
        assert_eq!(cursor["image"], image);
        assert_eq!(cursor["ts"], frame["ts"], "the frame that carried it");
    }

    /// A frame dropped as taken across a layout means the helper's next
    /// patch would amend a frame no viewer holds: the next frame is whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_frame_makes_the_next_one_whole() {
        let mut h = harness(&["captureWait"]);
        h.sinks.patch_clients.store(1, Ordering::Release);
        let _capture = WindowCapture::start(h.display.clone(), h.sinks.clone());
        let first = h.request().await;
        assert_eq!(first["force"], true);
        h.answer(&first, whole(0)).await;
        h.frame().await;
        let second = h.request().await;
        assert_eq!(second["force"], false);
        assert_eq!(second["patches"], true);
        let stale = json!({"id": second["id"], "success": false,
            "error": {"code":"display_frame_stale","message":"stale","operationPerformed":false}});
        h.frames
            .get_mut()
            .write_all(format!("{stale}\n").as_bytes())
            .await
            .unwrap();
        let third = h.request().await;
        assert_eq!(third["force"], true, "{third}");
        h.answer(&third, whole(0)).await;
    }

    /// Stopping never cancels the capture in flight, which would end the
    /// helper link: the loop ends once it answers and publishes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_waits_for_the_capture_in_flight_and_publishes_nothing_after() {
        let mut h = harness(&["captureWait"]);
        let capture = WindowCapture::start(h.display.clone(), h.sinks.clone());
        let first = h.request().await;
        drop(capture);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(h.display.available(), "the helper link is intact");
        h.answer(&first, whole(0)).await;
        let mut line = String::new();
        let next =
            tokio::time::timeout(Duration::from_millis(200), h.frames.read_line(&mut line)).await;
        assert!(next.is_err(), "no capture after stopping: {line}");
        assert!(
            h.published.borrow().is_none(),
            "nothing published after stopping"
        );
        assert!(h.display.available());
    }

    /// A new viewer needs a whole frame without waiting for the pacing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refresh_asks_for_a_whole_frame_at_once() {
        let mut h = harness(&[]);
        h.sinks.patch_clients.store(1, Ordering::Release);
        let capture = WindowCapture::start(h.display.clone(), h.sinks.clone());
        let first = h.request().await;
        h.answer(&first, whole(0)).await;
        h.frame().await;
        let asked = std::time::Instant::now();
        capture.refresh();
        let second = h.request().await;
        assert!(asked.elapsed() < super::super::presentation::FramePacing::PASSIVE.period() / 2);
        assert_eq!(second["force"], true);
        h.answer(&second, json!({"changed": false})).await;
    }
}
