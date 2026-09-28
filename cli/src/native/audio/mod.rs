//! Output of one privately launched Chrome session, never an input device.
//! The qualified image opts into native audio bindings. The protocol owner
//! subscribes only while its authenticated viewer wants sound; no relay lives here.

#[cfg(all(target_os = "linux", feature = "browser-audio"))]
mod pulse;
#[cfg(all(target_os = "linux", feature = "browser-audio"))]
mod server;
#[cfg(all(target_os = "linux", feature = "browser-audio"))]
pub(crate) use server::RetainedAudio;
#[cfg(all(target_os = "linux", feature = "browser-audio"))]
mod encoder;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const CHANNELS: u8 = 2;
pub(crate) const FRAME_SAMPLES: usize = 480;
pub(crate) const FRAME_BYTES: usize = FRAME_SAMPLES * CHANNELS as usize * 2;
/// Six 10 ms units. On overflow the subscription ends instead of replaying old sound.
// The capture server is the queue's only producer; without it only the
// consumer side is compiled.
#[cfg_attr(
    not(all(target_os = "linux", feature = "browser-audio")),
    allow(dead_code)
)]
const QUEUE_FRAMES: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AudioCodec {
    Opus,
    PcmS16le,
}

impl AudioCodec {
    /// The token a viewer declares on its upgrade (`audio=<token>`) and every
    /// audio record and packet names.
    pub(crate) const fn token(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::PcmS16le => "pcm-s16le",
        }
    }

    pub(crate) fn parse(token: &str) -> Option<Self> {
        [Self::Opus, Self::PcmS16le]
            .into_iter()
            .find(|codec| codec.token() == token)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AudioFormat {
    pub stream_id: String,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_samples: usize,
    /// Measured Opus encoder lookahead, to discard once at this subscription's start.
    pub priming_samples: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioPacket {
    /// A new subscription has its own format/epoch and encoder, without replay of earlier samples.
    pub format: Arc<AudioFormat>,
    pub seq: u64,
    /// Sandbox CLOCK_MONOTONIC microseconds at the first captured PCM sample.
    pub ts: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AudioError {
    Unavailable,
    Retired,
    #[cfg_attr(
        not(all(target_os = "linux", feature = "browser-audio")),
        allow(dead_code)
    )]
    Overrun,
    #[cfg_attr(
        not(all(target_os = "linux", feature = "browser-audio")),
        allow(dead_code)
    )]
    Discontinuity,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AudioObservation {
    pub compiled: bool,
    pub ready: bool,
    pub subscribers: usize,
    pub startup_us: u64,
    pub failure: Option<AudioError>,
}

#[derive(Default)]
struct QueueState {
    packets: VecDeque<AudioPacket>,
    ended: Option<AudioError>,
}
#[derive(Default)]
pub(super) struct AudioQueue {
    state: Mutex<QueueState>,
    wake: tokio::sync::Notify,
}
impl AudioQueue {
    #[cfg_attr(
        not(all(target_os = "linux", feature = "browser-audio")),
        allow(dead_code)
    )]
    fn push(&self, packet: AudioPacket) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.ended.is_some() {
            return false;
        }
        if state.packets.len() == QUEUE_FRAMES {
            state.packets.clear();
            state.ended = Some(AudioError::Overrun);
        } else {
            state.packets.push_back(packet);
        }
        let accepted = state.ended.is_none();
        drop(state);
        self.wake.notify_one();
        accepted
    }
    fn end(&self, reason: AudioError) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.packets.clear();
        state.ended.get_or_insert(reason);
        drop(state);
        self.wake.notify_one();
    }
    async fn recv(&self) -> Result<AudioPacket, AudioError> {
        loop {
            let notified = self.wake.notified();
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(reason) = state.ended {
                    return Err(reason);
                }
                if let Some(packet) = state.packets.pop_front() {
                    return Ok(packet);
                }
            }
            notified.await;
        }
    }
}

/// An output subscription cannot keep Chrome or its private server alive.
pub(crate) struct AudioSubscription {
    queue: Arc<AudioQueue>,
    #[cfg(all(target_os = "linux", feature = "browser-audio"))]
    source: std::sync::Weak<server::Shared>,
    #[cfg(all(target_os = "linux", feature = "browser-audio"))]
    id: uuid::Uuid,
}
impl AudioSubscription {
    /// One reader owns ordering and consumes this subscription's bounded queue.
    pub(crate) async fn recv(&mut self) -> Result<AudioPacket, AudioError> {
        self.queue.recv().await
    }
    /// A packet held by a socket writer is invalid after mute, retirement or overrun.
    pub(crate) fn is_live(&self) -> bool {
        self.queue
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ended
            .is_none()
    }
}
impl Drop for AudioSubscription {
    fn drop(&mut self) {
        self.queue.end(AudioError::Retired);
        #[cfg(all(target_os = "linux", feature = "browser-audio"))]
        if let Some(source) = self.source.upgrade() {
            source.remove(self.id);
        }
    }
}

/// Cloneable observation/subscription handle. Only RetainedAudio owns the process lifetime.
#[derive(Clone)]
pub(crate) struct AudioSource {
    #[cfg(all(target_os = "linux", feature = "browser-audio"))]
    shared: Arc<server::Shared>,
}
impl AudioSource {
    pub(crate) fn same_source(&self, other: &Self) -> bool {
        #[cfg(all(target_os = "linux", feature = "browser-audio"))]
        return Arc::ptr_eq(&self.shared, &other.shared);
        #[cfg(not(all(target_os = "linux", feature = "browser-audio")))]
        {
            let _ = other;
            false
        }
    }
    pub(crate) fn observe(&self) -> AudioObservation {
        #[cfg(all(target_os = "linux", feature = "browser-audio"))]
        return self.shared.observe();
        #[cfg(not(all(target_os = "linux", feature = "browser-audio")))]
        AudioObservation {
            compiled: false,
            ready: false,
            subscribers: 0,
            startup_us: 0,
            failure: Some(AudioError::Unavailable),
        }
    }
    pub(crate) fn subscribe(&self, codec: AudioCodec) -> Result<AudioSubscription, AudioError> {
        #[cfg(all(target_os = "linux", feature = "browser-audio"))]
        return self.shared.subscribe(codec);
        #[cfg(not(all(target_os = "linux", feature = "browser-audio")))]
        {
            let _ = codec;
            Err(AudioError::Unavailable)
        }
    }
}

#[cfg(all(test, target_os = "linux", feature = "browser-audio"))]
mod e2e;
#[cfg(test)]
mod tests;
