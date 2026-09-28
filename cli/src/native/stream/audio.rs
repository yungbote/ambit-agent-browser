//! One negotiated audio track. Device ownership stays with Chrome; this owner
//! only follows the viewer's request and retires its own bounded subscription.
use crate::native::audio::{AudioCodec, AudioError, AudioPacket, AudioSource, AudioSubscription};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Demand {
    pub enabled: bool,
    // The caller's generation survives coalescing and binds replies to its current request.
    pub generation: u64,
}
impl Demand {
    pub(super) fn set(&mut self, enabled: bool, generation: u64) -> bool {
        if generation <= self.generation || generation > super::wire::MAX_SAFE_INTEGER {
            return false;
        }
        self.enabled = enabled;
        self.generation = generation;
        true
    }
}

pub(super) struct AudioTrack {
    codec: Option<AudioCodec>,
    source: Option<AudioSource>,
    subscription: Option<AudioSubscription>,
    demand: Demand,
    available: Option<bool>,
    started: bool,
    pub offered: Arc<AtomicBool>,
}

impl AudioTrack {
    pub(super) fn new(codec: Option<AudioCodec>) -> Self {
        Self {
            codec,
            source: None,
            subscription: None,
            demand: Demand::default(),
            available: None,
            started: false,
            offered: Arc::new(AtomicBool::new(false)),
        }
    }
    fn status(&self, state: &str) -> Value {
        let mut value = json!({"type":"audio", "state":state, "codec":self.codec.unwrap()});
        if state != "available" {
            value["generation"] = json!(self.demand.generation);
        }
        value
    }
    fn stop(&mut self, messages: &mut Vec<Value>) {
        if self.subscription.take().is_some() {
            messages.push(self.status("stopped"));
        }
        self.started = false;
    }
    fn declare(&mut self, available: bool, messages: &mut Vec<Value>) {
        if self.available != Some(available) {
            self.available = Some(available);
            messages.push(self.status(if available {
                "available"
            } else {
                "unavailable"
            }));
        }
    }
    fn start(&mut self, messages: &mut Vec<Value>) {
        if !self.demand.enabled || self.available != Some(true) || self.subscription.is_some() {
            return;
        }
        match self.source.as_ref().unwrap().subscribe(self.codec.unwrap()) {
            Ok(subscription) => self.subscription = Some(subscription),
            Err(_) => self.declare(false, messages),
        }
    }
    pub(super) fn bind(&mut self, source: Option<AudioSource>) -> Vec<Value> {
        if self.codec.is_none() {
            return Vec::new();
        }
        // Even None -> the same device may coalesce in the watch; rebinding is a barrier.
        let mut messages = Vec::new();
        self.stop(&mut messages);
        self.source = source;
        messages.extend(self.synchronize(self.demand));
        messages
    }
    pub(super) fn synchronize(&mut self, demand: Demand) -> Vec<Value> {
        if self.codec.is_none() {
            return Vec::new();
        }
        let mut messages = Vec::new();
        let requested = self.demand != demand;
        self.demand = demand;
        let was_subscribed = self.subscription.is_some();
        if requested {
            self.stop(&mut messages);
        }
        if requested && !demand.enabled && !was_subscribed {
            messages.push(self.status("stopped"));
        }
        let ready = self
            .source
            .as_ref()
            .is_some_and(|source| source.observe().ready);
        if !ready {
            self.stop(&mut messages);
        }
        if requested && !ready {
            self.available = None;
        }
        self.declare(ready, &mut messages);
        self.start(&mut messages);
        messages
    }
    /// Arm the reader before awaiting the offer write, so a legitimate reply cannot
    /// race the writer's resumption. Only a successful write lets the writer process
    /// demand and send subscription packets; a failed write closes the connection.
    pub(super) fn offering(&self, message: &Value) {
        if message["state"] == "available" {
            self.offered.store(true, Ordering::Release);
        }
    }
    pub(super) async fn next(&mut self) -> Result<AudioPacket, AudioError> {
        match self.subscription.as_mut() {
            Some(subscription) => subscription.recv().await,
            None => std::future::pending().await,
        }
    }
    pub(super) fn accepts(&self, demand: Demand) -> bool {
        self.demand == demand
            && demand.enabled
            && self
                .subscription
                .as_ref()
                .is_some_and(|subscription| subscription.is_live())
    }
    pub(super) fn interrupted(&mut self, reason: AudioError) -> Vec<Value> {
        let mut messages = Vec::new();
        self.stop(&mut messages);
        if matches!(reason, AudioError::Unavailable | AudioError::Retired) {
            self.declare(false, &mut messages);
        } else {
            // Discontinuity/overrun starts from live samples with a new UUID, never a backlog.
            self.start(&mut messages);
        }
        messages
    }
    pub(super) fn packet(&mut self, packet: AudioPacket) -> Option<(Option<Value>, Message)> {
        if Some(packet.format.codec) != self.codec {
            return None;
        }
        let bytes = super::wire::binary_audio(&packet)?;
        let configuration = if self.started {
            None
        } else {
            self.started = true;
            let format = &packet.format;
            Some(
                json!({"type":"audio", "state":"started", "codec":format.codec, "generation":self.demand.generation,
                "streamId":format.stream_id, "sampleRate":format.sample_rate, "channels":format.channels,
                "frameSamples":format.frame_samples, "primingSamples":format.priming_samples}),
            )
        };
        Some((configuration, Message::Binary(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rapid_mute_and_reenable_remain_a_distinct_demand_after_coalescing() {
        let mut demand = Demand::default();
        assert!(demand.set(true, 1));
        let first = demand;
        assert!(!demand.set(true, 1));
        assert!(demand.set(false, 2));
        assert!(demand.set(true, 3));
        assert!(demand.enabled);
        assert_ne!(demand, first);
        assert!(!demand.set(false, 2));
        assert!(demand.enabled);
        assert!(demand.set(true, 4));
        assert!(!demand.set(false, super::super::wire::MAX_SAFE_INTEGER + 1));
    }
    #[test]
    fn legacy_peers_receive_no_audio_metadata_and_missing_devices_are_truthful() {
        assert!(AudioTrack::new(None).bind(None).is_empty());
        let mut track = AudioTrack::new(Some(AudioCodec::Opus));
        assert_eq!(
            track.bind(None),
            vec![json!({"type":"audio","codec":"opus","state":"unavailable","generation":0})]
        );
        assert!(!track.offered.load(Ordering::Acquire));
        assert!(track.synchronize(Demand::default()).is_empty());
    }
}
