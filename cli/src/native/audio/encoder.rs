use super::{
    AudioCodec, AudioError, AudioFormat, AudioPacket, CHANNELS, FRAME_SAMPLES, SAMPLE_RATE,
};
use std::sync::Arc;

pub(super) struct Encoder {
    opus: Option<opus::Encoder>,
    format: Arc<AudioFormat>,
    sequence: u64,
}
impl Encoder {
    pub(super) fn new(codec: AudioCodec) -> Result<Self, AudioError> {
        let mut opus = if codec == AudioCodec::Opus {
            let mut encoder = opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            )
            .map_err(|_| AudioError::Unavailable)?;
            encoder
                .set_bitrate(opus::Bitrate::Bits(128_000))
                .map_err(|_| AudioError::Unavailable)?;
            encoder.set_vbr(true).map_err(|_| AudioError::Unavailable)?;
            encoder
                .set_dtx(false)
                .map_err(|_| AudioError::Unavailable)?;
            encoder
                .set_inband_fec(false)
                .map_err(|_| AudioError::Unavailable)?;
            Some(encoder)
        } else {
            None
        };
        let priming_samples = opus
            .as_mut()
            .map(|encoder| encoder.get_lookahead())
            .transpose()
            .map_err(|_| AudioError::Unavailable)?
            .unwrap_or(0) as u32;
        Ok(Self {
            opus,
            sequence: 0,
            format: Arc::new(AudioFormat {
                stream_id: uuid::Uuid::new_v4().to_string(),
                codec,
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
                frame_samples: FRAME_SAMPLES,
                priming_samples,
            }),
        })
    }
    pub(super) fn encode(&mut self, samples: &[i16], ts: u64) -> Result<AudioPacket, AudioError> {
        if samples.len() != FRAME_SAMPLES * CHANNELS as usize {
            return Err(AudioError::Discontinuity);
        }
        let data = if let Some(encoder) = &mut self.opus {
            let mut bytes = vec![0; 4_096];
            let length = encoder
                .encode(samples, &mut bytes)
                .map_err(|_| AudioError::Discontinuity)?;
            bytes.truncate(length);
            bytes
        } else {
            samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect()
        };
        self.sequence += 1;
        Ok(AudioPacket {
            format: self.format.clone(),
            seq: self.sequence,
            ts,
            data,
        })
    }
}
