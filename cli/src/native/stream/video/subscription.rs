//! One viewer's subscription to an encoding: the units it has not written
//! yet, where its epoch begins, and what its path delivers.
//!
//! The producer pushes every unit it encodes; nothing encoded is dropped for
//! a viewer except by ending its epoch. An epoch begins at a key unit. A
//! viewer more than a second behind (its oldest unwritten unit a second older
//! than the newest) begins a new epoch at the next key unit instead: its
//! backlog is dropped here, at the producer, never skipped on the wire.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::policy::{LinkRate, Quality};
use crate::native::display::{Rect, Surface};

/// One encoded picture, shared by every subscriber of its encoding.
#[derive(Debug)]
pub(crate) struct Unit {
    pub data: Vec<u8>,
    /// Canonical wire header+payload bound, computed once before publication.
    pub wire_bytes: usize,
    pub key: bool,
    /// Sandbox monotonic microseconds at capture.
    pub ts: u64,
    pub coded: (u32, u32),
    pub visible: Rect,
    pub surface: Surface,
    pub input_seq: Option<u64>,
    pub quality: Quality,
    /// The stream's WebCodecs codec string, on key units only.
    pub codec_string: Option<String>,
    /// An exact unit (contract browser-presentation-units): `data` is a PNG
    /// of the capture's pixels in this rectangle of the coded picture, drawn
    /// over the viewer's surface and never decoded as video. Never a key; its
    /// `quality` is the picture's under it and goes unsent.
    pub exact: Option<Rect>,
    /// With diagnostics on, when the picture passed each producer stage.
    pub stages: Option<super::stages::Stages>,
}

impl Unit {
    /// What the unit is, as diagnostics name it: `exact`, or its quality.
    pub(crate) fn label(&self) -> &'static str {
        match self.exact {
            Some(_) => "exact",
            None => self.quality.label(),
        }
    }

    /// Raster geometry of the actual applied owned window. Cursor inclusion
    /// changes pixels, not the source's coordinate/identity contract.
    pub(super) fn follows(&self, surface: &Surface, window: (u32, u32)) -> bool {
        let mut expected = surface.clone();
        expected.cursor_included = self.surface.cursor_included;
        self.surface == expected && (self.visible.width, self.visible.height) == window
    }
}

/// How far behind a viewer may fall before it is resynchronized.
pub(super) const RESYNC_AFTER: Duration = Duration::from_secs(1);

/// What a subscriber's next pull yields.
#[derive(Debug)]
pub(crate) enum Delivery {
    /// The next unit of the current epoch.
    Unit(Arc<Unit>),
    /// The previous epoch ended; the next unit (a key unit) begins another.
    NewEpoch,
    /// The producer can no longer serve this subscription.
    Ended(String),
}

#[derive(Default)]
struct Queue {
    units: VecDeque<Arc<Unit>>,
    /// Units are accepted only from a key unit on.
    awaiting_key: bool,
    /// A new epoch must be announced before the next unit.
    restarted: bool,
    ended: Option<String>,
}

/// The shared half: the producer pushes, the viewer's writer pulls.
pub(crate) struct Subscriber {
    queue: Mutex<Queue>,
    notify: tokio::sync::Notify,
    /// Whether this viewer's path has room for another picture.
    ready: AtomicBool,
    /// Pictures per second this viewer is served at most.
    rate: AtomicU32,
    link_rate: AtomicU64,
    /// The viewer declared `videoExact=png`: it draws exact units.
    exact: bool,
}

impl Subscriber {
    pub(super) fn new(rate: u32, exact: bool) -> Self {
        Self {
            queue: Mutex::new(Queue {
                awaiting_key: true,
                ..Queue::default()
            }),
            notify: tokio::sync::Notify::new(),
            ready: AtomicBool::new(true),
            rate: AtomicU32::new(rate),
            link_rate: AtomicU64::new(0),
            exact,
        }
    }

    pub(super) fn exact(&self) -> bool {
        self.exact
    }

    pub(super) fn rate(&self) -> u32 {
        self.rate.load(Ordering::Acquire)
    }

    pub(super) fn set_rate(&self, rate: u32) {
        self.rate.store(rate, Ordering::Release);
    }

    pub(super) fn set_link_rate(&self, rate: LinkRate) {
        self.link_rate.store(
            (u64::from(rate.bits_per_second) << 32) | u64::from(rate.burst_bytes),
            Ordering::Release,
        );
    }

    pub(super) fn link_rate(&self) -> Option<LinkRate> {
        let rate = self.link_rate.load(Ordering::Acquire);
        (rate != 0).then_some(LinkRate {
            bits_per_second: (rate >> 32) as u32,
            burst_bytes: rate as u32,
        })
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Offers one unit. Returns true when this subscriber needs a key unit
    /// (it is waiting for one to begin its epoch, or it just fell behind).
    pub(super) fn push(&self, unit: &Arc<Unit>) -> bool {
        let mut queue = self.queue();
        if queue.ended.is_some() {
            return false;
        }
        let behind = queue.units.front().is_some_and(|oldest| {
            unit.ts.saturating_sub(oldest.ts) > RESYNC_AFTER.as_micros() as u64
        });
        // Age alone cannot bound a finite sweep of the same capture: all its
        // timestamps match. One indivisible picture is allowed when empty;
        // further queued bytes stay within the existing path's burst.
        let queued: usize = queue.units.iter().map(|unit| unit.wire_bytes).sum();
        let budget = self
            .link_rate()
            .map_or(INITIAL_BUDGET, |rate| rate.burst_bytes as usize);
        let full = !queue.units.is_empty() && queued.saturating_add(unit.wire_bytes) > budget;
        if behind || full {
            queue.units.clear();
            queue.awaiting_key = true;
            queue.restarted = true;
        }
        if queue.restarted {
            // A blocked writer has not yet retired its old epoch. Keep no
            // additional units and ask for no repeated key; delivering the
            // epoch transition requests exactly one replacement.
            drop(queue);
            self.notify.notify_one();
            return false;
        }
        if queue.awaiting_key && (!self.ready() || !unit.key) {
            drop(queue);
            self.notify.notify_one();
            return self.ready();
        }
        queue.awaiting_key = false;
        queue.units.push_back(unit.clone());
        drop(queue);
        self.notify.notify_one();
        false
    }

    /// The producer ends every epoch of this subscription.
    pub(super) fn end(&self, reason: &str) {
        let mut queue = self.queue();
        queue.units.clear();
        queue.ended.get_or_insert_with(|| reason.to_owned());
        drop(queue);
        self.notify.notify_one();
    }

    pub(super) fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// The next delivery, waiting for one.
    pub(crate) async fn next(&self) -> Delivery {
        loop {
            let notified = self.notify.notified();
            {
                let mut queue = self.queue();
                if let Some(reason) = &queue.ended {
                    return Delivery::Ended(reason.clone());
                }
                // VideoTrack's existing in-flight Flow guard decides when
                // this writer may observe/retire the old epoch.
                if queue.restarted {
                    queue.restarted = false;
                    return Delivery::NewEpoch;
                }
                if let Some(unit) = queue.units.pop_front() {
                    return Delivery::Unit(unit);
                }
            }
            notified.await;
        }
    }

    /// Publishes whether the viewer's path has room (see `Flow`).
    pub(crate) fn set_ready(&self, ready: bool) -> bool {
        let changed = self.ready.swap(ready, Ordering::AcqRel) != ready;
        if changed && ready {
            self.notify.notify_one();
        }
        changed
    }

    pub(super) fn needs_key(&self) -> bool {
        let queue = self.queue();
        queue.awaiting_key && !queue.restarted && queue.ended.is_none()
    }

    /// Bytes waiting to be written.
    pub(crate) fn queued_bytes(&self) -> usize {
        self.queue().units.iter().map(|unit| unit.wire_bytes).sum()
    }
}

/// How long a delivery-rate sample informs the budget, and how long the
/// shortest round trip stands (the JPEG delivery bound's windows).
const RATE_WINDOW: Duration = Duration::from_secs(5);
const ROUND_TRIP_WINDOW: Duration = Duration::from_secs(10);
/// Before any acknowledgement: this much may be in flight.
const INITIAL_BUDGET: usize = 2 * 1024 * 1024;

struct Sent {
    seq: u64,
    bytes: usize,
    at: Instant,
    delivered_before: u64,
}

/// A viewer's units written but not yet acknowledged as painted, and what
/// the acknowledgements say about its path. The producer captures a picture
/// for a viewer only while the bytes ahead of it fit what the path delivers
/// in its shortest round trip (at least one picture of the last size), so a
/// picture never waits behind a standing queue; captures are skipped, and
/// the damage accumulates into the next one.
#[derive(Default)]
pub(crate) struct Flow {
    sent: VecDeque<Sent>,
    bytes: usize,
    delivered: u64,
    last_unit: usize,
    rates: VecDeque<(Instant, f64)>,
    round_trips: VecDeque<(Instant, Duration)>,
    burst: Option<usize>,
}

impl Flow {
    pub(crate) fn set_link_rate(&mut self, rate: Option<LinkRate>) {
        self.burst = rate.map(|rate| rate.burst_bytes as usize);
    }
    pub(crate) fn sent(&mut self, seq: u64, bytes: usize, at: Instant) {
        self.sent.push_back(Sent {
            seq,
            bytes,
            at,
            delivered_before: self.delivered,
        });
        self.bytes += bytes;
        self.last_unit = bytes;
    }

    /// A cumulative acknowledgement of `seq` at `at`.
    pub(crate) fn acknowledge(&mut self, seq: u64, at: Instant) {
        let mut newest = None;
        while self.sent.front().is_some_and(|sent| sent.seq <= seq) {
            let sent = self.sent.pop_front().unwrap();
            self.bytes -= sent.bytes;
            self.delivered += sent.bytes as u64;
            newest = Some(sent);
        }
        let Some(sent) = newest else {
            return;
        };
        let round_trip = at.saturating_duration_since(sent.at);
        if round_trip.is_zero() {
            return;
        }
        let rate = (self.delivered - sent.delivered_before) as f64 / round_trip.as_secs_f64();
        record(&mut self.rates, at, rate, RATE_WINDOW);
        record(&mut self.round_trips, at, round_trip, ROUND_TRIP_WINDOW);
    }

    /// A new epoch: nothing of the previous one is awaited.
    pub(crate) fn reset(&mut self) {
        self.sent.clear();
        self.bytes = 0;
    }

    /// What may be in flight: the best recent rate over the shortest recent
    /// round trip, and never less than one picture of the last size.
    pub(crate) fn budget(&self) -> usize {
        if let Some(burst) = self.burst {
            return burst;
        }
        let rate = self.rates.iter().map(|(_, rate)| *rate).reduce(f64::max);
        let round_trip = self.round_trips.iter().map(|(_, rtt)| *rtt).min();
        match (rate, round_trip) {
            (Some(rate), Some(round_trip)) => {
                ((rate * round_trip.as_secs_f64()) as usize).max(self.last_unit)
            }
            _ => INITIAL_BUDGET,
        }
    }

    /// Whether another picture may be captured for this viewer, with
    /// `queued` bytes still unwritten.
    pub(crate) fn has_room(&self, queued: usize) -> bool {
        self.bytes + queued <= self.budget()
    }

    /// The highest sequence written, for acknowledgements to stay below.
    pub(crate) fn last_sent(&self) -> Option<u64> {
        self.sent.back().map(|sent| sent.seq)
    }
}

fn record<T>(samples: &mut VecDeque<(Instant, T)>, now: Instant, value: T, window: Duration) {
    samples.push_back((now, value));
    while samples
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) > window)
    {
        samples.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn unit(ts: u64, key: bool, bytes: usize) -> Arc<Unit> {
        let mut unit = Unit {
            data: vec![0; bytes],
            wire_bytes: 0,
            key,
            ts,
            coded: (2048, 2048),
            visible: Rect {
                x: 0,
                y: 0,
                width: 1840,
                height: 1888,
            },
            surface: Surface::new(2048, 2048),
            input_seq: None,
            quality: Quality::Motion,
            codec_string: key.then(|| "av01.1.12M.08".into()),
            exact: None,
            stages: None,
        };
        unit.wire_bytes = crate::native::stream::wire::video_budget_bytes(
            &unit,
            crate::native::video::VideoCodec::Av1Full,
        )
        .unwrap();
        Arc::new(unit)
    }

    async fn delivered(subscriber: &Subscriber) -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_millis(20), subscriber.next()).await;
            match next {
                Ok(Delivery::Unit(unit)) => {
                    seen.push(format!("{}{}", if unit.key { "K" } else { "p" }, unit.ts))
                }
                Ok(Delivery::NewEpoch) => seen.push("epoch".into()),
                Ok(Delivery::Ended(reason)) => {
                    seen.push(format!("ended:{reason}"));
                    return seen;
                }
                Err(_) => return seen,
            }
        }
    }

    /// An epoch begins at a key unit: what came before is not this viewer's,
    /// and it says so by asking for one.
    #[tokio::test]
    async fn an_epoch_begins_at_a_key_unit() {
        let subscriber = Subscriber::new(60, false);
        assert!(subscriber.push(&unit(1, false, 10)), "asks for a key unit");
        assert!(!subscriber.push(&unit(2, true, 10)));
        assert!(!subscriber.push(&unit(3, false, 10)));
        assert_eq!(delivered(&subscriber).await, ["K2", "p3"]);
    }

    /// A viewer more than a second behind loses its backlog here and begins
    /// a new epoch at the next key unit; the units before it never reach it.
    #[tokio::test]
    async fn a_viewer_a_second_behind_begins_a_new_epoch_at_the_next_key_unit() {
        let subscriber = Subscriber::new(60, false);
        subscriber.push(&unit(0, true, 10));
        for ts in [100_000, 500_000, 1_000_000] {
            assert!(!subscriber.push(&unit(ts, false, 10)));
        }
        assert!(
            !subscriber.push(&unit(1_000_001, false, 10)),
            "the old epoch must retire before requesting a key"
        );
        assert!(
            !subscriber.push(&unit(1_000_002, false, 10)),
            "no key treadmill while the writer is blocked"
        );
        assert!(matches!(subscriber.next().await, Delivery::NewEpoch));
        subscriber.push(&unit(1_000_003, true, 10));
        subscriber.push(&unit(1_000_004, false, 10));
        assert_eq!(delivered(&subscriber).await, ["K1000003", "p1000004"]);
    }

    /// A viewer that keeps up is never resynchronized, however long it runs.
    #[tokio::test]
    async fn a_viewer_that_keeps_up_stays_in_its_epoch() {
        let subscriber = Subscriber::new(60, false);
        subscriber.push(&unit(0, true, 10));
        for picture in 1..=180 {
            let ts = picture * 16_667;
            assert!(!subscriber.push(&unit(ts, false, 10)));
            assert!(matches!(subscriber.next().await, Delivery::Unit(_)));
        }
        assert!(matches!(subscriber.next().await, Delivery::Unit(_)));
        assert_eq!(subscriber.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn an_ended_subscription_says_why_and_takes_nothing_more() {
        let subscriber = Subscriber::new(60, false);
        subscriber.push(&unit(0, true, 10));
        subscriber.end("encoder failed");
        assert!(!subscriber.push(&unit(1, true, 10)));
        assert_eq!(delivered(&subscriber).await, ["ended:encoder failed"]);
    }

    #[tokio::test]
    async fn tiny_units_charge_their_complete_wire_cost_while_queued_and_release_it_on_pull() {
        let subscriber = Subscriber::new(60, false);
        let key = unit(1, true, 42);
        let delta = unit(2, false, 42);
        assert!(key.wire_bytes > key.data.len() && delta.wire_bytes > delta.data.len());
        subscriber.push(&key);
        subscriber.push(&delta);
        assert_eq!(subscriber.queued_bytes(), key.wire_bytes + delta.wire_bytes);
        let mut flow = Flow::default();
        flow.set_link_rate(Some(LinkRate {
            bits_per_second: 20_000,
            burst_bytes: 100,
        }));
        assert!(
            !flow.has_room(subscriber.queued_bytes()),
            "84 raw bytes fit; complete envelopes must not"
        );
        assert!(matches!(subscriber.next().await, Delivery::Unit(_)));
        assert_eq!(subscriber.queued_bytes(), delta.wire_bytes);
        assert!(matches!(subscriber.next().await, Delivery::Unit(_)));
        assert_eq!(subscriber.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn a_stalled_same_capture_queue_is_quarantined_without_repeated_keys() {
        let subscriber = Subscriber::new(60, false);
        subscriber.set_link_rate(LinkRate {
            bits_per_second: 500_000,
            burst_bytes: 1024,
        });
        let first = unit(1, true, 100);
        subscriber.push(&first);
        assert!(matches!(subscriber.next().await, Delivery::Unit(_)));
        subscriber.set_ready(false);
        for _ in 0..1000 {
            assert!(!subscriber.push(&unit(1, false, 100)));
            assert!(subscriber.queued_bytes() <= 1024);
        }
        assert!(
            !subscriber.needs_key(),
            "wait for the old writer epoch before requesting"
        );
        assert!(matches!(subscriber.next().await, Delivery::NewEpoch));
        assert!(subscriber.needs_key());
        assert!(
            !subscriber.push(&unit(1, true, 100)),
            "blocked subscriber retains no new key"
        );
        assert_eq!(subscriber.queued_bytes(), 0);
        subscriber.set_ready(true);
        let key = unit(2, true, 100);
        subscriber.push(&key);
        match subscriber.next().await {
            Delivery::Unit(actual) => assert!(Arc::ptr_eq(&actual, &key)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "actual native key ownership: AMBIT_LARGE_NATIVE_KEY"]
    async fn eight_slow_or_stalled_viewers_share_one_native_key_and_bound_unwritten_retention() {
        let rss = || {
            std::fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find(|line| line.starts_with("VmRSS:"))
                .unwrap()
                .to_owned()
        };
        let before = rss();
        let mut shared = Arc::try_unwrap(unit(1, true, 1)).unwrap();
        shared.data = std::fs::read(std::env::var("AMBIT_LARGE_NATIVE_KEY").unwrap()).unwrap();
        shared.coded = (4096, 4096);
        shared.visible = Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 4096,
        };
        shared.surface = Surface::new(4096, 4096);
        shared.codec_string = Some("av01.1.16M.08".into());
        shared.wire_bytes = crate::native::stream::wire::video_budget_bytes(
            &shared,
            crate::native::video::VideoCodec::Av1Full,
        )
        .unwrap();
        let key = Arc::new(shared);
        let viewers: Vec<_> = (0..8).map(|_| Subscriber::new(60, false)).collect();
        for viewer in &viewers {
            viewer.set_link_rate(LinkRate {
                bits_per_second: 500_000,
                burst_bytes: 1024,
            });
            assert!(!viewer.push(&key));
            assert_eq!(
                viewer.queued_bytes(),
                key.wire_bytes,
                "one indivisible key remains retained"
            );
        }
        assert_eq!(
            Arc::strong_count(&key),
            9,
            "all pending peers share the same actual body"
        );
        let retained = rss();
        // A real writer owns its in-flight key until full paint ACK. Slow
        // progress does not renew/quarantine that key or reset its byte debt.
        let mut flights = Vec::new();
        let mut flow = Flow::default();
        flow.set_link_rate(Some(LinkRate {
            bits_per_second: 500_000,
            burst_bytes: 1024,
        }));
        for viewer in &viewers {
            let Delivery::Unit(actual) = viewer.next().await else {
                panic!("first key")
            };
            assert!(Arc::ptr_eq(&actual, &key));
            flights.push(actual);
            viewer.set_ready(false);
        }
        let at = Instant::now();
        flow.sent(1, key.wire_bytes, at);
        for _ in 0..1000 {
            for viewer in &viewers {
                assert!(!viewer.push(&unit(1, false, 100)));
                assert!(viewer.queued_bytes() <= 1024);
            }
            assert!(
                !flow.has_room(0),
                "no partial-progress paint ACK or debt reset"
            );
        }
        assert_eq!(
            Arc::strong_count(&key),
            9,
            "the key remains one shared in-flight body"
        );
        flow.acknowledge(
            1,
            at + Duration::from_secs_f64(key.wire_bytes as f64 * 8.0 / 500_000.0),
        );
        assert!(flow.has_room(0));
        for viewer in &viewers {
            viewer.set_ready(true);
            assert!(matches!(viewer.next().await, Delivery::NewEpoch));
            assert!(viewer.needs_key());
        }
        drop(flights);
        assert_eq!(Arc::strong_count(&key), 1);
        println!(
            "LARGE_VIEW_OWNERSHIP {}",
            serde_json::json!({"payloadBytes":key.data.len(),"peers":8,"beforeRss":before,"retainedRss":retained,"afterRss":rss(),"distinctRetainedKeyBodies":1,"burstBytes":1024})
        );
    }

    /// Captures are skipped while the bytes ahead exceed what the path
    /// delivers in its shortest round trip, never less than one picture.
    #[test]
    fn flow_allows_what_the_path_delivers_in_one_round_trip() {
        let origin = Instant::now();
        let mut flow = Flow::default();
        assert_eq!(flow.budget(), INITIAL_BUDGET, "before any acknowledgement");
        flow.sent(1, 100_000, origin);
        // Painted 50 ms later: 2 MB/s over 50 ms is a 100 KB budget.
        flow.acknowledge(1, origin + Duration::from_millis(50));
        assert_eq!(flow.budget(), 100_000);
        let later = origin + Duration::from_millis(60);
        flow.sent(2, 60_000, later);
        assert!(flow.has_room(40_000));
        assert!(!flow.has_room(40_001));
        flow.sent(3, 150_000, later);
        assert_eq!(
            flow.budget(),
            150_000,
            "at least one picture of the last size"
        );
        assert!(!flow.has_room(0));
        flow.acknowledge(3, later + Duration::from_millis(40));
        assert!(flow.has_room(0));
        assert_eq!(flow.last_sent(), None);
        flow.sent(4, 10, later);
        flow.reset();
        assert_eq!((flow.bytes, flow.last_sent()), (0, None));
    }

    #[test]
    fn an_explicit_path_burst_is_not_inflated_by_the_last_unit_or_an_epoch_reset() {
        let mut flow = Flow::default();
        let rate = LinkRate {
            bits_per_second: 5_000_000,
            burst_bytes: 125_000,
        };
        flow.set_link_rate(Some(rate));
        flow.sent(1, 250_000, Instant::now());
        assert_eq!(flow.budget(), 125_000);
        assert!(!flow.has_room(0));
        flow.reset();
        assert_eq!(flow.budget(), 125_000);
        flow.set_link_rate(None);
        assert_eq!(flow.budget(), INITIAL_BUDGET);
    }
}
