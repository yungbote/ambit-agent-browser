//! The producer's decisions that need no thread or socket: the coded size a
//! window is encoded at, when a still picture is refined, and at what rate a
//! viewer is served. Each is a small state machine the producer drives with
//! its own clock.

use serde_json::Value;
use std::time::{Duration, Instant};

/// The edge's current path budget. It changes encoding, never input custody.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinkRate {
    pub bits_per_second: u32,
    pub burst_bytes: u32,
}

impl LinkRate {
    pub(super) fn read(message: &Value, generation: u64, enabled: bool) -> Option<Self> {
        let fields = message.as_object()?;
        if !enabled
            || generation == 0
            || fields.len() != 4
            || message["type"] != "rate"
            || message["generation"].as_u64()? != generation
        {
            return None;
        }
        let bits_per_second = u32::try_from(message["bitsPerSecond"].as_u64()?).ok()?;
        let burst_bytes = u32::try_from(message["burstBytes"].as_u64()?).ok()?;
        (bits_per_second >= 1000 && (1..=12 * 1024 * 1024).contains(&burst_bytes)).then_some(Self {
            bits_per_second,
            burst_bytes,
        })
    }

    pub(super) fn capture_rate(self, requested: u32, picture_bytes: u32) -> u32 {
        let affordable = (self.bits_per_second / 8 / picture_bytes.max(1)).clamp(10, MAX_RATE);
        affordable.min(requested)
    }

    /// Payload target of a key: a quarter second of the path, within its burst after the bounded header.
    pub(super) fn key_bytes(self) -> u32 {
        (self.bits_per_second / 32)
            .min(self.burst_bytes.saturating_sub(4096 + 4))
            .min(4 * 1024 * 1024)
            .max(1)
    }
}

/// The coded size moves in steps of this many device pixels.
const CLASS_STEP: u32 = 256;
/// Headroom a class keeps beyond the window: a dock drag that grows the
/// window by up to one step stays in its class. Within the probed size it
/// never takes the class past it; a window past it is past what the viewer
/// probed anyway.
const HEADROOM: u32 = CLASS_STEP;
/// The size every viewer probed its decoder at (contract section 6).
const PROBED: u32 = 2048;
const LARGEST: u32 = 4096;
/// A class shrinks only after the window has needed one this much smaller
/// for `SHRINK_AFTER`: a drag back and forth never shrinks it.
const SHRINK_MARGIN: u32 = 2 * CLASS_STEP;
const SHRINK_AFTER: Duration = Duration::from_secs(10);

/// The class of one window dimension: rounded up to a step, plus a step of
/// headroom (only up to the probed size while the window fits it).
fn class(window: u32) -> u32 {
    let rounded = window.div_ceil(CLASS_STEP) * CLASS_STEP;
    let roomy = if rounded <= PROBED {
        (rounded + HEADROOM).min(PROBED)
    } else {
        rounded + HEADROOM
    };
    roomy.max(rounded).min(LARGEST)
}

/// The size a stream is encoded at. It grows at once when the window no
/// longer fits (a key unit), holds while the window fits, and shrinks only
/// after the window needed a class at least two steps smaller for ten
/// seconds, so a drag back and forth inside the class costs no key unit.
#[derive(Debug)]
pub(super) struct CodedSize {
    current: Option<(u32, u32)>,
    smaller_since: Option<Instant>,
}

impl CodedSize {
    pub(super) fn new() -> Self {
        Self {
            current: None,
            smaller_since: None,
        }
    }

    /// The coded size for a window of `window` at `now`.
    pub(super) fn fit(&mut self, window: (u32, u32), now: Instant) -> (u32, u32) {
        let wanted = (class(window.0), class(window.1));
        let Some(current) = self.current else {
            self.current = Some(wanted);
            return wanted;
        };
        if window.0 > current.0 || window.1 > current.1 {
            // Grow what no longer fits; keep the other dimension's class.
            let grown = (current.0.max(wanted.0), current.1.max(wanted.1));
            self.current = Some(grown);
            self.smaller_since = None;
            return grown;
        }
        let smaller =
            wanted.0 + SHRINK_MARGIN <= current.0 || wanted.1 + SHRINK_MARGIN <= current.1;
        if !smaller {
            self.smaller_since = None;
            return current;
        }
        let since = *self.smaller_since.get_or_insert(now);
        if now.duration_since(since) < SHRINK_AFTER {
            return current;
        }
        self.smaller_since = None;
        self.current = Some(wanted);
        wanted
    }
}

/// How good one encoded picture is (the unit's `quality`). The producer
/// refines a still picture in one step: measured on the node, a direct
/// re-encode at the still target ends sooner and costs fewer bytes than
/// stopping at an intermediate quantizer, so `refine` is never produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quality {
    /// Encoded under the motion budget.
    Motion,
    /// A re-encode of the unchanged capture at the still target.
    Final,
}

impl Quality {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Final => "final",
        }
    }

    /// The quantizer (0-63) of each, measured on the production node
    /// (media-producer/encoder-decision.md): 32 keeps scrolled text at about
    /// 5 KiB a picture; 8 reaches 49.7 dB RGB PSNR on dense text at 4:4:4,
    /// the still target.
    pub(crate) const fn quantizer(self) -> u8 {
        match self {
            Self::Motion => 32,
            Self::Final => 8,
        }
    }
}

/// How long the screen must stay still after the last picture of motion was
/// read before that picture is refined: longer than a frame at 60 pictures a
/// second, so a scroll or a drag keeps its pictures cheap, and short enough
/// that the refinement of a whole screen of dense text (about 95 ms on the
/// node, media-producer/refinement-speed.md) ends within 150 ms of the last
/// damage.
pub(super) const STILL_AFTER: Duration = Duration::from_millis(30);

/// Whether a stream's current picture still owes its refinement, and since
/// when the screen has been still. A stream starts refined: nothing is owed
/// before the first picture of motion.
#[derive(Debug, Default)]
pub(super) struct Refinement {
    still_since: Option<Instant>,
}

impl Refinement {
    /// A picture of new damage, read from the screen at `read`, was encoded
    /// at motion quality: the screen has been still since it was read, not
    /// since its encode ended.
    pub(super) fn moved(&mut self, read: Instant) {
        self.still_since = Some(read);
    }

    /// When the current picture is due for its refinement, if it owes one:
    /// once the screen and the window's geometry have both held for
    /// `STILL_AFTER`. `geometry` is when the window was last laid out (now
    /// while a layout is in flight), so during a drag, whose layouts replace
    /// each picture's size within a frame or two, no picture is refined; the
    /// last one is, once the drag stops.
    pub(super) fn due(&self, geometry: Instant) -> Option<Instant> {
        self.still_since
            .map(|still| still.max(geometry) + STILL_AFTER)
    }

    /// The refinement was encoded.
    pub(super) fn refined(&mut self) {
        self.still_since = None;
    }
}

/// The most pictures per second any stream is captured at.
pub(super) const MAX_RATE: u32 = 60;

/// A subscriber's rate: what its viewer asked for (0: its display rate,
/// up to the most), bounded by the most.
pub(super) fn rate(requested: u32) -> u32 {
    match requested {
        0 => MAX_RATE,
        requested => requested.min(MAX_RATE),
    }
}

/// The least time between two pictures at `rate`.
pub(super) fn period(rate: u32) -> Duration {
    Duration::from_micros(1_000_000 / u64::from(rate.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_path_rate_is_integral_bounded_and_belongs_to_the_enabled_generation() {
        let valid =
            json!({"type":"rate","generation":3,"bitsPerSecond":5_000_000,"burstBytes":200_000});
        assert_eq!(
            LinkRate::read(&valid, 3, true),
            Some(LinkRate {
                bits_per_second: 5_000_000,
                burst_bytes: 200_000
            })
        );
        assert_eq!(LinkRate::read(&valid, 2, true), None);
        assert_eq!(LinkRate::read(&valid, 3, false), None);
        for (field, value) in [
            ("bitsPerSecond", json!(999)),
            ("bitsPerSecond", json!(-1)),
            ("bitsPerSecond", json!(u64::from(u32::MAX) + 1)),
            ("bitsPerSecond", json!(5_000_000.5)),
            ("burstBytes", json!(0)),
            ("burstBytes", json!(12 * 1024 * 1024 + 1)),
            ("generation", json!(3.5)),
        ] {
            let mut message = valid.clone();
            message[field] = value;
            assert_eq!(LinkRate::read(&message, 3, true), None, "{message}");
        }
        let mut extra = valid.clone();
        extra["enabled"] = json!(true);
        assert_eq!(LinkRate::read(&extra, 3, true), None);
        let rate = LinkRate {
            bits_per_second: 5_000_000,
            burst_bytes: 200_000,
        };
        assert_eq!(rate.capture_rate(60, 25_000), 25);
        assert_eq!(rate.capture_rate(15, 25_000), 15);
        assert_eq!(rate.key_bytes(), 156_250);
    }

    #[test]
    fn a_class_is_a_step_up_with_one_step_of_headroom_inside_the_probed_size() {
        for (window, expected) in [
            (1, 512),
            (988, 1280),
            (1024, 1280),
            (1025, 1536),
            (1588, 2048),
            (1840, 2048),
            (1888, 2048),
            (2048, 2048),
            (2049, 2560),
            (3900, 4096),
            (4096, 4096),
        ] {
            assert_eq!(class(window), expected, "{window}");
        }
    }

    /// The lead's drag: 300 CSS px (600 device px) back and forth from the
    /// default dock, repeatedly. Only the first growth changes the coded
    /// size, then nothing does, and the class never flaps for one window.
    #[test]
    fn a_drag_back_and_forth_grows_once_and_never_changes_again() {
        let origin = Instant::now();
        let mut coded = CodedSize::new();
        let mut changes = Vec::new();
        let mut previous = coded.fit((988, 1888), origin);
        assert_eq!(previous, (1280, 2048));
        for pass in 0..6 {
            for step in 0..=30 {
                let width = if pass % 2 == 0 {
                    988 + step * 20
                } else {
                    1588 - step * 20
                };
                let now = origin + Duration::from_millis(pass * 600 + step * 16);
                let size = coded.fit((width as u32, 1888), now);
                if size != previous {
                    changes.push((pass, step, size));
                    previous = size;
                }
            }
        }
        // 1288 px no longer fits 1280: one step up plus a step of headroom.
        assert_eq!(
            changes,
            vec![(0, 15, (1792, 2048))],
            "one growth, no shrink"
        );
        // The same window always gets the same coded size afterwards.
        for width in [988, 1200, 1588] {
            assert_eq!(
                coded.fit((width, 1888), origin + Duration::from_secs(4)),
                (1792, 2048)
            );
        }
    }

    /// Past the probed size too, a drag of 300 CSS px out and back from the
    /// probe surface (1840 device px wide) grows the class once.
    #[test]
    fn a_drag_past_the_probed_size_grows_once() {
        let origin = Instant::now();
        let mut coded = CodedSize::new();
        assert_eq!(coded.fit((1840, 1888), origin), (2048, 2048));
        let mut sizes = vec![(2048, 2048)];
        for pass in 0..6 {
            for step in 0..=30 {
                let width = if pass % 2 == 0 {
                    1840 + step * 20
                } else {
                    2440 - step * 20
                };
                let now = origin + Duration::from_millis(pass * 600 + step * 16);
                let size = coded.fit((width as u32, 1888), now);
                if sizes.last() != Some(&size) {
                    sizes.push(size);
                }
            }
        }
        assert_eq!(sizes, vec![(2048, 2048), (2560, 2048)]);
    }

    #[test]
    fn a_class_shrinks_only_after_the_window_stayed_two_steps_smaller_for_ten_seconds() {
        let origin = Instant::now();
        let mut coded = CodedSize::new();
        assert_eq!(coded.fit((3000, 1888), origin), (3328, 2048));
        let later = |seconds: u64| origin + Duration::from_secs(seconds);
        // One step smaller: never shrinks.
        assert_eq!(coded.fit((2800, 1888), later(1)), (3328, 2048));
        assert_eq!(coded.fit((2800, 1888), later(30)), (3328, 2048));
        // Two steps smaller, briefly, then back: the clock restarts.
        assert_eq!(coded.fit((1800, 1888), later(31)), (3328, 2048));
        assert_eq!(coded.fit((2900, 1888), later(35)), (3328, 2048));
        assert_eq!(coded.fit((1800, 1888), later(36)), (3328, 2048));
        assert_eq!(coded.fit((1800, 1888), later(45)), (3328, 2048));
        assert_eq!(coded.fit((1800, 1888), later(46)), (2048, 2048));
    }

    #[test]
    fn a_still_picture_is_refined_once_and_new_damage_owes_it_again() {
        let origin = Instant::now();
        let laid_out = origin - Duration::from_secs(1);
        let mut refinement = Refinement::default();
        assert_eq!(refinement.due(laid_out), None, "a stream starts refined");
        refinement.moved(origin);
        let later = origin + Duration::from_millis(16);
        refinement.moved(later);
        assert_eq!(
            refinement.due(laid_out),
            Some(later + STILL_AFTER),
            "the newest motion counts"
        );
        refinement.refined();
        assert_eq!(refinement.due(laid_out), None);
        const {
            assert!(Quality::Motion.quantizer() > Quality::Final.quantizer());
        }
    }

    /// A drag lays the window out again within a frame or two of each
    /// picture: the picture of a size about to be replaced is not refined.
    /// The refinement waits until both the screen and the window's geometry
    /// have held for `STILL_AFTER`, and a layout in flight (the display
    /// reports now) keeps moving it on.
    #[test]
    fn a_refinement_waits_for_the_window_geometry_to_hold_too() {
        let read = Instant::now();
        let ms = Duration::from_millis;
        let mut refinement = Refinement::default();
        refinement.moved(read);
        assert_eq!(
            refinement.due(read - ms(500)),
            Some(read + STILL_AFTER),
            "laid out long before: the picture's read counts"
        );
        assert_eq!(
            refinement.due(read + ms(12)),
            Some(read + ms(12) + STILL_AFTER),
            "laid out after the read: the layout counts"
        );
        let in_flight = read + ms(40);
        assert!(
            refinement.due(in_flight) > Some(in_flight),
            "a layout in flight is never waited out"
        );
    }

    #[test]
    fn a_rate_is_the_viewers_own_up_to_sixty() {
        assert_eq!(rate(0), 60);
        assert_eq!(rate(30), 30);
        assert_eq!(rate(120), 60);
        assert_eq!(period(60), Duration::from_micros(16_666));
    }
}
