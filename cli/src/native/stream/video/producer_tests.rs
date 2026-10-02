//! The producer against a display helper the test plays (the picture
//! channel's fake), with the real encoder, and libaom's decoder to see what
//! a viewer would paint.

use super::super::testing::{Change, FakeScreen, BLUE, GREEN, GREY, RED};
use super::*;
use crate::native::video::convert::to_rgb;
use crate::native::video::{Chroma, Decoded, Decoder};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};

struct Rig {
    producer: Arc<Producer>,
    screen: FakeScreen,
    display: Arc<DisplayClient>,
    /// The helper's side of the control socket: layouts are answered here.
    control: tokio::io::BufReader<tokio::net::UnixStream>,
}

/// A producer of a 640x480 grey window.
fn rig() -> Rig {
    let crate::native::display::TestPictures {
        display,
        helper,
        control,
        ..
    } = DisplayClient::test_pictures();
    let (frame_tx, _) = broadcast::channel(16);
    let media = Arc::new(StreamMedia::new(Default::default()));
    let cursors = CursorIdentities::new(
        frame_tx,
        media.clone(),
        Arc::new(RwLock::new(None)),
        Arc::new(RwLock::new(None)),
    );
    let producer = Producer::start(Source {
        display: display.clone(),
        media,
        cursors,
        runtime: tokio::runtime::Handle::current(),
    })
    .unwrap();
    Rig {
        producer,
        screen: FakeScreen::new(helper),
        display,
        control: tokio::io::BufReader::new(control),
    }
}

impl Rig {
    fn subscribe(&self) -> Subscription {
        self.subscribe_to(VideoCodec::Av1Full, 60)
    }

    fn subscribe_to(&self, codec: VideoCodec, rate: u32) -> Subscription {
        self.producer.subscribe(codec, rate).unwrap()
    }

    fn paint(&self, top: u32, bottom: u32, colour: [u8; 4]) {
        self.screen.paint(top, bottom, colour);
    }

    fn change(&self, change: Change) {
        self.screen.changes.send(change).unwrap();
    }

    /// The requests the helper received so far.
    fn requested(&self) -> Vec<Value> {
        self.screen.requested()
    }

    /// The next request on the helper's control socket.
    async fn control_request(&mut self) -> Value {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.control.read_line(&mut line))
            .await
            .expect("a control request")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn control_answer(&mut self, request: &Value, data: Value) {
        use tokio::io::AsyncWriteExt;
        let reply = serde_json::json!({"id": request["id"], "success": true, "data": data});
        self.control
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    }
}

async fn delivery(subscription: &Subscription) -> Delivery {
    tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("a delivery")
}

async fn unit(subscription: &Subscription) -> Arc<Unit> {
    match delivery(subscription).await {
        Delivery::Unit(unit) => unit,
        other => panic!("expected a unit, got {other:?}"),
    }
}

/// What a viewer received: every unit of its epoch, in order, and what
/// painting them shows. Decoding waits until asked, so it never delays the
/// units being timed.
struct Viewer {
    subscription: Subscription,
    units: Vec<Arc<Unit>>,
    /// When each unit arrived.
    arrivals: Vec<Instant>,
    decoder: Decoder,
    decoded: usize,
    shown: Option<Decoded>,
}

impl Viewer {
    fn new(subscription: Subscription) -> Self {
        Self {
            subscription,
            units: Vec::new(),
            arrivals: Vec::new(),
            decoder: Decoder::new(),
            decoded: 0,
            shown: None,
        }
    }

    async fn unit(&mut self) -> Arc<Unit> {
        let unit = unit(&self.subscription).await;
        self.units.push(unit.clone());
        self.arrivals.push(Instant::now());
        unit
    }

    /// Takes every unit until none comes for a while: the stream is still.
    async fn settle(&mut self) {
        while let Ok(Delivery::Unit(unit)) =
            tokio::time::timeout(Duration::from_millis(150), self.subscription.next()).await
        {
            self.units.push(unit);
            self.arrivals.push(Instant::now());
        }
    }

    /// Takes units until the painted picture shows `colour` at `row`, and
    /// returns the unit that first showed it.
    async fn until_shows(&mut self, row: u32, colour: [u8; 4]) -> Arc<Unit> {
        loop {
            let unit = self.unit().await;
            if near(self.painted(row), colour) {
                return unit;
            }
        }
    }

    /// Takes units until a refinement comes.
    async fn refined(&mut self) -> Arc<Unit> {
        loop {
            let unit = self.unit().await;
            if unit.quality == Quality::Final {
                return unit;
            }
        }
    }

    /// Every unit is a newer capture than the one before, the key unit a
    /// viewer asked for of the same capture, or the one refinement of the
    /// capture before it: no picture is doubled and none goes back in time.
    fn assert_ordered(&self) {
        for pair in self.units.windows(2) {
            let (before, after) = (&pair[0], &pair[1]);
            let ordered = match after.quality {
                Quality::Motion => after.ts > before.ts || (after.key && after.ts == before.ts),
                Quality::Refine | Quality::Final => after.ts == before.ts,
            };
            assert!(
                ordered,
                "{:?} {} after {:?} {}",
                after.quality, after.ts, before.quality, before.ts
            );
        }
    }

    /// The next unit of motion quality.
    async fn motion(&mut self) -> Arc<Unit> {
        loop {
            let unit = self.unit().await;
            if unit.quality == Quality::Motion {
                return unit;
            }
        }
    }

    /// The colour painted at `row` (RGB, halfway across) once every unit
    /// received so far is painted.
    fn painted(&mut self, row: u32) -> [u8; 3] {
        for unit in &self.units[self.decoded..] {
            self.shown = Some(self.decoder.decode(&unit.data));
        }
        self.decoded = self.units.len();
        let shown = self.shown.as_ref().expect("a painted picture");
        let (x, y) = (320, row as usize);
        let (cx, cy, chroma_width) = match shown.chroma {
            Chroma::Full => (x, y, shown.width as usize),
            Chroma::Subsampled => (x / 2, y / 2, shown.width as usize / 2),
        };
        to_rgb(
            shown.colour,
            shown.planes[0][y * shown.width as usize + x],
            shown.planes[1][cy * chroma_width + cx],
            shown.planes[2][cy * chroma_width + cx],
        )
    }
}

fn near(actual: [u8; 3], bgrx: [u8; 4]) -> bool {
    let expected = [bgrx[2], bgrx[1], bgrx[0]];
    actual
        .iter()
        .zip(expected)
        .all(|(actual, expected)| actual.abs_diff(expected) <= 6)
}

#[test]
fn aperture_offset_window_fits_its_complete_framebuffer_extent() {
    let encoding = Encoding::new(VideoCodec::Av1Full);
    let reply: PictureReply = serde_json::from_value(serde_json::json!({"width":2048,"height":2048,"stride":8192,"rows":[[0,2048]],"cursorIncluded":false,
        "visible":{"x":500,"y":80,"width":1418,"height":1888}})).unwrap();
    let pixels = vec![128u8; 2048 * 2048 * 4];
    let capture = Capture {
        ts: 1,
        read: Instant::now(),
        visible: reply.window(),
        surface: Surface::new(2048, 2048),
        input_seq: None,
        pointer: None,
    };
    encoding.mark(&reply);
    encoding.take(&capture, &reply, &pixels);
    let job = lock(&encoding.mailbox).job.take().unwrap();
    assert!(job.buffer.coded.0 >= 1918 && job.buffer.coded.1 >= 1968);
    assert_eq!(job.capture.visible, reply.window());
}

#[test]
fn aperture_gutter_cannot_change_any_decoded_coded_pixel() {
    for codec in [VideoCodec::Av1Full, VideoCodec::Av1] {
        // Odd origins challenge420 boundary averaging, not only flat padding.
        let visible = Rect {
            x: 101,
            y: 81,
            width: 63,
            height: 47,
        };
        let reply: PictureReply = serde_json::from_value(serde_json::json!({"width":256,"height":192,"stride":1024,"rows":[[0,192]],"cursorIncluded":false,"visible":visible})).unwrap();
        let mut results = Vec::new();
        for secret in [RED, BLUE] {
            let mut pixels = Vec::new();
            for y in 0..192 {
                for x in 0..256 {
                    pixels.extend_from_slice(
                        if (101..164).contains(&x) && (81..128).contains(&y) {
                            &GREEN
                        } else {
                            &secret
                        },
                    );
                }
            }
            let encoding = Encoding::new(codec);
            let capture = Capture {
                ts: 1,
                read: Instant::now(),
                visible,
                surface: Surface::new(256, 192),
                input_seq: None,
                pointer: None,
            };
            encoding.mark(&reply);
            encoding.take(&capture, &reply, &pixels);
            let job = lock(&encoding.mailbox).job.take().unwrap();
            let mut encoder =
                crate::native::video::open(codec, job.buffer.coded.0, job.buffer.coded.1, 2)
                    .unwrap();
            let unit = encoder
                .encode(
                    &job.buffer.picture.picture(),
                    EncodeRequest {
                        key: true,
                        quantizer: 32,
                        refine: false,
                    },
                )
                .unwrap();
            let decoded = Decoder::new().decode(&unit.data).rgb();
            assert!(decoded[..3].iter().all(|channel| *channel <= 2));
            results.push((unit.data, decoded));
        }
        assert_eq!(
            results[0], results[1],
            "secret gutter must influence neither bytes nor any decoded pixel"
        );
    }
}

#[test]
fn aperture_metadata_change_rebuilds_padding_without_pixel_damage() {
    let encoding = Encoding::new(VideoCodec::Av1Full);
    let mut reply: PictureReply = serde_json::from_value(serde_json::json!({"width":256,"height":192,"stride":1024,"rows":[[0,192]],"cursorIncluded":false})).unwrap();
    let pixels: Vec<u8> = (0..256 * 192).flat_map(|_| GREEN).collect();
    let capture = |reply: &PictureReply, ts| Capture {
        ts,
        read: Instant::now(),
        visible: reply.window(),
        surface: Surface::new(256, 192),
        input_seq: None,
        pointer: None,
    };
    encoding.mark(&reply);
    encoding.take(&capture(&reply, 1), &reply, &pixels);
    let first = lock(&encoding.mailbox).job.take().unwrap();
    lock(&encoding.mailbox).free.push(first.buffer);
    reply.visible = Some(Rect {
        x: 101,
        y: 81,
        width: 63,
        height: 47,
    });
    reply.rows.clear();
    encoding.mark(&reply);
    encoding.take(&capture(&reply, 2), &reply, &pixels);
    let second = lock(&encoding.mailbox)
        .job
        .take()
        .expect("new aperture is new work without pixel damage");
    let mut encoder = crate::native::video::open(
        VideoCodec::Av1Full,
        second.buffer.coded.0,
        second.buffer.coded.1,
        2,
    )
    .unwrap();
    let unit = encoder
        .encode(
            &second.buffer.picture.picture(),
            EncodeRequest {
                key: true,
                quantizer: 32,
                refine: false,
            },
        )
        .unwrap();
    let decoded = Decoder::new().decode(&unit.data).rgb();
    assert!(decoded[..3].iter().all(|channel| *channel <= 2));
    let inside = (100 * second.buffer.coded.0 as usize + 120) * 4;
    assert!(decoded[inside + 1] >= 250 && decoded[inside] <= 6 && decoded[inside + 2] <= 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_demand_without_video_uses_the_one_capture_thread_and_has_fresh_opaque_pixels() {
    let rig = rig();
    assert!(rig.requested().is_empty());
    rig.paint(100, 140, RED);
    let after = crate::native::stream::monotonic_us();
    let receive = rig.producer.snapshot(after).unwrap();
    let picture = tokio::time::timeout(Duration::from_secs(5), receive)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(picture.bounds.requested_us >= after);
    assert!(picture.bounds.picture_us >= picture.bounds.requested_us);
    assert!(picture.bounds.received_us >= picture.bounds.picture_us);
    assert!(!picture.surface.cursor_included);
    assert_eq!(
        picture.visible,
        Rect {
            x: 0,
            y: 0,
            width: 640,
            height: 480
        }
    );
    assert_eq!(
        picture
            .rgba(Rect {
                x: 320,
                y: 120,
                width: 1,
                height: 1
            })
            .unwrap()
            .into_raw(),
        [255, 0, 0, 255]
    );
    assert!(
        lock(&rig.producer.inner.state).encodings.is_empty(),
        "a raw demand starts no encoder"
    );
    let requests = rig.requested();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["force"], true);
    // Zero wait is omitted by the existing request serializer; the helper
    // reads absence as zero, which the fake uses too.
    assert_eq!(requests[0]["waitMs"].as_u64().unwrap_or(0), 0);
    assert_eq!(requests[0]["cursor"], false);
    assert!(!lock(&rig.producer.inner.state).snapshots.pending());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_demand_does_not_encode_for_a_blocked_viewer() {
    let rig = rig();
    let subscription = rig.subscribe();
    unit(&subscription).await;
    subscription.set_ready(false);
    rig.paint(100, 140, BLUE);
    let receive = rig
        .producer
        .snapshot(crate::native::stream::monotonic_us())
        .unwrap();
    let picture = tokio::time::timeout(Duration::from_secs(5), receive)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        picture
            .rgba(Rect {
                x: 320,
                y: 120,
                width: 1,
                height: 1
            })
            .unwrap()
            .into_raw(),
        [0, 0, 255, 255]
    );
    let encoding = lock(&rig.producer.inner.state).encodings[0].clone();
    let mailbox = lock(&encoding.mailbox);
    assert!(
        mailbox.behind,
        "normal motion catches up when its viewer is ready"
    );
    assert!(
        mailbox.job.is_none(),
        "snapshot demand never bypasses video backpressure"
    );
}

#[test]
fn a_capture_admitted_before_backpressure_is_marked_but_not_encoded_after_blocking() {
    let encoding = Arc::new(Encoding::new(VideoCodec::Av1Full));
    let subscriber = Arc::new(Subscriber::new(rate(60)));
    lock(&encoding.subscribers).push(subscriber.clone());
    let Decision::Capture(plan) = decide(&[encoding.clone()], Instant::now()) else {
        panic!("a ready first viewer admits its initial capture")
    };
    assert_eq!(plan.encodings.len(), 1);
    subscriber.set_ready(false);

    let reply: PictureReply = serde_json::from_value(serde_json::json!({
        "width": 640,
        "height": 480,
        "stride": 2560,
        "rows": [[0, 480]],
        "cursorIncluded": false
    }))
    .unwrap();
    let capture = Capture {
        ts: 1,
        read: Instant::now(),
        visible: reply.window(),
        surface: Surface::new(640, 480),
        input_seq: None,
        pointer: None,
    };
    let pixels = vec![128u8; 640 * 480 * 4];
    encoding.mark(&reply);
    plan.encodings[0].take_if_ready(&capture, &reply, &pixels);

    let mailbox = lock(&encoding.mailbox);
    assert!(
        mailbox.behind,
        "the blocked viewer must catch up when ready"
    );
    assert!(
        mailbox.job.is_none(),
        "backpressure survives an in-flight capture"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_snapshot_demand_has_no_plan_and_stopped_or_failed_producers_retire_requests() {
    let stopped = rig();
    // Hold the state lock so the capture thread cannot begin this canceled demand.
    {
        let mut state = lock(&stopped.producer.inner.state);
        drop(state.snapshots.request(0));
        assert!(!state.snapshots.pending());
        assert!(matches!(
            decide(&state.encodings, Instant::now()),
            Decision::Wait(None)
        ));
    }
    assert!(stopped.requested().is_empty());
    let receive = lock(&stopped.producer.inner.state).snapshots.request(0);
    stopped.producer.inner.stop();
    assert!(receive.await.unwrap().unwrap_err().contains("stopped"));
    assert!(stopped.producer.snapshot(0).is_err());

    let rig = rig();
    let receive = lock(&rig.producer.inner.state).snapshots.request(0);
    rig.producer.inner.fail("picture channel ended".into());
    assert_eq!(receive.await.unwrap().unwrap_err(), "picture channel ended");
    assert!(rig.producer.snapshot(0).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn constrained_refinement_steps_keep_the_reference_chain_and_new_damage_preempts_them() {
    let rig = rig();
    for y in 0..480u32 {
        rig.paint(
            y,
            y + 1,
            [(y * 17) as u8, (y * 71) as u8, (y * 113) as u8, 0],
        );
    }
    let subscription = rig.subscribe();
    subscription.set_link_rate(LinkRate {
        bits_per_second: 20_000,
        burst_bytes: 64 * 1024,
    });
    let first = unit(&subscription).await;
    assert!(first.key && first.quality == Quality::Motion);
    assert!(
        first.data.len() > 20_000 / 320,
        "the fixture must actually exercise regional work"
    );
    let mut decoder = Decoder::new();
    decoder.decode(&first.data);
    let step = unit(&subscription).await;
    assert_eq!(step.quality, Quality::Refine);
    assert_eq!(step.ts, first.ts);
    assert!(!step.key);
    decoder.decode(&step.data);
    let next = unit(&subscription).await;
    assert_eq!(next.quality, Quality::Refine);
    assert_eq!(next.ts, first.ts);
    decoder.decode(&next.data);
    rig.paint(100, 140, RED);
    for _ in 0..30 {
        let unit = unit(&subscription).await;
        decoder.decode(&unit.data);
        if unit.quality == Quality::Motion {
            assert!(unit.ts > first.ts && !unit.key);
            return;
        }
        assert_eq!(unit.ts, first.ts);
    }
    panic!("new damage remained queued behind the old refinement sweep");
}

/// A real writer charges the complete existing wire envelope to Flow, then
/// receives a paint acknowledgement only after those bytes can cross the
/// path. This deliberately keeps the key's serialization debt visible;
/// pulling encoded units eagerly is not a physically possible low-rate link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn constrained_refinement_with_wire_flow_and_serialized_paint_acknowledgements() {
    use super::super::subscription::Flow;
    let rig = rig();
    for y in 0..480u32 {
        rig.paint(
            y,
            y + 1,
            [(y * 17) as u8, (y * 71) as u8, (y * 113) as u8, 0],
        );
    }
    let subscription = rig.subscribe();
    let rate = LinkRate {
        bits_per_second: 20_000,
        burst_bytes: 64 * 1024,
    };
    subscription.set_link_rate(rate);
    let mut flow = Flow::default();
    flow.set_link_rate(Some(rate));
    let mut decoder = Decoder::new();
    let origin = Instant::now();
    let mut stream_id = uuid::Uuid::new_v4().to_string();
    let mut seq = 0;
    let mut received = 0;
    let mut preempted = false;
    for _ in 0..100 {
        let unit = match delivery(&subscription).await {
            Delivery::Unit(unit) => unit,
            Delivery::NewEpoch => {
                // The real track retires its old backlog and decoder epoch
                // when new damage is newer than its unwritten queue. This
                // records recovery rather than retrying a refused packet.
                flow.reset();
                seq = 0;
                stream_id = uuid::Uuid::new_v4().to_string();
                decoder = Decoder::new();
                subscription.set_ready(flow.has_room(subscription.queued_bytes()));
                eprintln!("FLOW_NEW_EPOCH");
                continue;
            }
            other => panic!("the wire-paced stream ended: {other:?}"),
        };
        seq += 1;
        received += 1;
        let wire =
            super::super::super::wire::binary_video(&unit, VideoCodec::Av1Full, &stream_id, seq)
                .expect("the actual wire accepts the encoded unit");
        assert!(unit.wire_bytes >= wire.len() && unit.wire_bytes - wire.len() <= 15);
        let sent_at = Instant::now();
        flow.sent(seq, wire.len(), sent_at);
        subscription.set_ready(flow.has_room(subscription.queued_bytes()));
        let serialization =
            Duration::from_secs_f64(wire.len() as f64 * 8.0 / f64::from(rate.bits_per_second));
        eprintln!(
            "FLOW_PICTURE {}",
            serde_json::json!({"seq":seq,"key":unit.key,"quality":unit.quality.label(),"payloadBytes":unit.data.len(),"wireBytes":wire.len(),"sentMs":origin.elapsed().as_secs_f64()*1000.0,"serializationMs":serialization.as_secs_f64()*1000.0,"queuedBytes":subscription.queued_bytes(),"burstBytes":flow.budget()})
        );
        tokio::time::sleep_until(tokio::time::Instant::from_std(sent_at + serialization)).await;
        decoder.decode(&unit.data);
        flow.acknowledge(seq, Instant::now());
        subscription.set_ready(flow.has_room(subscription.queued_bytes()));
        if received == 3 {
            rig.paint(100, 140, RED);
        } else if received > 3 && unit.quality == Quality::Motion {
            preempted = true;
        }
        if received >= 70 && preempted {
            break;
        }
    }
    rig.producer.inner.stop();
    assert!(
        preempted,
        "new damage must eventually preempt finite refinement"
    );
    assert!(
        received >= 70,
        "exercise the native model beyond its former64-frame abort"
    );
}

/// A new stream's first picture is taken whole and encoded as a key unit
/// at the window's size class; damage then travels as dependent units that
/// carry exactly what changed, and a still picture is refined to the still
/// target by re-encoding its capture, with no more damage and no new
/// capture. When the refinement is due is the policy's
/// (`a_still_picture_is_refined_once_and_new_damage_owes_it_again`); how
/// soon it arrives is the CPU's, measured on a real path by the real-Chrome
/// proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_starts_whole_follows_damage_and_refines_a_still_picture() {
    let rig = rig();
    let mut viewer = Viewer::new(rig.subscribe());
    let first = viewer.motion().await;
    assert!(first.key);
    assert_eq!(
        first.coded,
        (1024, 768),
        "a step up plus a step of headroom"
    );
    assert_eq!((first.visible.width, first.visible.height), (640, 480));
    assert_eq!(first.codec_string.as_deref(), Some("av01.1.08M.08"));
    assert!(!first.surface.cursor_included);
    assert!(near(viewer.painted(200), GREY));
    let asked = rig.requested();
    assert_eq!(asked[0]["force"], true, "{asked:?}");

    rig.paint(100, 140, RED);
    let moved = viewer.until_shows(120, RED).await;
    let refined = viewer.refined().await;
    assert!(moved.quality == Quality::Motion && !moved.key && moved.codec_string.is_none());
    assert!(moved.ts > first.ts);
    assert!(!refined.key);
    assert_eq!(
        refined.ts, moved.ts,
        "a refinement re-encodes the same capture"
    );
    viewer.assert_ordered();
    assert!(near(viewer.painted(120), RED));
    assert!(near(viewer.painted(200), GREY));
}

/// A still picture is refined only once the window's geometry holds too. A
/// drag lays the window out again within a frame or two of each picture, so
/// none of its pictures is refined: while a layout is in flight the
/// refinement waits, and it follows the layout's answer by the still time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picture_is_not_refined_while_the_window_is_laid_out() {
    let mut rig = rig();
    let mut viewer = Viewer::new(rig.subscribe());
    viewer.motion().await;
    viewer.refined().await;
    let display = rig.display.clone();
    let layout = tokio::spawn(async move {
        let guard = display.layout().await;
        display.resize(&guard, 640, 480, None, false).await.is_ok()
    });
    let request = rig.control_request().await;
    assert_eq!(request["op"], "resize", "{request}");
    rig.paint(100, 140, RED);
    viewer.until_shows(120, RED).await;
    let moved = viewer.units.len();
    // Held well past the still time: a refinement due by the picture alone
    // would come now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let answered = Instant::now();
    rig.control_answer(
        &request,
        serde_json::json!({"width": 640, "height": 480, "windows": []}),
    )
    .await;
    assert!(layout.await.unwrap(), "the layout is answered");
    viewer.refined().await;
    let refined = viewer.units[moved..]
        .iter()
        .position(|unit| unit.quality == Quality::Final)
        .map(|index| viewer.arrivals[moved + index])
        .expect("a refinement");
    assert!(
        refined >= answered + super::super::policy::STILL_AFTER,
        "refined {:?} after the layout was answered",
        refined.saturating_duration_since(answered)
    );
    viewer.assert_ordered();
}

/// Key units come on a subscription, on a viewer's request and on a new
/// coded size, never otherwise; viewers of one kind share one encoder, so
/// they receive the very same units.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_units_come_on_subscription_request_and_coded_size_only() {
    let rig = rig();
    let first = rig.subscribe();
    assert!(unit(&first).await.key);
    for band in 0..3 {
        rig.paint(band * 10, band * 10 + 5, RED);
        // A refinement of the previous picture may come first; it is never a
        // key unit either.
        let unit = loop {
            let unit = unit(&first).await;
            assert!(!unit.key, "only a request or a new size makes a key unit");
            if unit.quality == Quality::Motion {
                break unit;
            }
        };
        assert!(!unit.key);
    }
    let second = rig.subscribe();
    let joined = loop {
        let unit = unit(&first).await;
        if unit.key {
            break unit;
        }
    };
    let shared = unit(&second).await;
    assert!(Arc::ptr_eq(&joined, &shared), "one encoder serves both");

    first.keyframe();
    let asked = loop {
        let unit = unit(&second).await;
        if unit.key {
            break unit;
        }
    };
    assert_eq!(asked.coded, (1024, 768));

    // Inside the size class: the window grows, no key unit.
    rig.change(Change::Layout {
        framebuffer: (1024, 768),
        window: (700, 480),
        colour: BLUE,
    });
    let grown = loop {
        let unit = unit(&second).await;
        if unit.visible.width == 700 {
            break unit;
        }
    };
    assert!(!grown.key && grown.coded == (1024, 768));
    // Past it: a new coded size begins with a key unit.
    rig.change(Change::Layout {
        framebuffer: (1200, 768),
        window: (1100, 480),
        colour: GREEN,
    });
    let resized = loop {
        let unit = unit(&second).await;
        if unit.visible.width == 1100 {
            break unit;
        }
    };
    assert!(resized.key);
    assert_eq!(resized.coded, (1536, 768));
}

/// Pictures never carry the native pointer: video viewers draw it
/// themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pictures_never_carry_the_pointer() {
    let rig = rig();
    let viewer = rig.subscribe();
    for band in 0..3 {
        rig.paint(band * 8, band * 8 + 4, RED);
        assert!(!unit(&viewer).await.surface.cursor_included);
    }
    let asked = rig.requested();
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|asked| asked["cursor"] == false),
        "{asked:?}"
    );
}

/// Viewers of another codec or another rate get their own encoding. The
/// helper reports a change once, to whichever picture comes next; an
/// encoding that picture was not for catches up from the slot when it is
/// due, by a capture of its own with no more damage. However late the
/// threads run, the change reaches both viewers, no picture is doubled or
/// goes back in time, and each still picture is refined. That no wait for
/// damage holds the catch-up back is
/// `a_wait_for_damage_ends_when_another_encoding_owes_a_picture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encoding_a_picture_was_not_for_catches_up_when_due() {
    let rig = rig();
    let mut fast = Viewer::new(rig.subscribe_to(VideoCodec::Av1Full, 60));
    // A second's period: a change right after its picture reaches only the
    // fast encoding.
    let mut slow = Viewer::new(rig.subscribe_to(VideoCodec::Av1, 1));
    fast.motion().await;
    slow.motion().await;
    for colour in [RED, BLUE, GREEN] {
        rig.paint(200, 220, colour);
        let alone = fast.until_shows(210, colour).await;
        let caught_up = slow.until_shows(210, colour).await;
        if caught_up.ts <= alone.ts {
            // A thread ran a second late and the slow encoding was due when
            // the change came, so it took the change itself: again, right
            // after that picture.
            continue;
        }
        assert!(!caught_up.key, "a catch-up is a dependent unit");
        for viewer in [&mut fast, &mut slow] {
            viewer.refined().await;
            viewer.assert_ordered();
            assert!(near(viewer.painted(210), colour));
        }
        return;
    }
    panic!("the slow encoding was due for every change");
}

/// The capture thread serves one request at a time, so a wait for damage
/// holds back every encoding: it never runs past the moment another
/// encoding owes a picture, to the wire's millisecond.
#[test]
fn a_wait_for_damage_ends_when_another_encoding_owes_a_picture() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    // Due now (its 16.7 ms period has passed) and has seen the screen.
    let fast = || encoding(60, Some(ms(20)), false, false, now);
    let wait = |encodings: &[Arc<Encoding>]| captured(decide(encodings, now)).wait_ms;
    assert_eq!(
        wait(&[fast()]),
        PICTURE_WAIT_MS,
        "nothing else: the whole wait"
    );
    assert_eq!(
        wait(&[fast(), encoding(10, Some(ms(10)), false, false, now)]),
        PICTURE_WAIT_MS,
        "an encoding that has seen the screen owes nothing"
    );
    assert_eq!(
        wait(&[fast(), encoding(10, Some(ms(10)), true, false, now)]),
        90,
        "one that has not is due 90 ms from now"
    );
    assert_eq!(
        wait(&[fast(), encoding(2, Some(ms(10)), true, false, now)]),
        PICTURE_WAIT_MS,
        "one due after the whole wait"
    );
    assert_eq!(
        wait(&[
            fast(),
            encoding(10, Some(ms(10)), true, false, now),
            encoding(10, Some(ms(40)), true, false, now),
        ]),
        60,
        "the earliest owed"
    );
}

/// An encoding whose encoder holds both buffers takes no picture; it owes
/// one once a buffer comes back, which the capture thread cannot hear while
/// the helper waits: a wait for damage ends when it is due, and after that
/// within its period.
#[test]
fn an_encoding_waiting_for_its_encoder_is_looked_at_within_its_period() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let fast = || encoding(60, Some(ms(20)), false, false, now);
    let wait = |encodings: &[Arc<Encoding>]| captured(decide(encodings, now)).wait_ms;
    assert_eq!(
        wait(&[fast(), encoding(10, Some(ms(10)), true, true, now)]),
        90,
        "its due time"
    );
    assert_eq!(
        wait(&[fast(), encoding(60, Some(ms(20)), true, true, now)]),
        17,
        "a period from now, to the millisecond"
    );
    assert_eq!(
        wait(&[fast(), encoding(60, Some(ms(20)), false, true, now)]),
        PICTURE_WAIT_MS,
        "it has seen the screen"
    );
    assert!(
        matches!(
            decide(&[encoding(60, Some(ms(20)), true, true, now)], now),
            Decision::Wait(None)
        ),
        "its encoder wakes the thread"
    );
}

/// A picture that needs no damage asks for none; a whole one waits only for
/// a layout in progress; and with nothing due the thread sleeps until the
/// next encoding is due.
#[test]
fn a_capture_asks_for_what_its_encodings_need() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let request = captured(decide(&[encoding(60, Some(ms(20)), true, false, now)], now));
    assert_eq!((request.force, request.wait_ms), (false, 0));
    let request = captured(decide(
        &[
            encoding(60, None, true, false, now),
            encoding(10, Some(ms(10)), true, false, now),
        ],
        now,
    ));
    assert_eq!((request.force, request.wait_ms), (true, PICTURE_WAIT_MS));
    match decide(
        &[
            encoding(60, Some(ms(5)), false, false, now),
            encoding(60, Some(ms(20)), true, true, now),
        ],
        now,
    ) {
        Decision::Wait(at) => assert_eq!(at, Some(now - ms(5) + period(60))),
        Decision::Capture(_) => panic!("nothing is due"),
    }
    assert!(matches!(decide(&[], now), Decision::Wait(None)));
}

/// An encoding of `rate` whose last picture was `ago` before `now` (none:
/// it has none yet), that has seen the screen or not, and whose encoder
/// holds both buffers or not.
fn encoding(
    rate: u32,
    ago: Option<Duration>,
    behind: bool,
    busy: bool,
    now: Instant,
) -> Arc<Encoding> {
    let encoding = Arc::new(Encoding::new(VideoCodec::Av1Full));
    lock(&encoding.subscribers).push(Arc::new(Subscriber::new(rate)));
    let mut mailbox = lock(&encoding.mailbox);
    mailbox.pictured = ago.map(|ago| now - ago);
    mailbox.behind = behind;
    if busy {
        mailbox.free.clear();
    }
    drop(mailbox);
    encoding
}

fn captured(decision: Decision) -> PictureRequest {
    match decision {
        Decision::Capture(plan) => plan.request,
        Decision::Wait(at) => panic!("a capture, not a wait until {at:?}"),
    }
}

/// A viewer whose path has no room skips captures; the damage accumulates
/// and its next picture shows all of it. Nothing encoded is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_path_skips_captures_and_the_damage_accumulates() {
    let rig = rig();
    let mut viewer = Viewer::new(rig.subscribe());
    viewer.motion().await;
    viewer.subscription.set_ready(false);
    // A refinement already due may still be in flight.
    viewer.settle().await;
    rig.requested();
    rig.paint(10, 20, BLUE);
    rig.paint(300, 310, GREEN);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        rig.requested().is_empty(),
        "no capture while the path is full"
    );
    viewer.subscription.set_ready(true);
    let unit = viewer.motion().await;
    assert!(!unit.key);
    assert!(near(viewer.painted(15), BLUE));
    assert!(near(viewer.painted(305), GREEN));
}

/// A viewer that falls more than a second behind loses its backlog at the
/// producer and begins a new epoch with a key unit; nothing is skipped
/// inside an epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_a_second_behind_is_resynchronized_with_a_key_unit() {
    let rig = rig();
    let slow = rig.subscribe();
    let origin = unit(&slow).await;
    assert!(origin.key);
    for step in 0..70 {
        rig.paint(step, step + 1, RED);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut last = origin.ts;
    loop {
        match delivery(&slow).await {
            Delivery::Unit(unit) => {
                assert!(!unit.key, "no key unit inside the epoch");
                assert!(unit.ts >= last);
                last = unit.ts;
            }
            Delivery::NewEpoch => break,
            Delivery::Ended(reason) => panic!("ended: {reason}"),
        }
    }
    assert!(unit(&slow).await.key, "a new epoch begins with a key unit");
}

/// A helper that answers incoherently ends the channel, and every viewer
/// learns why instead of waiting forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_helper_ends_every_subscription_with_its_reason() {
    let rig = rig();
    let viewer = rig.subscribe();
    assert!(unit(&viewer).await.key);
    // The break comes with the helper's next answer, whether the producer
    // is waiting in it or asks next; the helper then leaves, so nothing more
    // can be painted.
    rig.change(Change::Break);
    loop {
        match delivery(&viewer).await {
            Delivery::Unit(_) => continue,
            Delivery::NewEpoch => panic!("a new epoch from a broken helper"),
            Delivery::Ended(reason) => {
                assert!(reason.contains("pictures"), "{reason}");
                break;
            }
        }
    }
    assert!(
        rig.producer.subscribe(VideoCodec::Av1Full, 60).is_err(),
        "a failed producer serves no one"
    );
}
