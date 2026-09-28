//! One negotiated audio track. Device ownership stays with Chrome; this owner
//! follows the viewer's request and retires its own bounded subscription.
//! The records follow the track discipline in `track`.
use crate::native::audio::{AudioCodec, AudioError, AudioPacket, AudioSource, AudioSubscription};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

use super::track::{Demand, Offer};

pub(super) struct AudioTrack {
    codec: AudioCodec,
    offer: Offer,
    source: Option<AudioSource>,
    subscription: Option<AudioSubscription>,
    demand: Demand,
    /// The current subscription's `started` record has been emitted.
    started: bool,
}

impl AudioTrack {
    pub(super) fn new(codec: AudioCodec) -> Self {
        Self {
            codec,
            offer: Offer::new("audio", codec.token()),
            source: None,
            subscription: None,
            demand: Demand::default(),
            started: false,
        }
    }

    /// The flag the reader consults before admitting `enabled:true`.
    pub(super) fn offered(&self) -> Arc<AtomicBool> {
        self.offer.offered()
    }

    fn ready(&self) -> bool {
        self.source
            .as_ref()
            .is_some_and(|source| source.observe().ready)
    }

    /// Ends the current epoch without a record: whoever ends it says why.
    fn retire(&mut self) {
        self.subscription = None;
        self.started = false;
    }

    /// Declares the offer and, while the viewer's request is enabled, holds a
    /// subscription; one that cannot be taken answers the request unavailable.
    fn serve(&mut self, records: &mut Vec<Value>) {
        let generation = self.demand.generation;
        if !self.ready() {
            self.retire();
            records.extend(self.offer.declare(false, generation));
            return;
        }
        records.extend(self.offer.declare(true, generation));
        if !self.demand.enabled || self.subscription.is_some() {
            return;
        }
        match self
            .source
            .as_ref()
            .map(|source| source.subscribe(self.codec))
        {
            Some(Ok(subscription)) => {
                self.subscription = Some(subscription);
                self.started = false;
            }
            _ => records.push(self.offer.refuse(generation)),
        }
    }

    /// The stream's source was bound, replaced or rebound (a coalesced
    /// removal included): the old epoch ends, and the viewer's standing
    /// request is served from the new source under its own generation.
    pub(super) fn bind(&mut self, source: Option<AudioSource>) -> Vec<Value> {
        self.retire();
        self.source = source;
        let mut records = Vec::new();
        self.serve(&mut records);
        records
    }

    /// A newer request. It retires the previous epoch and is answered
    /// `stopped` when it disables, `unavailable` when it cannot be served, and
    /// otherwise by `started` before its first packet.
    pub(super) fn request(&mut self, demand: Demand) -> Vec<Value> {
        let mut records = Vec::new();
        if demand == self.demand {
            return records;
        }
        self.demand = demand;
        self.retire();
        if !demand.enabled {
            records.push(self.offer.stopped(demand.generation));
        } else if self.ready() {
            self.serve(&mut records);
        } else {
            records.push(self.offer.refuse(demand.generation));
        }
        records
    }

    /// The source ended the subscription. A failed or retired source makes
    /// the request unavailable; an overrun or discontinuity begins a new
    /// epoch from live samples under the same generation, never a backlog.
    pub(super) fn interrupted(&mut self, reason: AudioError) -> Vec<Value> {
        self.retire();
        let mut records = Vec::new();
        match reason {
            AudioError::Unavailable | AudioError::Retired => {
                records.push(self.offer.refuse(self.demand.generation));
            }
            AudioError::Overrun | AudioError::Discontinuity => self.serve(&mut records),
        }
        records
    }

    pub(super) async fn next(&mut self) -> Result<AudioPacket, AudioError> {
        match self.subscription.as_mut() {
            Some(subscription) => subscription.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Whether a packet this track produced may still be written: the
    /// viewer's request has not moved on, and its epoch was not retired
    /// while the packet waited.
    pub(super) fn current(&self, demand: Demand) -> bool {
        self.demand == demand
            && demand.enabled
            && self
                .subscription
                .as_ref()
                .is_some_and(|subscription| subscription.is_live())
    }

    /// The records and message one packet becomes: `started` precedes its
    /// epoch's first packet. A packet the wire refuses ends the epoch as
    /// unavailable instead.
    pub(super) fn deliver(&mut self, packet: AudioPacket) -> (Vec<Value>, Option<Message>) {
        let bytes = (packet.format.codec == self.codec)
            .then(|| super::wire::binary_audio(&packet))
            .flatten();
        let Some(bytes) = bytes else {
            return (self.interrupted(AudioError::Unavailable), None);
        };
        let mut records = Vec::new();
        if !self.started {
            self.started = true;
            let format = &packet.format;
            records.push(self.offer.started(
                self.demand.generation,
                &format.stream_id,
                json!({"sampleRate": format.sample_rate, "channels": format.channels,
                    "frameSamples": format.frame_samples, "primingSamples": format.priming_samples}),
            ));
        }
        (records, Some(Message::Binary(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn demand(enabled: bool, generation: u64) -> Demand {
        let mut demand = Demand::default();
        assert!(demand.set(enabled, generation));
        demand
    }

    #[test]
    fn a_missing_device_is_truthful_and_every_request_is_answered_once() {
        let mut track = AudioTrack::new(AudioCodec::Opus);
        assert_eq!(
            track.bind(None),
            vec![json!({"type":"audio","codec":"opus","state":"unavailable","generation":0})]
        );
        assert!(!track.offered().load(Ordering::Acquire));
        assert!(track.bind(None).is_empty(), "the viewer already holds it");
        assert_eq!(
            track.request(demand(true, 1)),
            vec![json!({"type":"audio","codec":"opus","state":"unavailable","generation":1})],
            "an unservable request is answered, not ignored"
        );
        assert_eq!(
            track.request(demand(false, 2)),
            vec![json!({"type":"audio","codec":"opus","state":"stopped","generation":2})]
        );
        assert!(track.request(demand(false, 2)).is_empty(), "inert repeat");
    }

    #[test]
    fn a_failed_source_answers_unavailable_and_an_epoch_end_answers_nothing_yet() {
        let mut track = AudioTrack::new(AudioCodec::PcmS16le);
        track.demand = demand(true, 5);
        assert_eq!(
            track.interrupted(AudioError::Retired),
            vec![json!({"type":"audio","codec":"pcm-s16le","state":"unavailable","generation":5})]
        );
        // Without a device an overrun cannot restart; the viewer already
        // holds unavailable, so nothing repeats and nothing claims stopped.
        assert!(track.interrupted(AudioError::Overrun).is_empty());
    }
}
