//! Negotiated binary media on the existing viewer connection. Image, audio and
//! video retain their own envelopes, limits, epochs and sequence and
//! acknowledgement owners.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use crate::native::video::VideoCodec;

const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
pub(super) const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// A video unit's bounds (contract section 2), which every hop checks.
const MAX_VIDEO_HEADER_BYTES: usize = 4096;
/// One application fragment; independent of the logical coded-picture allowance.
pub(super) const MAX_VIDEO_PART_BYTES: usize = 16 * 1024;
/// Cached viewers declare no coded-capacity extension and retain this limit.
pub(super) const LEGACY_VIDEO_BYTES: usize = 4 * 1024 * 1024;
const MAX_CODED: u32 = 4096;

/// One video unit: one temporal unit of one picture, in its epoch
/// (`stream_id`) at `seq`. The header is the contract's closed shape; a key
/// unit, and only a key unit, names the stream's codec string. None for
/// anything the contract forbids, which no hop would forward.
pub(super) fn binary_video(
    unit: &super::video::Unit,
    codec: VideoCodec,
    stream_id: &str,
    seq: u64,
) -> Option<Vec<u8>> {
    let header = video_header(unit, codec, stream_id, seq)?;
    let mut message = Vec::with_capacity(4 + header.len() + unit.data.len());
    message.extend_from_slice(&(header.len() as u32).to_be_bytes());
    message.extend_from_slice(&header);
    message.extend_from_slice(&unit.data);
    Some(message)
}

/// Before the connection assigns its epoch/sequence, the same serializer
/// bounds the complete unit cost. Canonical UUIDs have36 bytes; the largest
/// admitted sequence has the most decimal digits. A reset only shortens it.
/// This is an upper bound on this protocol's header, not a payload estimate.
pub(super) fn video_budget_bytes(unit: &super::video::Unit, codec: VideoCodec) -> Option<usize> {
    let header = video_header(
        unit,
        codec,
        "00000000-0000-0000-0000-000000000000",
        MAX_SAFE_INTEGER,
    )?;
    Some(4 + header.len() + unit.data.len())
}

fn video_header(
    unit: &super::video::Unit,
    codec: VideoCodec,
    stream_id: &str,
    seq: u64,
) -> Option<Vec<u8>> {
    encode_video_header(video_description(unit, codec, stream_id, seq)?)
}

fn encode_video_header(header: Value) -> Option<Vec<u8>> {
    let header = serde_json::to_vec(&header).ok()?;
    (header.len() <= MAX_VIDEO_HEADER_BYTES).then_some(header)
}

fn video_description(
    unit: &super::video::Unit,
    codec: VideoCodec,
    stream_id: &str,
    seq: u64,
) -> Option<Value> {
    let (width, height) = unit.coded;
    let visible = unit.visible;
    let inside = |offset: i32, extent: u32, bound: u32| {
        u32::try_from(offset).is_ok_and(|offset| {
            extent > 0 && offset.checked_add(extent).is_some_and(|end| end <= bound)
        })
    };
    if seq == 0
        || seq > MAX_SAFE_INTEGER
        || unit.ts > MAX_SAFE_INTEGER
        || unit.input_seq.is_some_and(|input| input > MAX_SAFE_INTEGER)
        || unit.data.is_empty()
        || unit.data.len() > codec.coded_capacity(width, height).unwrap_or(0)
        || !(1..=MAX_CODED).contains(&width)
        || !(1..=MAX_CODED).contains(&height)
        || !inside(visible.x, visible.width, width)
        || !inside(visible.y, visible.height, height)
        || unit.key != unit.codec_string.is_some()
    {
        return None;
    }
    let mut header = json!({
        "type": "media", "track": "video", "codec": codec.token(), "streamId": stream_id,
        "seq": seq, "ts": unit.ts, "key": unit.key,
        "coded": {"width": width, "height": height}, "visible": visible, "surface": unit.surface,
        "quality": unit.quality.label(), "byteLength": unit.data.len(),
    });
    if let Some(codec_string) = &unit.codec_string {
        header["codecString"] = json!(codec_string);
    }
    if let Some(input_seq) = unit.input_seq {
        header["inputSeq"] = json!(input_seq);
    }
    Some(header)
}

/// The first fragment reuses the canonical picture descriptor; later ones
/// carry only its identity and contiguous payload offset. The caller owns
/// the offset and commits it only after the message is actually written.
pub(super) fn binary_video_part(
    unit: &super::video::Unit,
    codec: VideoCodec,
    stream_id: &str,
    seq: u64,
    offset: usize,
    max_wire_bytes: usize,
) -> Option<(Vec<u8>, usize)> {
    let header = video_part_header(unit, codec, stream_id, seq, offset)?;
    let payload_bytes = max_wire_bytes
        .min(MAX_VIDEO_PART_BYTES)
        .checked_sub(4 + header.len())?
        .min(unit.data.len() - offset);
    if payload_bytes == 0 {
        return None;
    }
    let end = offset.checked_add(payload_bytes)?;
    let mut message = Vec::with_capacity(4 + header.len() + payload_bytes);
    message.extend_from_slice(&(header.len() as u32).to_be_bytes());
    message.extend_from_slice(&header);
    message.extend_from_slice(&unit.data[offset..end]);
    Some((message, end))
}

/// The smallest legal nonempty fragment, using the same canonical header.
pub(super) fn video_part_minimum(
    unit: &super::video::Unit,
    codec: VideoCodec,
    stream_id: &str,
    seq: u64,
    offset: usize,
) -> Option<usize> {
    Some(4 + video_part_header(unit, codec, stream_id, seq, offset)?.len() + 1)
}

fn video_part_header(
    unit: &super::video::Unit,
    codec: VideoCodec,
    stream_id: &str,
    seq: u64,
    offset: usize,
) -> Option<Vec<u8>> {
    if offset >= unit.data.len() || seq == 0 || seq > MAX_SAFE_INTEGER {
        return None;
    }
    let header = if offset == 0 {
        let mut header = video_description(unit, codec, stream_id, seq)?;
        header["offset"] = json!(0);
        encode_video_header(header)?
    } else {
        encode_video_header(json!({
            "type":"media", "track":"video", "streamId":stream_id,
            "seq":seq, "offset":offset,
        }))?
    };
    Some(header)
}

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

    fn video_unit(key: bool, bytes: usize) -> crate::native::stream::video::Unit {
        crate::native::stream::video::Unit {
            data: vec![7; bytes],
            wire_bytes: 0,
            key,
            ts: 1_234_567,
            coded: (2048, 2048),
            visible: crate::native::display::Rect {
                x: 0,
                y: 0,
                width: 1840,
                height: 1888,
            },
            surface: crate::native::display::Surface::new(2048, 2048),
            input_seq: Some(4411),
            quality: crate::native::stream::video::Quality::Motion,
            codec_string: key.then(|| "av01.1.12M.08".into()),
            stages: None,
        }
    }

    fn small_video_unit(key: bool, bytes: usize) -> crate::native::stream::video::Unit {
        let mut unit = video_unit(key, bytes);
        unit.coded = (64, 64);
        unit.visible = crate::native::display::Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        unit.surface = crate::native::display::Surface::new(64, 64);
        unit
    }

    /// A video unit is the contract's closed header and the exact encoded
    /// bytes; only a key unit names the codec string.
    #[test]
    fn a_video_unit_is_the_contracts_header_and_the_exact_payload() {
        let stream = uuid::Uuid::new_v4().to_string();
        let unit = video_unit(true, 10_240);
        let message = binary_video(&unit, VideoCodec::Av1Full, &stream, 1).unwrap();
        let (header, payload) = decode(&message);
        assert_eq!(payload, unit.data.as_slice());
        assert_eq!(
            header,
            json!({"type":"media","track":"video","codec":"av1-444","streamId":stream,
                "seq":1,"ts":1_234_567,"key":true,"codecString":"av01.1.12M.08",
                "coded":{"width":2048,"height":2048},
                "visible":{"x":0,"y":0,"width":1840,"height":1888},
                "surface":serde_json::to_value(&unit.surface).unwrap(),
                "inputSeq":4411,"quality":"motion","byteLength":10_240})
        );
        assert!(header.to_string().len() <= MAX_VIDEO_HEADER_BYTES);
        let dependent = video_unit(false, 1);
        let (header, _) = decode(&binary_video(&dependent, VideoCodec::Av1, &stream, 2).unwrap());
        assert_eq!(header["key"], false);
        assert!(header.get("codecString").is_none(), "{header}");
        let mut untagged = video_unit(false, 1);
        untagged.input_seq = None;
        let (header, _) = decode(&binary_video(&untagged, VideoCodec::Av1, &stream, 3).unwrap());
        assert!(header.get("inputSeq").is_none(), "absent, never null");
    }

    /// What the contract forbids never becomes a unit: every hop would close
    /// the channel on it.
    #[test]
    fn a_video_unit_outside_the_contract_is_refused() {
        let stream = uuid::Uuid::new_v4().to_string();
        let refused = |unit: &crate::native::stream::video::Unit, seq: u64| {
            binary_video(unit, VideoCodec::Av1Full, &stream, seq).is_none()
        };
        assert!(refused(&video_unit(true, 0), 1), "an empty payload");
        assert!(
            refused(
                &small_video_unit(
                    true,
                    VideoCodec::Av1Full.coded_capacity(64, 64).unwrap() + 1
                ),
                1
            ),
            "over the dimensional source allowance"
        );
        assert!(
            !refused(
                &small_video_unit(true, VideoCodec::Av1Full.coded_capacity(64, 64).unwrap()),
                1
            ),
            "dimensional source allowance exactly"
        );
        assert!(refused(&video_unit(true, 8), 0), "sequence 0");
        assert!(refused(&video_unit(true, 8), MAX_SAFE_INTEGER + 1));
        let mut unit = video_unit(true, 8);
        unit.ts = MAX_SAFE_INTEGER + 1;
        assert!(refused(&unit, 1));
        let mut unit = video_unit(true, 8);
        unit.input_seq = Some(MAX_SAFE_INTEGER + 1);
        assert!(refused(&unit, 1));
        for coded in [(0, 2048), (2048, 0), (4097, 2048), (2048, 4097)] {
            let mut unit = video_unit(true, 8);
            unit.coded = coded;
            assert!(refused(&unit, 1), "{coded:?}");
        }
        let mut unit = video_unit(true, 8);
        unit.visible.width = 2049;
        assert!(refused(&unit, 1), "visible wider than coded");
        let mut unit = video_unit(true, 8);
        unit.visible.y = 200;
        assert!(refused(&unit, 1), "visible below coded");
        let mut unit = video_unit(true, 8);
        unit.visible.height = 0;
        assert!(refused(&unit, 1), "an empty window");
        let mut unit = video_unit(true, 8);
        unit.visible.x = -1;
        assert!(refused(&unit, 1), "a window left of the picture");
        let mut unit = video_unit(true, 8);
        unit.codec_string = None;
        assert!(refused(&unit, 1), "a key unit without its codec string");
        let mut unit = video_unit(false, 8);
        unit.codec_string = Some("av01.1.12M.08".into());
        assert!(refused(&unit, 2), "a dependent unit with one");
    }

    #[test]
    fn video_budget_is_the_canonical_wire_cost_including_sequence_growth_and_reset_headers() {
        let stream = uuid::Uuid::new_v4().to_string();
        for codec in [VideoCodec::Av1Full, VideoCodec::Av1] {
            for key in [false, true] {
                for bytes in [1, 42, 1562, LEGACY_VIDEO_BYTES + 1] {
                    let mut unit = video_unit(key, bytes);
                    if key {
                        unit.codec_string = Some(format!(
                            "av01.{}.12M.08",
                            u8::from(codec == VideoCodec::Av1Full)
                        ));
                    }
                    let bound = video_budget_bytes(&unit, codec).unwrap();
                    for seq in [1, 9, 10, 99, 100, 999, 1000, MAX_SAFE_INTEGER] {
                        let actual = binary_video(&unit, codec, &stream, seq).unwrap().len();
                        assert!(
                            bound >= actual,
                            "header undercount at{seq},key={key},bytes={bytes}"
                        );
                        assert!(
                            bound - actual <= 15,
                            "only sequence-digit headroom is charged"
                        );
                        if seq == MAX_SAFE_INTEGER {
                            assert_eq!(bound, actual);
                        }
                    }
                }
            }
        }
        assert!(video_budget_bytes(
            &small_video_unit(
                true,
                VideoCodec::Av1Full.coded_capacity(64, 64).unwrap() + 1
            ),
            VideoCodec::Av1Full
        )
        .is_none());
    }

    #[test]
    fn video_parts_reuse_the_picture_descriptor_and_exact_contiguous_payload() {
        let stream = "00000000-0000-4000-8000-000000000001";
        let mut unit = video_unit(true, 48_123);
        for (offset, byte) in unit.data.iter_mut().enumerate() {
            *byte = (offset % 251) as u8;
        }
        let whole = binary_video(&unit, VideoCodec::Av1Full, stream, 999).unwrap();
        let (whole_header, _) = decode(&whole);
        for ceiling in [1024, 3125, MAX_VIDEO_PART_BYTES, usize::MAX] {
            let mut offset = 0;
            let mut rebuilt = Vec::new();
            let mut wire_bytes = 0;
            let mut count = 0;
            while offset < unit.data.len() {
                let (part, end) =
                    binary_video_part(&unit, VideoCodec::Av1Full, stream, 999, offset, ceiling)
                        .unwrap();
                assert!(part.len() <= ceiling.min(MAX_VIDEO_PART_BYTES));
                let (mut header, payload) = decode(&part);
                assert_eq!(header["offset"], offset);
                assert_eq!(header["seq"], 999);
                assert_eq!(payload, &unit.data[offset..end]);
                assert!(!payload.is_empty());
                if offset == 0 {
                    header.as_object_mut().unwrap().remove("offset");
                    assert_eq!(header, whole_header);
                } else {
                    assert_eq!(
                        header,
                        json!({"type":"media","track":"video",
                        "streamId":stream,"seq":999,"offset":offset})
                    );
                }
                wire_bytes += part.len();
                rebuilt.extend_from_slice(payload);
                offset = end;
                count += 1;
            }
            assert_eq!(rebuilt, unit.data);
            assert!(count >= 3);
            assert!(
                wire_bytes > whole.len(),
                "every repeated envelope has a cost"
            );
            assert!(
                binary_video_part(&unit, VideoCodec::Av1Full, stream, 999, offset, ceiling)
                    .is_none(),
                "no empty final fragment"
            );
        }
    }

    #[test]
    fn video_parts_refuse_invalid_first_descriptors_and_too_small_envelopes() {
        let stream = "00000000-0000-4000-8000-000000000001";
        let unit = video_unit(true, 100);
        for seq in [0, MAX_SAFE_INTEGER + 1] {
            assert!(binary_video_part(
                &unit,
                VideoCodec::Av1Full,
                stream,
                seq,
                0,
                MAX_VIDEO_PART_BYTES
            )
            .is_none());
        }
        for offset in [unit.data.len(), usize::MAX] {
            assert!(binary_video_part(
                &unit,
                VideoCodec::Av1Full,
                stream,
                1,
                offset,
                MAX_VIDEO_PART_BYTES
            )
            .is_none());
        }
        let (first, _) = binary_video_part(
            &unit,
            VideoCodec::Av1Full,
            stream,
            1,
            0,
            MAX_VIDEO_PART_BYTES,
        )
        .unwrap();
        let header_bytes = 4 + u32::from_be_bytes(first[..4].try_into().unwrap()) as usize;
        for ceiling in [0, 4, header_bytes] {
            assert!(binary_video_part(&unit, VideoCodec::Av1Full, stream, 1, 0, ceiling).is_none());
        }
        let (minimum, end) =
            binary_video_part(&unit, VideoCodec::Av1Full, stream, 1, 0, header_bytes + 1).unwrap();
        assert_eq!(end, 1);
        assert_eq!(minimum.len(), header_bytes + 1);
        let mut invalid = video_unit(true, 10);
        invalid.visible.x = -1;
        assert!(binary_video_part(
            &invalid,
            VideoCodec::Av1Full,
            stream,
            1,
            0,
            MAX_VIDEO_PART_BYTES
        )
        .is_none());
    }

    #[test]
    #[ignore = "composition fixture: AMBIT_LARGE_NATIVE_KEY and AMBIT_LARGE_WIRE_OUT"]
    fn serialize_actual_large_native_key_without_changing_one_payload_byte() {
        let data = std::fs::read(std::env::var("AMBIT_LARGE_NATIVE_KEY").unwrap()).unwrap();
        assert_eq!(data.len(), 21_734_926);
        let mut unit = video_unit(true, 0);
        unit.data = data;
        unit.coded = (4096, 4096);
        unit.visible = crate::native::display::Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 4096,
        };
        unit.surface = crate::native::display::Surface::new(4096, 4096);
        unit.codec_string = Some("av01.1.16M.08".into());
        unit.wire_bytes = video_budget_bytes(&unit, VideoCodec::Av1Full).unwrap();
        let body = binary_video(
            &unit,
            VideoCodec::Av1Full,
            "00000000-0000-4000-8000-000000000001",
            1,
        )
        .unwrap();
        let (header, payload) = decode(&body);
        assert_eq!(payload, unit.data);
        assert_eq!(header["byteLength"], 21_734_926);
        std::fs::write(std::env::var("AMBIT_LARGE_WIRE_OUT").unwrap(), &body).unwrap();
        println!(
            "LARGE_NATIVE_WIRE {}",
            json!({"payloadBytes":payload.len(),"wireBytes":body.len(),"budgetBytes":unit.wire_bytes,"header":header})
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
