//! What every media track on the viewer channel shares with its viewer: the
//! codec the viewer declared on the upgrade, the producer's offer, and the
//! viewer's subscription by generation.
//!
//! Each state has one record, `{"type":<track>,"state":..,"codec":..}`:
//! - `available` (no generation): the producer can serve the declared codec.
//!   Only after it may the viewer ask for the track.
//! - `unavailable` (the current generation, 0 before any request): the
//!   producer cannot serve it, or could not serve the request of that
//!   generation. It never masquerades as success. Without any codec to
//!   offer, it names none.
//! - `started` (the current generation): a subscription epoch began; its
//!   units carry the fresh `streamId` it names. A producer-side end of an
//!   epoch (overrun, discontinuity, source rebind) begins another epoch under
//!   the same generation with a new `started`.
//! - `stopped` (the current generation): the answer to `enabled:false`.
//!
//! A newer generation retires the previous subscription without a record of
//! its own: the viewer already discards everything older than its request.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::wire::MAX_SAFE_INTEGER;

/// The viewer's newest subscription intent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Demand {
    pub enabled: bool,
    /// Strictly increasing and a safe integer; it survives coalescing and
    /// binds every answer to the request it answers.
    pub generation: u64,
}

impl Demand {
    /// Applies a request. Only a strictly newer safe-integer generation
    /// counts; a repeated or older one is inert.
    pub(super) fn set(&mut self, enabled: bool, generation: u64) -> bool {
        if generation <= self.generation || generation > MAX_SAFE_INTEGER {
            return false;
        }
        *self = Self {
            enabled,
            generation,
        };
        true
    }
}

/// A viewer's subscription request for `track`: `{"type":<track>,
/// "enabled":bool,"generation":n}`. Anything else is not one.
pub(super) fn request(message: &Value) -> Option<(bool, u64)> {
    Some((
        message.get("enabled")?.as_bool()?,
        message.get("generation")?.as_u64()?,
    ))
}

/// The request a connection's reader admits for a track: only a track the
/// viewer declared (`offered` is its offer flag), and enabling only once the
/// producer wrote `available`. Disabling is always admitted.
pub(super) fn admit(offered: Option<&AtomicBool>, message: &Value) -> Option<(bool, u64)> {
    let offered = offered?;
    let (enabled, generation) = request(message)?;
    (!enabled || offered.load(Ordering::Acquire)).then_some((enabled, generation))
}

/// The offer one track holds with its viewer.
pub(super) struct Offer {
    track: &'static str,
    /// The codec offered; none when the producer has none for this viewer.
    codec: Option<&'static str>,
    /// The offer the viewer was last told, if any.
    told: Option<bool>,
    /// Read by the connection's reader: a request to enable is admitted only
    /// once `available` has been written.
    offered: Arc<AtomicBool>,
}

impl Offer {
    pub(super) fn new(track: &'static str, codec: Option<&'static str>) -> Self {
        Self {
            track,
            codec,
            told: None,
            offered: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The flag the reader consults before admitting `enabled:true`.
    pub(super) fn offered(&self) -> Arc<AtomicBool> {
        self.offered.clone()
    }

    fn record(&self, state: &str) -> Value {
        let mut record = json!({"type": self.track, "state": state});
        if let Some(codec) = self.codec {
            record["codec"] = json!(codec);
        }
        record
    }

    /// `available` or `unavailable` when the producer's readiness differs
    /// from what the viewer holds. The reader is armed before the record is
    /// written, so a viewer's prompt reply cannot race the writer. Without a
    /// codec the producer is never ready.
    pub(super) fn declare(&mut self, ready: bool, generation: u64) -> Option<Value> {
        let ready = ready && self.codec.is_some();
        if self.told == Some(ready) {
            return None;
        }
        self.told = Some(ready);
        if ready {
            self.offered.store(true, Ordering::Release);
            return Some(self.record("available"));
        }
        Some(self.unavailable_record(generation))
    }

    /// The answer to a request the producer cannot serve, whatever the viewer
    /// held before.
    pub(super) fn refuse(&mut self, generation: u64) -> Value {
        self.told = Some(false);
        self.unavailable_record(generation)
    }

    fn unavailable_record(&self, generation: u64) -> Value {
        let mut record = self.record("unavailable");
        record["generation"] = json!(generation);
        record
    }

    pub(super) fn stopped(&self, generation: u64) -> Value {
        let mut record = self.record("stopped");
        record["generation"] = json!(generation);
        record
    }

    /// `started` for one epoch, with the track's own configuration fields.
    pub(super) fn started(&self, generation: u64, stream_id: &str, fields: Value) -> Value {
        let mut record = self.record("started");
        record["generation"] = json!(generation);
        record["streamId"] = json!(stream_id);
        if let (Some(record), Value::Object(fields)) = (record.as_object_mut(), fields) {
            record.extend(fields);
        }
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_strictly_newer_safe_generation_changes_the_demand() {
        let mut demand = Demand::default();
        assert!(demand.set(true, 1));
        let first = demand;
        assert!(!demand.set(true, 1), "a repeated generation is inert");
        assert!(demand.set(false, 2));
        assert!(demand.set(true, 3));
        assert_ne!(demand, first, "re-enabling is a distinct request");
        assert!(!demand.set(false, 2), "an older generation is inert");
        assert!(demand.enabled);
        assert!(!demand.set(false, MAX_SAFE_INTEGER + 1));
        assert!(demand.set(false, MAX_SAFE_INTEGER));
    }

    #[test]
    fn a_request_needs_a_boolean_and_an_integer_generation() {
        assert_eq!(
            request(&json!({"type":"video","enabled":true,"generation":4})),
            Some((true, 4))
        );
        for message in [
            json!({"enabled":true}),
            json!({"generation":4}),
            json!({"enabled":"yes","generation":4}),
            json!({"enabled":true,"generation":-1}),
            json!({"enabled":true,"generation":1.5}),
        ] {
            assert_eq!(request(&message), None, "{message}");
        }
    }

    #[test]
    fn the_reader_admits_enabling_only_a_declared_and_offered_track() {
        let enable = json!({"type":"audio","enabled":true,"generation":1});
        let disable = json!({"type":"audio","enabled":false,"generation":2});
        assert_eq!(admit(None, &enable), None, "undeclared");
        assert_eq!(admit(None, &disable), None, "undeclared");
        let offer = Offer::new("audio", Some("opus"));
        let offered = offer.offered();
        assert_eq!(admit(Some(&offered), &enable), None, "not yet offered");
        assert_eq!(admit(Some(&offered), &disable), Some((false, 2)));
        offered.store(true, Ordering::Release);
        assert_eq!(admit(Some(&offered), &enable), Some((true, 1)));
    }

    /// Without any codec for the viewer, the producer says so once, with
    /// the generation it answers, and never claims to be available.
    #[test]
    fn an_offer_without_a_codec_is_unavailable_and_names_none() {
        let mut offer = Offer::new("video", None);
        assert_eq!(
            offer.declare(true, 0),
            Some(json!({"type":"video","state":"unavailable","generation":0}))
        );
        assert_eq!(offer.declare(true, 0), None);
        assert!(!offer.offered().load(Ordering::Acquire));
        assert_eq!(
            offer.stopped(2),
            json!({"type":"video","state":"stopped","generation":2})
        );
    }

    #[test]
    fn the_offer_is_declared_once_per_change_and_arms_the_reader() {
        let mut offer = Offer::new("audio", Some("opus"));
        assert_eq!(
            offer.declare(false, 0),
            Some(json!({"type":"audio","state":"unavailable","codec":"opus","generation":0}))
        );
        assert_eq!(offer.declare(false, 0), None);
        assert!(!offer.offered().load(Ordering::Acquire));
        assert_eq!(
            offer.declare(true, 0),
            Some(json!({"type":"audio","state":"available","codec":"opus"}))
        );
        assert!(offer.offered().load(Ordering::Acquire));
        assert_eq!(offer.declare(true, 3), None);
        assert_eq!(
            offer.refuse(3),
            json!({"type":"audio","state":"unavailable","codec":"opus","generation":3})
        );
        assert_eq!(
            offer.declare(true, 3),
            Some(json!({"type":"audio","state":"available","codec":"opus"}))
        );
        assert_eq!(
            offer.started(3, "id", json!({"sampleRate":48000})),
            json!({"type":"audio","state":"started","codec":"opus","generation":3,
                "streamId":"id","sampleRate":48000})
        );
        assert_eq!(
            offer.stopped(4),
            json!({"type":"audio","state":"stopped","codec":"opus","generation":4})
        );
    }
}
