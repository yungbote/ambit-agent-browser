//! A cursor over one admitted shared picture. Received-prefix credit only
//! advances this transfer; the track's complete-picture paint Flow is separate.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use super::{LinkRate, Unit};
use crate::native::stream::wire;
use crate::native::video::VideoCodec;
use tokio_tungstenite::tungstenite::protocol::frame::FrameHeader;

struct Charge {
    end: usize,
    bytes: usize,
}

pub(super) struct Parts {
    unit: Arc<Unit>,
    codec: VideoCodec,
    stream: String,
    seq: u64,
    sent: usize,
    received: usize,
    outstanding: VecDeque<Charge>,
    bytes: usize,
    reserved: Option<Charge>,
    canonical_bytes: usize,
    socket_bytes: usize,
    rate: Option<LinkRate>,
    first_at: Option<Instant>,
}

impl Parts {
    pub(super) fn new(unit: Arc<Unit>, codec: VideoCodec, stream: String, seq: u64) -> Self {
        Self {
            unit,
            codec,
            stream,
            seq,
            sent: 0,
            received: 0,
            outstanding: VecDeque::new(),
            bytes: 0,
            reserved: None,
            canonical_bytes: 0,
            socket_bytes: 0,
            rate: None,
            first_at: None,
        }
    }

    pub(super) fn complete(&self) -> bool {
        self.sent == self.unit.data.len() && self.reserved.is_none()
    }

    pub(super) fn in_flight(&self) -> usize {
        self.bytes
    }

    /// Bootstrap is deliberately smaller than a paint/codec budget. The
    /// chunks-mode edge sends rate only after measured consumer-prefix
    /// progress, then the same existing rate owner governs byte pacing.
    pub(super) fn limits(&self) -> (usize, usize) {
        match self.rate {
            None => (1024, 3 * 1024),
            Some(rate) => {
                let quantum = (u64::from(rate.bits_per_second) / 8 / 40)
                    .max(1)
                    .min(wire::MAX_VIDEO_PART_BYTES as u64) as usize;
                (quantum, (2 * quantum).min(rate.burst_bytes as usize))
            }
        }
    }

    pub(super) fn set_rate(&mut self, rate: LinkRate) {
        self.rate = Some(rate);
    }

    pub(super) fn ready(&self) -> bool {
        let (_, window) = self.limits();
        if self.complete() || self.reserved.is_some() || window == 0 {
            return false;
        }
        let Some(minimum) =
            wire::video_part_minimum(&self.unit, self.codec, &self.stream, self.seq, self.sent)
        else {
            return false;
        };
        let minimum = minimum + FrameHeader::default().len(minimum as u64);
        self.bytes == 0
            || window
                .checked_sub(self.bytes)
                .is_some_and(|room| room >= minimum)
    }

    pub(super) fn first_at(&self) -> Option<Instant> {
        self.first_at
    }

    pub(super) fn follows(
        &self,
        surface: &crate::native::display::Surface,
        window: (u32, u32),
    ) -> bool {
        self.unit.follows(surface, window)
    }

    /// Reserve the exact receivable endpoint before I/O. A receipt may arrive
    /// before send completion, but the outstanding I/O reservation still
    /// prevents another message. Failure/retirement drops this transfer.
    pub(super) fn prepare(
        &mut self,
        max_wire: usize,
        window: usize,
        now: Instant,
    ) -> Option<Vec<u8>> {
        if self.reserved.is_some() || self.complete() || window == 0 {
            return None;
        }
        // Use the actual serializer for this server's unmasked WS framing,
        // rather than guessing a fixed transport-header cost.
        let frame_header = FrameHeader::default();
        let minimum =
            wire::video_part_minimum(&self.unit, self.codec, &self.stream, self.seq, self.sent)?;
        let minimum_socket = minimum.checked_add(frame_header.len(minimum as u64))?;
        // A valid tiny rate cannot fit even the next header in25ms. Only an
        // empty window may overrun, with the smallest actual nonempty part;
        // that complete charge must drain before another part can leave.
        let available = if self.bytes == 0 {
            window.max(minimum_socket)
        } else {
            window.checked_sub(self.bytes)?
        };
        let socket_ceiling = max_wire.max(minimum_socket).min(available);
        let mut ceiling = socket_ceiling.min(wire::MAX_VIDEO_PART_BYTES);
        while ceiling.checked_add(frame_header.len(ceiling as u64))? > socket_ceiling {
            ceiling = ceiling.checked_sub(1)?;
        }
        let (message, end) = wire::binary_video_part(
            &self.unit,
            self.codec,
            &self.stream,
            self.seq,
            self.sent,
            ceiling,
        )?;
        let bytes = message
            .len()
            .checked_add(frame_header.len(message.len() as u64))?;
        self.bytes = self.bytes.checked_add(bytes)?;
        self.reserved = Some(Charge { end, bytes });
        self.outstanding.push_back(Charge { end, bytes });
        self.canonical_bytes += message.len();
        self.socket_bytes += bytes;
        self.first_at.get_or_insert(now);
        Some(message)
    }

    /// Success commits the issued endpoint. An already-received reservation
    /// has released its byte charge; never add it to the ledger a second time.
    pub(super) fn sent(&mut self) -> Option<(usize, usize)> {
        let charge = self.reserved.take()?;
        self.sent = charge.end;
        let result = (charge.end, charge.bytes);
        Some(result)
    }

    /// A receipt must end at an actually issued fragment. Duplicate, backward,
    /// partial, future or another-picture receipts release no byte credit.
    pub(super) fn receive(&mut self, stream: &str, seq: u64, offset: usize) -> bool {
        if stream != self.stream
            || seq != self.seq
            || offset <= self.received
            || offset > self.reserved.as_ref().map_or(self.sent, |part| part.end)
            || !self.outstanding.iter().any(|part| part.end == offset)
        {
            return false;
        }
        while self
            .outstanding
            .front()
            .is_some_and(|part| part.end <= offset)
        {
            let part = self.outstanding.pop_front().unwrap();
            self.bytes -= part.bytes;
        }
        self.received = offset;
        true
    }

    pub(super) fn costs(&self) -> (usize, usize) {
        (self.canonical_bytes, self.socket_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::display::{Rect, Surface};
    use crate::native::stream::video::Quality;

    fn parts() -> Parts {
        let unit = Arc::new(Unit {
            data: (0..48_000).map(|offset| (offset % 251) as u8).collect(),
            wire_bytes: 0,
            key: true,
            ts: 1,
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
            codec_string: Some("av01.1.12M.08".into()),
        });
        Parts::new(
            unit,
            VideoCodec::Av1Full,
            "00000000-0000-4000-8000-000000000001".into(),
            1,
        )
    }

    #[test]
    fn received_prefix_releases_only_committed_fragment_boundaries() {
        let mut picture = parts();
        let stream = picture.stream.clone();
        let first = picture.prepare(1024, 3125, Instant::now()).unwrap();
        let reserved = picture.reserved.as_ref().unwrap().end;
        assert!(!picture.receive(&stream, 1, reserved + 1), "not issued");
        assert!(
            picture.prepare(1024, 3125, Instant::now()).is_none(),
            "one reservation"
        );
        let (end1, charge1) = picture.sent().unwrap();
        assert_eq!(
            charge1,
            first.len() + FrameHeader::default().len(first.len() as u64)
        );
        picture.prepare(1024, 3125, Instant::now()).unwrap();
        let (end2, charge2) = picture.sent().unwrap();
        let outstanding = picture.in_flight();
        for (id, seq, offset) in [
            ("other", 1, end1),
            (stream.as_str(), 2, end1),
            (stream.as_str(), 1, 0),
            (stream.as_str(), 1, end1 - 1),
            (stream.as_str(), 1, end2 + 1),
            (stream.as_str(), 1, usize::MAX),
        ] {
            assert!(!picture.receive(id, seq, offset));
            assert_eq!(picture.in_flight(), outstanding);
        }
        assert!(picture.receive(&stream, 1, end2), "coalesced exact prefix");
        assert_eq!(picture.in_flight(), 0);
        assert_eq!(outstanding, charge1 + charge2);
        assert!(!picture.receive(&stream, 1, end1));
        assert!(!picture.receive(&stream, 1, end2));
    }

    #[test]
    fn small_window_drains_large_picture_without_paint_and_releases_shared_storage_on_cancel() {
        let mut picture = parts();
        let source = picture.unit.clone();
        let stream = picture.stream.clone();
        let mut sent_envelopes = 0;
        let mut sent_socket = 0;
        let mut rebuilt = Vec::new();
        while !picture.complete() {
            let part = picture.prepare(1024, 3125, Instant::now()).unwrap();
            let header_end = 4 + u32::from_be_bytes(part[..4].try_into().unwrap()) as usize;
            rebuilt.extend_from_slice(&part[header_end..]);
            sent_envelopes += part.len();
            let (end, cost) = picture.sent().unwrap();
            sent_socket += cost;
            assert!(picture.in_flight() <= 3125);
            assert!(picture.receive(&stream, 1, end));
        }
        assert_eq!(rebuilt, source.data);
        assert_eq!(picture.costs(), (sent_envelopes, sent_socket));
        assert_eq!(picture.in_flight(), 0);
        assert_eq!(Arc::strong_count(&source), 2);
        drop(picture);
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn stalled_window_and_rate_drop_cannot_reserve_another_fragment() {
        let mut picture = parts();
        for _ in 0..3 {
            picture.prepare(1024, 3125, Instant::now()).unwrap();
            picture.sent().unwrap();
        }
        let retained = picture.in_flight();
        assert!(picture.prepare(1024, 3125, Instant::now()).is_none());
        assert!(picture.prepare(1024, 1000, Instant::now()).is_none());
        assert_eq!(picture.in_flight(), retained);
        assert_eq!(picture.outstanding.len(), 3);
    }

    #[test]
    fn early_receipt_settles_reservation_without_duplicate_charge_or_overlapping_io() {
        let mut picture = parts();
        let stream = picture.stream.clone();
        picture.prepare(1024, 3125, Instant::now()).unwrap();
        let end = picture.reserved.as_ref().unwrap().end;
        assert!(picture.receive(&stream, 1, end));
        assert_eq!(picture.in_flight(), 0);
        assert!(
            picture.prepare(1024, 3125, Instant::now()).is_none(),
            "I/O still pending"
        );
        assert!(!picture.complete());
        assert_eq!(picture.sent().unwrap().0, end);
        assert!(
            picture.outstanding.is_empty(),
            "no second charge at callback"
        );
        assert!(!picture.receive(&stream, 1, end));
        assert!(picture.prepare(1024, 3125, Instant::now()).is_some());
    }

    #[test]
    fn tiny_rate_reserves_actual_minimum_envelopes_and_rate_drop_preserves_credit() {
        let mut picture = parts();
        let now = Instant::now();
        let slow = LinkRate {
            bits_per_second: 1000,
            burst_bytes: 1,
        };
        picture.set_rate(slow);
        assert_eq!(picture.limits(), (3, 1));
        let stream = picture.stream.clone();
        for _ in 0..2 {
            assert!(picture.ready());
            let part = picture.prepare(3, 1, now).unwrap();
            let (end, cost) = picture.sent().unwrap();
            assert!(cost > 1, "only actual minimum envelope overrun");
            let header_end = 4 + u32::from_be_bytes(part[..4].try_into().unwrap()) as usize;
            assert_eq!(part.len() - header_end, 1);
            assert!(!picture.ready(), "receipt must drain full overrun");
            assert!(picture.receive(&stream, 1, end));
            assert!(picture.ready());
        }
        let at = Instant::now();
        picture.set_rate(LinkRate {
            bits_per_second: 500_000,
            burst_bytes: 65_536,
        });
        let (quantum, window) = picture.limits();
        assert_eq!((quantum, window), (1562, 3124));
        picture.prepare(quantum, window, at).unwrap();
        let (end, cost) = picture.sent().unwrap();
        picture.receive(&stream, 1, end);
        picture.set_rate(LinkRate {
            bits_per_second: 250_000,
            burst_bytes: 65_536,
        });
        assert_eq!(picture.in_flight(), 0);
        assert!(picture.ready());
        assert!(cost > 0);
    }

    /// Framing fixture only. The archive's outer u32 message length preserves
    /// WS message boundaries; it is not part of the media protocol. Immediate
    /// receipts here produce no network/pacing/paint performance evidence.
    #[test]
    #[ignore = "canonical fixture: AMBIT_LARGE_NATIVE_KEY and AMBIT_NATIVE_PARTS_OUT"]
    fn export_actual_native_key_as_canonical_parts_without_whole_wire_copy() {
        use std::io::Write;
        let data = std::fs::read(std::env::var("AMBIT_LARGE_NATIVE_KEY").unwrap()).unwrap();
        assert_eq!(data.len(), 21_734_926);
        let source = Arc::new(Unit {
            data,
            wire_bytes: 0,
            key: true,
            ts: 1_234_567,
            coded: (4096, 4096),
            visible: Rect {
                x: 0,
                y: 0,
                width: 4096,
                height: 4096,
            },
            surface: Surface::new(4096, 4096),
            input_seq: Some(4411),
            quality: Quality::Motion,
            codec_string: Some("av01.1.16M.08".into()),
        });
        let stream = "00000000-0000-4000-8000-000000000001";
        let mut picture = Parts::new(source, VideoCodec::Av1Full, stream.into(), 1);
        picture.set_rate(LinkRate {
            bits_per_second: 500_000,
            burst_bytes: 65_536,
        });
        let path = std::env::var("AMBIT_NATIVE_PARTS_OUT").unwrap();
        let mut archive = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut fragments = 0;
        let mut first_header = None;
        while !picture.complete() {
            let (quantum, window) = picture.limits();
            let part = picture.prepare(quantum, window, Instant::now()).unwrap();
            if first_header.is_none() {
                let end = 4 + u32::from_be_bytes(part[..4].try_into().unwrap()) as usize;
                first_header =
                    Some(serde_json::from_slice::<serde_json::Value>(&part[4..end]).unwrap());
            }
            archive
                .write_all(&(part.len() as u32).to_be_bytes())
                .unwrap();
            archive.write_all(&part).unwrap();
            let (end, _) = picture.sent().unwrap();
            assert!(picture.in_flight() <= 3124);
            assert!(picture.receive(stream, 1, end));
            fragments += 1;
        }
        archive.flush().unwrap();
        println!(
            "NATIVE_PARTS_FIXTURE {}",
            serde_json::json!({
                "payloadBytes":21_734_926,"canonicalBytes":picture.costs().0,
                "nativeWsBytes":picture.costs().1,"fragments":fragments,"firstHeader":first_header,
                "metadata":"serializer fixture, not real helper/input/clock provenance",
                "performance":"not measured; serialization only"
            })
        );
    }
}
