//! Negotiated binary media on the existing viewer connection. Image and audio
//! retain their own envelopes, limits, epochs and sequence/acknowledgement owners.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
pub(super) const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Audio is one 10 ms unit; it never consumes a JPEG sequence or frame-window slot.
pub(super) fn binary_audio(packet: &crate::native::audio::AudioPacket) -> Option<Vec<u8>> {
    use crate::native::audio::{AudioCodec, CHANNELS, FRAME_BYTES, FRAME_SAMPLES, SAMPLE_RATE};
    let format = &packet.format;
    if packet.seq == 0
        || packet.seq > MAX_SAFE_INTEGER
        || packet.ts > MAX_SAFE_INTEGER
        || format.sample_rate != SAMPLE_RATE
        || format.channels != CHANNELS
        || format.frame_samples != FRAME_SAMPLES
        || format.priming_samples > SAMPLE_RATE
        || uuid::Uuid::parse_str(&format.stream_id).is_err()
        || packet.data.is_empty()
        || packet.data.len() > 4096
        || (format.codec == AudioCodec::PcmS16le
            && (packet.data.len() != FRAME_BYTES || format.priming_samples != 0))
    {
        return None;
    }
    let header = serde_json::to_vec(&json!({
        "type":"media", "track":"audio", "codec":format.codec, "streamId":format.stream_id,
        "seq":packet.seq, "ts":packet.ts, "samples":FRAME_SAMPLES, "byteLength":packet.data.len()
    }))
    .ok()?;
    if header.len() > 1024 {
        return None;
    }
    let mut message = Vec::with_capacity(4 + header.len() + packet.data.len());
    message.extend_from_slice(&(header.len() as u32).to_be_bytes());
    message.extend_from_slice(&header);
    message.extend_from_slice(&packet.data);
    Some(message)
}

fn take_image(value: &mut Value) -> Option<Vec<u8>> {
    let object = value.as_object_mut()?;
    let text = object.remove("data")?;
    let data = STANDARD.decode(text.as_str()?).ok()?;
    if data.is_empty() || data.len() > MAX_FRAME_BYTES {
        return None;
    }
    object.insert("byteLength".into(), json!(data.len()));
    Some(data)
}

/// One WebSocket message is a four-byte big-endian header length, that JSON
/// header, then each JPEG in declaration order. Whole frames have byteLength;
/// deltas declare byteLength on each patch. No offsets or trailing bytes are
/// implicit, and the exact dependency/surface fields remain unchanged.
pub(super) fn binary_frame(text: &str) -> Option<Vec<u8>> {
    let mut header: Value = serde_json::from_str(text).ok()?;
    if header["type"] != "frame" || header["seq"].as_u64()? == 0 {
        return None;
    }
    let mut images = Vec::new();
    if header.get("patches").is_some() {
        if header.get("data").is_some() {
            return None;
        }
        let patches = header.get_mut("patches")?.as_array_mut()?;
        if patches.is_empty() || patches.len() > 64 {
            return None;
        }
        for patch in patches {
            images.push(take_image(patch)?);
        }
    } else {
        images.push(take_image(&mut header)?);
    }
    // CDP's legacy frame has no encoding field but its capture format is JPEG.
    header["encoding"] = json!("jpeg");
    let metadata = serde_json::to_vec(&header).ok()?;
    if metadata.len() > MAX_HEADER_BYTES {
        return None;
    }
    let size = images.iter().try_fold(4 + metadata.len(), |size, image| {
        size.checked_add(image.len())
    })?;
    if size > MAX_FRAME_BYTES {
        return None;
    }
    let mut message = Vec::with_capacity(size);
    message.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
    message.extend_from_slice(&metadata);
    for image in images {
        message.extend_from_slice(&image);
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::audio::{AudioCodec, AudioFormat, AudioPacket};
    use std::sync::Arc;

    fn packet(codec: AudioCodec, size: usize) -> AudioPacket {
        AudioPacket {
            format: Arc::new(AudioFormat {
                stream_id: uuid::Uuid::new_v4().to_string(),
                codec,
                sample_rate: 48_000,
                channels: 2,
                frame_samples: 480,
                priming_samples: if codec == AudioCodec::Opus { 120 } else { 0 },
            }),
            seq: 1,
            ts: 10_000,
            data: vec![17; size],
        }
    }
    #[test]
    fn audio_envelope_keeps_its_track_epoch_and_exact_bounded_payload() {
        for (codec, size) in [(AudioCodec::Opus, 4096), (AudioCodec::PcmS16le, 1920)] {
            let source = packet(codec, size);
            let frame = binary_audio(&source).unwrap();
            let (metadata, payload) = decode(&frame);
            assert_eq!(payload, source.data);
            assert_eq!(
                metadata,
                json!({"type":"media","track":"audio","codec":codec,
                "streamId":source.format.stream_id,"seq":1,"ts":10_000,"samples":480,"byteLength":size})
            );
            assert!(frame.len() <= 4 + 1024 + 4096);
        }
    }
    #[test]
    fn invalid_audio_cannot_enter_the_image_or_audio_wire() {
        let mut bad = vec![
            packet(AudioCodec::Opus, 0),
            packet(AudioCodec::Opus, 4097),
            packet(AudioCodec::PcmS16le, 1919),
            packet(AudioCodec::PcmS16le, 1921),
        ];
        let mut value = packet(AudioCodec::Opus, 80);
        value.seq = 0;
        bad.push(value);
        let mut value = packet(AudioCodec::Opus, 80);
        value.ts = MAX_SAFE_INTEGER + 1;
        bad.push(value);
        let mut value = packet(AudioCodec::Opus, 80);
        value.seq = MAX_SAFE_INTEGER + 1;
        bad.push(value);
        let mut value = packet(AudioCodec::Opus, 80);
        Arc::make_mut(&mut value.format).stream_id = "not-an-epoch".into();
        bad.push(value);
        let mut value = packet(AudioCodec::Opus, 80);
        Arc::make_mut(&mut value.format).sample_rate = 44_100;
        bad.push(value);
        let mut value = packet(AudioCodec::Opus, 80);
        Arc::make_mut(&mut value.format).channels = 1;
        bad.push(value);
        let mut value = packet(AudioCodec::Opus, 80);
        Arc::make_mut(&mut value.format).frame_samples = 960;
        bad.push(value);
        let mut value = packet(AudioCodec::PcmS16le, 1920);
        Arc::make_mut(&mut value.format).priming_samples = 1;
        bad.push(value);
        for value in bad {
            assert!(binary_audio(&value).is_none(), "{value:?}");
        }
        assert!(
            binary_frame(r#"{"type":"media","track":"audio","seq":1,"data":"AQID"}"#).is_none()
        );
    }

    fn decode(frame: &[u8]) -> (Value, &[u8]) {
        let end = 4 + u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        (
            serde_json::from_slice(&frame[4..end]).unwrap(),
            &frame[end..],
        )
    }

    #[test]
    fn binary_whole_keeps_metadata_and_exact_jpeg_bytes() {
        let frame = binary_frame(&json!({"type":"frame","seq":7,"encoding":"jpeg","surface":{"generation":"same"},"data":"/9j/2Q=="}).to_string()).unwrap();
        let (header, jpeg) = decode(&frame);
        assert_eq!(header["byteLength"], 4);
        assert_eq!(header["seq"], 7);
        assert_eq!(header["surface"]["generation"], "same");
        assert!(header.get("data").is_none());
        assert_eq!(jpeg, [255, 216, 255, 217]);
    }

    #[test]
    fn binary_patches_preserve_crop_and_base_in_declaration_order() {
        let frame = binary_frame(
            &json!({"type":"frame","seq":8,"baseSeq":7,"patches":[
                {"x":16,"y":0,"width":32,"height":16,"sourceX":16,"sourceY":0,"data":"AQID"},
                {"x":48,"y":16,"width":32,"height":16,"sourceX":16,"sourceY":16,"data":"BAU="}
            ]})
            .to_string(),
        )
        .unwrap();
        let (header, bytes) = decode(&frame);
        assert_eq!(header["baseSeq"], 7);
        assert_eq!(header["patches"][0]["byteLength"], 3);
        assert_eq!(header["patches"][1]["byteLength"], 2);
        assert_eq!(header["patches"][1]["sourceY"], 16);
        assert!(header["patches"][0].get("data").is_none());
        assert_eq!(bytes, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn malformed_or_ambiguous_images_are_not_binary_frames() {
        for value in [
            json!({"type":"frame","seq":1,"data":"?"}),
            json!({"type":"frame","seq":1,"data":""}),
            json!({"type":"frame","seq":1,"data":"AQ==","patches":[]}),
            json!({"type":"frame","seq":1,"patches":[]}),
            json!({"type":"status","seq":1,"data":"AQ=="}),
        ] {
            assert!(binary_frame(&value.to_string()).is_none());
        }
    }
}
