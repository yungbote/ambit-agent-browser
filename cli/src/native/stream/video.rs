//! The video track on the viewer channel (the video track contract): a
//! viewer that declared `video=<tokens>` on a binary upgrade is offered one
//! codec, subscribes by generation, and is sent video units instead of JPEG
//! frames while its subscription is served. Records follow the discipline in
//! `track`; each epoch has its own stream id and sequence from 1, and begins
//! with a key unit.
//!
//! Pictures never carry the native pointer, so video is offered only to a
//! viewer that draws the pointer itself (`cursor=viewer`); any other keeps
//! frames, which composite it. Pictures come from the display helper's
//! shared memory, which exists on Linux only; elsewhere no encoder is offered.

// The producer's half exists on Linux only.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(all(test, target_os = "linux"))]
mod cpu_bench;
mod policy;
#[cfg(target_os = "linux")]
mod producer;
#[cfg(target_os = "linux")]
pub(crate) mod snapshot;
mod subscription;
#[cfg(all(test, target_os = "linux"))]
pub(super) mod testing;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::{watch, RwLock};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::cursor_identity::CursorIdentities;
use super::track::{self, Demand, Offer};
use super::StreamMedia;
use crate::native::display::DisplayClient;
use crate::native::video::{Declared, VideoCodec, VideoError};
use policy::LinkRate;
#[cfg(test)]
pub(super) use policy::Quality;
#[cfg(all(test, target_os = "linux"))]
pub(super) use producer::measured;
use producer::{Producer, Subscription};
use subscription::Flow;
pub(super) use subscription::{Delivery, Unit};

/// The session's video: the display pictures come from, and the producer
/// that captures it while any viewer is subscribed (one per display, shared
/// by every video viewer).
pub(super) struct VideoHub {
    display: Arc<RwLock<Option<Arc<DisplayClient>>>>,
    /// Rings when `display` holds another display.
    display_changed: watch::Receiver<()>,
    media: Arc<StreamMedia>,
    cursors: CursorIdentities,
    producer: std::sync::Mutex<Weak<Producer>>,
}

impl VideoHub {
    pub(super) fn new(
        display: Arc<RwLock<Option<Arc<DisplayClient>>>>,
        display_changed: watch::Receiver<()>,
        media: Arc<StreamMedia>,
        cursors: CursorIdentities,
    ) -> Self {
        Self {
            display,
            display_changed,
            media,
            cursors,
            producer: std::sync::Mutex::new(Weak::new()),
        }
    }

    /// The stream's display now.
    pub(super) async fn display(&self) -> Option<Arc<DisplayClient>> {
        self.display.read().await.clone()
    }

    /// Rings whenever the stream's display is replaced.
    pub(super) fn display_changes(&self) -> watch::Receiver<()> {
        self.display_changed.clone()
    }

    /// A subscription to `display`'s pictures in `codec`, from the producer
    /// that already serves it or a new one.
    #[cfg(target_os = "linux")]
    fn subscribe(
        &self,
        display: &Arc<DisplayClient>,
        codec: VideoCodec,
        rate: u32,
    ) -> Result<Subscription, VideoError> {
        let mut current = self
            .producer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let producer = match current
            .upgrade()
            .filter(|producer| producer.serves(display))
        {
            Some(producer) => producer,
            None => {
                let producer = Producer::start(producer::Source {
                    display: display.clone(),
                    media: self.media.clone(),
                    cursors: self.cursors.clone(),
                    runtime: tokio::runtime::Handle::current(),
                })?;
                *current = Arc::downgrade(&producer);
                producer
            }
        };
        producer.subscribe(codec, rate)
    }

    #[cfg(not(target_os = "linux"))]
    fn subscribe(
        &self,
        _display: &Arc<DisplayClient>,
        _codec: VideoCodec,
        _rate: u32,
    ) -> Result<Subscription, VideoError> {
        Err(VideoError::Unavailable(
            "pictures need the Linux display helper".into(),
        ))
    }
}

/// No pictures off Linux: a subscription cannot exist there.
#[cfg(not(target_os = "linux"))]
mod producer {
    use super::subscription::Delivery;

    pub(crate) enum Subscription {}

    pub(crate) enum Producer {}

    impl Subscription {
        pub(crate) async fn next(&self) -> Delivery {
            match *self {}
        }
        pub(crate) fn keyframe(&self) {
            match *self {}
        }
        pub(crate) fn set_ready(&self, _: bool) {
            match *self {}
        }
        pub(crate) fn set_rate(&self, _: u32) {
            match *self {}
        }
        pub(crate) fn set_link_rate(&self, _: super::LinkRate) {
            match *self {}
        }
        pub(crate) fn queued_bytes(&self) -> usize {
            match *self {}
        }
    }
}

/// What the viewer sends about its video, for the connection's writer:
/// the newest cumulative acknowledgement and how many key units it asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Feedback {
    pub ack: Option<(Uuid, u64)>,
    pub keyframes: u64,
    pub rate: Option<(u64, LinkRate)>,
}

/// The reader's half of a video track.
pub(super) struct VideoInbox {
    offered: Arc<AtomicBool>,
    feedback: watch::Sender<Feedback>,
}

impl VideoInbox {
    /// A `rate` budget is feedback of the current enabled video generation,
    /// never input authority. Without one the producer keeps fixed quality.
    /// A `video` message: a subscription request is returned for the
    /// connection's config (enabling only once offered); a request for a key
    /// unit counts only for the current enabled generation.
    pub(super) fn receive(&self, message: &Value, demand: Demand) -> Option<(bool, u64)> {
        if message["type"] == "rate" {
            if let Some(rate) = LinkRate::read(message, demand.generation, demand.enabled) {
                self.feedback
                    .send_modify(|feedback| feedback.rate = Some((demand.generation, rate)));
            }
            return None;
        }
        if let Some(request) = track::admit(Some(&self.offered), message) {
            return Some(request);
        }
        let asked = message.get("keyframe").and_then(Value::as_bool) == Some(true);
        let generation = message.get("generation").and_then(Value::as_u64);
        if asked && demand.enabled && generation == Some(demand.generation) {
            self.feedback
                .send_modify(|feedback| feedback.keyframes += 1);
        }
        None
    }

    /// `{"type":"ack","track":"video","streamId":..,"seq":n}`: the viewer
    /// painted that unit and every one before it.
    pub(super) fn acknowledge(&self, message: &Value) {
        let stream = message
            .get("streamId")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok());
        let seq = message.get("seq").and_then(Value::as_u64);
        if let (Some(stream), Some(seq)) = (stream, seq) {
            self.feedback
                .send_modify(|feedback| feedback.ack = Some((stream, seq)));
        }
    }
}

/// One epoch of a subscription: its stream id and the last sequence written.
struct Epoch {
    id: Uuid,
    /// `id` as units and records carry it.
    stream_id: String,
    seq: u64,
}

/// One connection's video track: the writer's half.
pub(super) struct VideoTrack {
    /// The upgrade declared the dimensional source allowance; absent is legacy.
    coded_capacity: bool,
    hub: Arc<VideoHub>,
    /// The codec offered; none when this viewer cannot be served video.
    codec: Option<VideoCodec>,
    offer: Offer,
    /// The viewer's requested rate (0: its display's).
    rate: u32,
    display: Option<Arc<DisplayClient>>,
    demand: Demand,
    subscription: Option<Subscription>,
    epoch: Option<Epoch>,
    flow: Flow,
    /// The key unit requests already passed on.
    keyframes: u64,
    link_rate: Option<LinkRate>,
}

impl VideoTrack {
    /// The track a viewer declared, and its reader's half.
    pub(super) fn new(
        hub: Arc<VideoHub>,
        declared: Declared,
        draws_pointer: bool,
        rate: u32,
    ) -> (Self, VideoInbox, watch::Receiver<Feedback>) {
        let codec = draws_pointer.then(|| declared.negotiate()).flatten();
        let offer = Offer::new("video", codec.map(VideoCodec::token));
        let (feedback, feedback_rx) = watch::channel(Feedback::default());
        let inbox = VideoInbox {
            offered: offer.offered(),
            feedback,
        };
        let track = Self {
            coded_capacity: false,
            hub,
            codec,
            offer,
            rate,
            display: None,
            demand: Demand::default(),
            subscription: None,
            epoch: None,
            flow: Flow::default(),
            keyframes: 0,
            link_rate: None,
        };
        (track, inbox, feedback_rx)
    }

    /// Whether this viewer is sent video units instead of frames: its
    /// subscription is served.
    pub(super) fn serving(&self) -> bool {
        self.subscription.is_some()
    }

    /// The codec and display this viewer can be served from now, if any.
    fn source(&self) -> Option<(VideoCodec, Arc<DisplayClient>)> {
        self.codec
            .zip(self.display.clone())
            .filter(|(_, display)| pictures_served(display))
    }

    /// Ends the current epoch without a record (whoever ends it says why),
    /// and hands back its subscription: dropped after a successor is taken,
    /// the producer and its encoder stay up in between.
    fn retire(&mut self) -> Option<Subscription> {
        self.epoch = None;
        self.flow.reset();
        self.subscription.take()
    }

    /// Declares the offer and, while the viewer's request is enabled, holds
    /// a subscription; one that cannot be taken answers the request
    /// unavailable.
    fn serve(&mut self, records: &mut Vec<Value>) {
        let generation = self.demand.generation;
        let Some((codec, display)) = self.source() else {
            drop(self.retire());
            records.extend(self.offer.declare(false, generation));
            return;
        };
        records.extend(self.offer.declare(true, generation));
        if !self.demand.enabled || self.subscription.is_some() {
            return;
        }
        match self.hub.subscribe(&display, codec, self.rate) {
            Ok(subscription) => {
                if let Some(rate) = self.link_rate {
                    subscription.set_link_rate(rate);
                }
                self.subscription = Some(subscription);
                self.epoch = None;
                self.flow.reset();
            }
            Err(_) => records.push(self.offer.refuse(generation)),
        }
    }

    /// The stream's display was bound or replaced: the previous epoch ends,
    /// and the viewer's standing request is served from the new display
    /// under its own generation.
    pub(super) fn bind(&mut self, display: Option<Arc<DisplayClient>>) -> Vec<Value> {
        drop(self.retire());
        self.display = display;
        let mut records = Vec::new();
        self.serve(&mut records);
        records
    }

    /// A newer request. It retires the previous epoch and is answered
    /// `stopped` when it disables, `unavailable` when it cannot be served,
    /// and otherwise by `started` before its first unit.
    pub(super) fn request(&mut self, demand: Demand) -> Vec<Value> {
        let mut records = Vec::new();
        if demand == self.demand {
            return records;
        }
        self.demand = demand;
        self.link_rate = None;
        self.flow.set_link_rate(None);
        let previous = self.retire();
        if !demand.enabled {
            records.push(self.offer.stopped(demand.generation));
        } else if self.source().is_some() {
            self.serve(&mut records);
        } else {
            records.push(self.offer.refuse(demand.generation));
        }
        drop(previous);
        records
    }

    /// The viewer's rate changed (0: its display's).
    pub(super) fn set_rate(&mut self, rate: u32) {
        self.rate = rate;
        if let Some(subscription) = &self.subscription {
            subscription.set_rate(rate);
        }
    }

    pub(super) async fn next(&self) -> Delivery {
        match &self.subscription {
            // Capture admission includes queued bytes; delivery admission
            // considers in-flight bytes only. It permits one indivisible
            // picture from an empty flight without resetting its paint debt.
            Some(subscription) if self.flow.has_room(0) => subscription.next().await,
            _ => std::future::pending().await,
        }
    }

    /// Whether a delivery this track produced may still be written: the
    /// viewer's request has not moved on since.
    pub(super) fn current(&self, demand: Demand) -> bool {
        self.demand == demand && demand.enabled && self.subscription.is_some()
    }

    /// The records and message one delivery becomes. `started` precedes an
    /// epoch's first unit, which is a key unit. A unit the wire refuses, or
    /// a subscription the producer ended, ends the request as unavailable. A
    /// unit that is not `fresh` (the viewer's request or the display moved
    /// on, which the writer applies next) is not written.
    pub(super) fn deliver(
        &mut self,
        delivery: Delivery,
        fresh: bool,
    ) -> (Vec<Value>, Option<Message>) {
        let unit = match delivery {
            Delivery::Unit(_) if !fresh => return (Vec::new(), None),
            Delivery::Unit(unit) => unit,
            Delivery::NewEpoch => {
                self.epoch = None;
                self.flow.reset();
                self.update_ready();
                if let Some(subscription) = &self.subscription {
                    subscription.keyframe();
                }
                return (Vec::new(), None);
            }
            Delivery::Ended(_) => return self.refused(),
        };
        if !self.coded_capacity && unit.data.len() > super::wire::LEGACY_VIDEO_BYTES {
            // Before started or any body: preserve the established track-only
            // unsupported transition, leaving the peer's audio/input intact.
            return self.refused();
        }
        let mut records = Vec::new();
        if self.epoch.is_none() {
            let (Some(codec_string), true) = (unit.codec_string.as_deref(), unit.key) else {
                return self.refused();
            };
            let id = Uuid::new_v4();
            let stream_id = id.to_string();
            records.push(self.offer.started(
                self.demand.generation,
                &stream_id,
                json!({ "codecString": codec_string }),
            ));
            self.epoch = Some(Epoch {
                id,
                stream_id,
                seq: 0,
            });
        }
        let (Some(codec), Some(epoch)) = (self.codec, self.epoch.as_mut()) else {
            return self.refused();
        };
        epoch.seq += 1;
        let Some(message) = super::wire::binary_video(&unit, codec, &epoch.stream_id, epoch.seq)
        else {
            return self.refused();
        };
        self.flow.sent(epoch.seq, message.len(), Instant::now());
        self.update_ready();
        (records, Some(Message::Binary(message)))
    }

    /// The producer ended the subscription, or the contract forbids what it
    /// produced: the request ends unavailable.
    fn refused(&mut self) -> (Vec<Value>, Option<Message>) {
        drop(self.retire());
        (vec![self.offer.refuse(self.demand.generation)], None)
    }

    /// Resource capacity declared by this connection's immutable upgrade.
    pub(super) fn declare_coded_capacity(&mut self, supported: bool) {
        self.coded_capacity = supported;
    }

    /// Applies the viewer's newest feedback: its cumulative acknowledgement
    /// of the current epoch, and any key unit it asked for.
    pub(super) fn feedback(&mut self, feedback: Feedback) {
        if let Some((generation, rate)) = feedback.rate {
            if self.demand.enabled
                && self.demand.generation == generation
                && self.link_rate != Some(rate)
            {
                self.link_rate = Some(rate);
                self.flow.set_link_rate(Some(rate));
                if let Some(subscription) = &self.subscription {
                    subscription.set_link_rate(rate);
                }
                self.update_ready();
            }
        }
        if let (Some((stream, seq)), Some(epoch)) = (feedback.ack, self.epoch.as_ref()) {
            // An acknowledgement of a retired epoch, or beyond what was
            // written, releases nothing.
            if stream == epoch.id && seq <= epoch.seq {
                self.flow.acknowledge(seq, Instant::now());
                self.update_ready();
            }
        }
        if feedback.keyframes > self.keyframes {
            self.keyframes = feedback.keyframes;
            if let Some(subscription) = &self.subscription {
                subscription.keyframe();
            }
        }
    }

    /// Tells the producer whether this viewer's path has room.
    fn update_ready(&self) {
        if let Some(subscription) = &self.subscription {
            subscription.set_ready(self.flow.has_room(subscription.queued_bytes()));
        }
    }
}

#[cfg(target_os = "linux")]
fn pictures_served(display: &DisplayClient) -> bool {
    display.pictures().is_some()
}

#[cfg(not(target_os = "linux"))]
fn pictures_served(_: &DisplayClient) -> bool {
    false
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::testing::{FakeScreen, RED};
    use super::*;
    use crate::native::video::encodes;
    use std::time::Duration;
    use tokio::sync::broadcast;

    #[test]
    fn legacy_peer_refuses_large_picture_before_started_or_body_without_refusing_coded_peer() {
        let make_track = |coded| {
            let (mut track, _, _) = VideoTrack::new(hub(None), declared("av1-444"), true, 0);
            track.codec = Some(VideoCodec::Av1Full);
            track.offer = Offer::new("video", Some("av1-444"));
            track.demand = demand(true, 7);
            track.declare_coded_capacity(coded);
            track
        };
        let unit = Arc::new(Unit {
            data: vec![1; super::super::wire::LEGACY_VIDEO_BYTES + 1],
            wire_bytes: 0,
            key: true,
            ts: 1,
            coded: (4096, 4096),
            visible: crate::native::display::Rect {
                x: 0,
                y: 0,
                width: 4096,
                height: 4096,
            },
            surface: crate::native::display::Surface::new(4096, 4096),
            input_seq: None,
            quality: Quality::Motion,
            codec_string: Some("av01.1.16M.08".into()),
        });
        let (records, body) = make_track(false).deliver(Delivery::Unit(unit.clone()), true);
        assert!(body.is_none());
        assert_eq!(
            records,
            [json!({"type":"video","state":"unavailable","codec":"av1-444","generation":7})]
        );
        let (records, body) = make_track(true).deliver(Delivery::Unit(unit), true);
        assert_eq!(records[0]["state"], "started");
        assert!(body.is_some());
    }

    fn hub(display: Option<Arc<DisplayClient>>) -> Arc<VideoHub> {
        let (frame_tx, _) = broadcast::channel(16);
        let media = Arc::new(StreamMedia::new(Default::default()));
        let cursors = CursorIdentities::new(
            frame_tx,
            media.clone(),
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(None)),
        );
        Arc::new(VideoHub::new(
            Arc::new(RwLock::new(display)),
            watch::channel(()).1,
            media,
            cursors,
        ))
    }

    fn demand(enabled: bool, generation: u64) -> Demand {
        let mut demand = Demand::default();
        assert!(demand.set(enabled, generation));
        demand
    }

    fn declared(value: &str) -> Declared {
        Declared::parse(value).unwrap()
    }

    fn header(message: &Message) -> Value {
        let Message::Binary(bytes) = message else {
            panic!("a unit is binary")
        };
        let end = 4 + u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        serde_json::from_slice(&bytes[4..end]).unwrap()
    }

    /// The next unit the track writes, with the records written before it.
    async fn next_unit(track: &mut VideoTrack) -> (Vec<Value>, Value) {
        let mut records = Vec::new();
        loop {
            let delivery = tokio::time::timeout(Duration::from_secs(5), track.next())
                .await
                .expect("a delivery");
            let (mut written, message) = track.deliver(delivery, true);
            records.append(&mut written);
            if let Some(message) = message {
                return (records, header(&message));
            }
        }
    }

    /// Without libaom there is no encoder to prove anything with.
    fn encoder() -> bool {
        let present = encodes(VideoCodec::Av1Full);
        if !present {
            eprintln!("libaom.so.3 is absent: no encoder to serve video with");
        }
        present
    }

    /// A viewer the producer has no codec for (none it can encode, or a
    /// viewer that needs the pointer drawn in) is told so once, names no
    /// codec, and every request it makes is still answered.
    #[tokio::test]
    async fn a_viewer_without_a_codec_for_it_is_told_once_and_every_request_answered() {
        for (value, draws_pointer) in [("vp9-444,vp9", true), ("av1-444,av1", false)] {
            let (mut track, _inbox, _feedback) =
                VideoTrack::new(hub(None), declared(value), draws_pointer, 0);
            assert_eq!(
                track.bind(None),
                vec![json!({"type":"video","state":"unavailable","generation":0})],
                "{value}"
            );
            assert!(track.bind(None).is_empty(), "already told");
            assert_eq!(
                track.request(demand(true, 1)),
                vec![json!({"type":"video","state":"unavailable","generation":1})]
            );
            assert_eq!(
                track.request(demand(false, 2)),
                vec![json!({"type":"video","state":"stopped","generation":2})]
            );
            assert!(!track.serving());
        }
    }

    /// Each epoch of a served request begins with `started` (its generation,
    /// a fresh stream id and the stream's codec string) and a key unit at
    /// sequence 1; its units then count up by one. A newer generation is a
    /// new epoch; disabling is answered `stopped` and ends the video.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_epoch_begins_with_started_and_a_key_unit_at_sequence_one() {
        if !encoder() {
            return;
        }
        let crate::native::display::TestPictures {
            display, helper, ..
        } = DisplayClient::test_pictures();
        let screen = FakeScreen::new(helper);
        let (mut track, _inbox, _feedback) =
            VideoTrack::new(hub(Some(display.clone())), declared("av1,av1-444"), true, 0);
        assert_eq!(
            track.bind(Some(display.clone())),
            vec![json!({"type":"video","state":"available","codec":"av1-444"})],
            "full chroma first, whatever the viewer's order"
        );
        assert!(
            track.request(demand(true, 1)).is_empty(),
            "answered by started"
        );
        assert!(track.serving());
        let (records, first) = next_unit(&mut track).await;
        assert_eq!(
            records,
            vec![
                json!({"type":"video","state":"started","codec":"av1-444","generation":1,
                "streamId":first["streamId"],"codecString":first["codecString"]})
            ]
        );
        assert!(Uuid::parse_str(first["streamId"].as_str().unwrap()).is_ok());
        assert_eq!(
            (first["seq"].as_u64(), first["key"].as_bool()),
            (Some(1), Some(true))
        );
        screen.paint(10, 20, RED);
        let (records, second) = next_unit(&mut track).await;
        assert!(records.is_empty());
        assert_eq!(second["seq"], 2);
        assert_eq!(second["streamId"], first["streamId"]);
        assert!(second.get("codecString").is_none());

        assert!(track.request(demand(true, 2)).is_empty());
        let (records, restarted) = next_unit(&mut track).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["generation"], 2);
        assert_ne!(
            restarted["streamId"], first["streamId"],
            "a fresh stream id"
        );
        assert_eq!(
            (restarted["seq"].as_u64(), restarted["key"].as_bool()),
            (Some(1), Some(true))
        );

        assert_eq!(
            track.request(demand(false, 3)),
            vec![json!({"type":"video","state":"stopped","codec":"av1-444","generation":3})]
        );
        assert!(!track.serving());
    }

    /// Acknowledgements count only for the epoch they name and for what was
    /// written; a request for a key unit, counted by the reader, makes the
    /// next unit one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feedback_counts_only_for_its_epoch_and_a_key_request_is_answered() {
        if !encoder() {
            return;
        }
        let crate::native::display::TestPictures {
            display, helper, ..
        } = DisplayClient::test_pictures();
        let screen = FakeScreen::new(helper);
        let (mut track, inbox, feedback) =
            VideoTrack::new(hub(Some(display.clone())), declared("av1-444"), true, 0);
        track.bind(Some(display.clone()));
        track.request(demand(true, 4));
        let (_, first) = next_unit(&mut track).await;
        let stream = Uuid::parse_str(first["streamId"].as_str().unwrap()).unwrap();
        // The reader counts a key request only for the current generation.
        assert_eq!(
            inbox.receive(
                &json!({"type":"video","keyframe":true,"generation":3}),
                track.demand
            ),
            None
        );
        assert_eq!(
            feedback.borrow().keyframes,
            0,
            "an older generation's request"
        );
        inbox.receive(
            &json!({"type":"video","keyframe":true,"generation":4}),
            track.demand,
        );
        inbox.acknowledge(
            &json!({"type":"ack","track":"video","streamId":stream.to_string(),"seq":1}),
        );
        let sent = *feedback.borrow();
        assert_eq!(
            sent,
            Feedback {
                ack: Some((stream, 1)),
                keyframes: 1,
                rate: None,
            }
        );
        assert_eq!(track.flow.last_sent(), Some(1));
        track.feedback(Feedback {
            ack: Some((Uuid::new_v4(), 1)),
            keyframes: 0,
            rate: None,
        });
        assert_eq!(
            track.flow.last_sent(),
            Some(1),
            "another epoch releases nothing"
        );
        track.feedback(Feedback {
            ack: Some((stream, 9)),
            keyframes: 0,
            rate: None,
        });
        assert_eq!(
            track.flow.last_sent(),
            Some(1),
            "nothing beyond what was written"
        );
        track.feedback(sent);
        assert_eq!(track.flow.last_sent(), None, "painted: nothing in flight");
        // A refinement encoded before the request may come first.
        let asked = loop {
            let (_, unit) = next_unit(&mut track).await;
            if unit["key"] == true {
                break unit;
            }
            assert_eq!(unit["quality"], "final");
        };
        assert!(asked["seq"].as_u64() >= Some(2));
        drop(screen);
    }

    #[tokio::test]
    async fn rate_feedback_is_current_generation_only_and_resubscription_restores_legacy_flow() {
        let (mut track, inbox, feedback) = VideoTrack::new(hub(None), declared("av1-444"), true, 0);
        track.demand = demand(true, 7);
        let message =
            json!({"type":"rate","generation":7,"bitsPerSecond":5_000_000,"burstBytes":125_000});
        inbox.receive(&message, track.demand);
        track.feedback(*feedback.borrow());
        assert_eq!(
            track.link_rate,
            Some(LinkRate {
                bits_per_second: 5_000_000,
                burst_bytes: 125_000
            })
        );
        assert_eq!(track.flow.budget(), 125_000);
        track.request(demand(true, 8));
        assert_eq!(track.link_rate, None);
        track.feedback(*feedback.borrow());
        assert_eq!(
            track.link_rate, None,
            "a late feedback of generation seven cannot cap eight"
        );
        assert_eq!(track.flow.budget(), 2 * 1024 * 1024);
    }

    /// A display without pictures cannot serve: its request is answered
    /// unavailable, and a display that serves them later serves the standing
    /// request under its own generation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_request_is_served_when_a_display_with_pictures_is_bound() {
        if !encoder() {
            return;
        }
        let (plain, _control, _frames) = DisplayClient::test_channel();
        let crate::native::display::TestPictures {
            display, helper, ..
        } = DisplayClient::test_pictures();
        let _screen = FakeScreen::new(helper);
        let (mut track, _inbox, _feedback) =
            VideoTrack::new(hub(None), declared("av1-444"), true, 0);
        assert_eq!(
            track.bind(Some(plain)),
            vec![json!({"type":"video","state":"unavailable","codec":"av1-444","generation":0})]
        );
        assert_eq!(
            track.request(demand(true, 1)),
            vec![json!({"type":"video","state":"unavailable","codec":"av1-444","generation":1})]
        );
        let mut hub_display = track.hub.display.write().await;
        *hub_display = Some(display.clone());
        drop(hub_display);
        assert_eq!(
            track.bind(Some(display)),
            vec![json!({"type":"video","state":"available","codec":"av1-444"})]
        );
        let (records, unit) = next_unit(&mut track).await;
        assert_eq!(records[0]["state"], "started");
        assert_eq!(records[0]["generation"], 1);
        assert_eq!(unit["seq"], 1);
    }

    /// Units that are no longer the viewer's are not written, a stream the
    /// producer restarts begins a new epoch at its key unit, and what the
    /// contract forbids ends the request unavailable instead of reaching the
    /// wire.
    #[test]
    fn epochs_restart_at_key_units_and_forbidden_units_never_leave() {
        let (mut track, _inbox, _feedback) =
            VideoTrack::new(hub(None), declared("av1-444"), true, 0);
        track.codec = Some(VideoCodec::Av1Full);
        track.offer = Offer::new("video", Some("av1-444"));
        track.demand = demand(true, 7);
        let unit = |key: bool, ts: u64| {
            Arc::new(Unit {
                data: vec![1; 32],
                wire_bytes: 0,
                key,
                ts,
                coded: (1024, 768),
                visible: crate::native::display::Rect {
                    x: 0,
                    y: 0,
                    width: 640,
                    height: 480,
                },
                surface: crate::native::display::Surface::new(640, 480),
                input_seq: None,
                quality: Quality::Motion,
                codec_string: key.then(|| "av01.1.08M.08".into()),
            })
        };
        let (records, message) = track.deliver(Delivery::Unit(unit(false, 1)), false);
        assert!(
            records.is_empty() && message.is_none(),
            "a stale unit is not written"
        );
        let (records, message) = track.deliver(Delivery::Unit(unit(true, 2)), true);
        let first = header(&message.unwrap());
        assert_eq!(records[0]["streamId"], first["streamId"]);
        let (_, message) = track.deliver(Delivery::Unit(unit(false, 3)), true);
        assert_eq!(header(&message.unwrap())["seq"], 2);
        let (records, message) = track.deliver(Delivery::NewEpoch, true);
        assert!(records.is_empty() && message.is_none());
        let (records, message) = track.deliver(Delivery::Unit(unit(true, 4)), true);
        let restarted = header(&message.unwrap());
        assert_eq!(records[0]["generation"], 7, "the same generation");
        assert_ne!(restarted["streamId"], first["streamId"]);
        assert_eq!(restarted["seq"], 1);
        let (_, message) = track.deliver(Delivery::NewEpoch, true);
        assert!(message.is_none());
        assert_eq!(
            track.deliver(Delivery::Unit(unit(false, 5)), true),
            (
                vec![
                    json!({"type":"video","state":"unavailable","codec":"av1-444","generation":7})
                ],
                None
            ),
            "an epoch never begins at a dependent unit"
        );
        track.demand = demand(true, 8);
        assert_eq!(
            track
                .deliver(Delivery::Ended("encoder failed".into()), true)
                .0,
            vec![json!({"type":"video","state":"unavailable","codec":"av1-444","generation":8})]
        );
    }
}
