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
}

/// A producer of a 640x480 grey window.
fn rig() -> Rig {
    let crate::native::display::TestPictures {
        display, helper, ..
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
        display,
        media,
        cursors,
        runtime: tokio::runtime::Handle::current(),
    })
    .unwrap();
    Rig {
        producer,
        screen: FakeScreen::new(helper),
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
    decoder: Decoder,
    decoded: usize,
    shown: Option<Decoded>,
}

impl Viewer {
    fn new(subscription: Subscription) -> Self {
        Self {
            subscription,
            units: Vec::new(),
            decoder: Decoder::new(),
            decoded: 0,
            shown: None,
        }
    }

    async fn unit(&mut self) -> Arc<Unit> {
        let unit = unit(&self.subscription).await;
        self.units.push(unit.clone());
        unit
    }

    /// Takes every unit until none comes for a while: the stream is still.
    async fn settle(&mut self) {
        while let Ok(Delivery::Unit(unit)) =
            tokio::time::timeout(Duration::from_millis(150), self.subscription.next()).await
        {
            self.units.push(unit);
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

/// A new stream's first picture is taken whole and encoded as a key unit
/// at the window's size class; damage then travels as dependent units that
/// carry exactly what changed, and a still picture is refined to the still
/// target soon after the last damage.
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
    let damaged = std::time::Instant::now();
    let moved = viewer.motion().await;
    let refined = viewer.unit().await;
    let refined_after = damaged.elapsed();
    assert!(!moved.key && moved.codec_string.is_none());
    assert!(moved.ts >= first.ts);
    assert_eq!(refined.quality, Quality::Final);
    assert!(!refined.key);
    assert_eq!(
        refined.ts, moved.ts,
        "a refinement re-encodes the same capture"
    );
    assert!(
        refined_after < Duration::from_millis(150),
        "refined {refined_after:?} after the damage"
    );
    assert!(near(viewer.painted(120), RED));
    assert!(near(viewer.painted(200), GREY));
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
/// due, without waiting for more damage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encoding_a_picture_was_not_for_catches_up_when_due() {
    let rig = rig();
    let mut fast = Viewer::new(rig.subscribe_to(VideoCodec::Av1Full, 60));
    let mut slow = Viewer::new(rig.subscribe_to(VideoCodec::Av1, 10));
    // Each new encoding's first picture is whole, so the other may see an
    // unchanged picture again: wait until both are still.
    tokio::join!(fast.settle(), slow.settle());
    rig.paint(100, 120, RED);
    fast.motion().await;
    slow.motion().await;
    // Right after the slow encoding's picture: only the fast one is due.
    rig.paint(200, 220, BLUE);
    let painted_at = std::time::Instant::now();
    fast.motion().await;
    let caught_up = slow.motion().await;
    let elapsed = painted_at.elapsed();
    assert!(!caught_up.key);
    // Due one period after its previous picture, which preceded the change;
    // waiting for more damage would add the helper's whole wait.
    assert!(
        elapsed < period(10) + Duration::from_millis(50),
        "caught up {elapsed:?} after the change"
    );
    assert!(near(fast.painted(210), BLUE));
    assert!(
        near(slow.painted(210), BLUE),
        "the slow stream shows the change"
    );
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
    rig.change(Change::Break);
    rig.paint(0, 4, RED);
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
