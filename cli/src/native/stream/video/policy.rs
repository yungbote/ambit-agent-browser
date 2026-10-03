//! The producer's decisions that need no thread or socket: the coded size a
//! window is encoded at, when a still picture is refined, and at what rate a
//! viewer is served. Each is a small state machine the producer drives with
//! its own clock.

use crate::native::video::EncoderRegion;
use serde_json::Value;
use std::collections::VecDeque;
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
    /// A dependent region of an unchanged capture, before all regions finish.
    Refine,
    /// A re-encode of the unchanged capture at the still target.
    Final,
}

impl Quality {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Refine => "refine",
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
            Self::Refine | Self::Final => 8,
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

/// The most rows of the window a change may cover and still leave as an
/// exact unit (contract browser-presentation-units): a typed key, a caret or
/// a hover changes fewer. Agreed with fe-browser-media until their decode and
/// composite are measured; a 96-row band of the window codes in about 2 ms.
pub(super) const EXACT_ROWS: u32 = 128;

/// One refinement quantum of byte credit. An indivisible update can cost
/// more than a quantum; its complete cost becomes debt, never an increased
/// allowance or an excuse to queue further updates.
#[derive(Debug, Default)]
pub(super) struct RefinementCredit {
    accounted: Option<Instant>,
    credit: f64,
    rate: Option<u32>,
}

/// Finite coverage of the currently held picture. The encoder returns its
/// actual region, so codec alignment cannot create missing or repeated work.
pub(super) struct RefinementSweep {
    regions: VecDeque<EncoderRegion>,
}

impl RefinementSweep {
    pub(super) fn new(visible: EncoderRegion, edge: u32, pointer: Option<(i32, i32)>) -> Self {
        let edge = edge.max(32);
        let mut regions = VecDeque::new();
        for y in (0..visible.height).step_by(edge as usize) {
            for x in (0..visible.width).step_by(edge as usize) {
                regions.push_back(EncoderRegion {
                    x: visible.x + x,
                    y: visible.y + y,
                    width: edge.min(visible.width - x),
                    height: edge.min(visible.height - y),
                });
            }
        }
        if let Some((x, y)) =
            pointer.and_then(|(x, y)| Some((u32::try_from(x).ok()?, u32::try_from(y).ok()?)))
        {
            if let Some(index) = regions.iter().position(|region| {
                x >= region.x
                    && x < region.x + region.width
                    && y >= region.y
                    && y < region.y + region.height
            }) {
                let first = regions.remove(index).unwrap();
                regions.push_front(first);
            }
        }
        Self { regions }
    }

    pub(super) fn next(&mut self) -> Option<EncoderRegion> {
        self.regions.pop_front()
    }

    pub(super) fn covered(&mut self, actual: EncoderRegion) -> bool {
        self.regions.retain(|region| {
            !(region.x >= actual.x
                && region.y >= actual.y
                && region.x + region.width <= actual.x + actual.width
                && region.y + region.height <= actual.y + actual.height)
        });
        self.regions.is_empty()
    }
}

impl RefinementCredit {
    fn accrue(&mut self, rate: u32, now: Instant) {
        if let Some(at) = self.accounted {
            self.credit += now.saturating_duration_since(at).as_secs_f64()
                * f64::from(self.rate.unwrap_or(rate))
                / 8.0;
        } else {
            self.credit = f64::from(rate) / 320.0;
        }
        self.credit = self.credit.min(f64::from(rate) / 320.0);
        self.accounted = Some(now);
        self.rate = Some(rate);
    }

    pub(super) fn due(&mut self, rate: u32, now: Instant) -> Instant {
        self.accrue(rate, now);
        if self.credit >= 0.0 {
            now
        } else {
            now + Duration::from_secs_f64(-self.credit * 8.0 / f64::from(rate))
        }
    }

    pub(super) fn spent(&mut self, rate: u32, bytes: usize, now: Instant) {
        self.accrue(rate, now);
        self.credit -= bytes as f64;
    }
}

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

/// A stream's rate as a budget of pictures, not a phase: a picture may be
/// taken as soon as the screen changes, so the picture that shows a person's
/// key or click never waits for the next slot of a cadence, while over any
/// longer stretch the stream takes at most one picture a period. Damage that
/// keeps coming (a scroll, a drag) is paced one period apart; after a still
/// moment one picture may follow the last at once. (A virtual scheduling
/// cell rate rule with a tolerance of one period.)
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Pace {
    next: Option<Instant>,
}

impl Pace {
    /// The earliest the next picture may be taken; none before the first.
    pub(super) fn next(&self) -> Option<Instant> {
        self.next
    }

    /// A picture was read from the screen at `read`, at a rate whose period
    /// is `period`.
    pub(super) fn took(&mut self, read: Instant, period: Duration) {
        self.next = Some(self.next.map_or(read, |next| (next + period).max(read)));
    }

    /// A stream whose next picture is due at `at`.
    #[cfg(test)]
    pub(super) fn due_at(at: Instant) -> Self {
        Self { next: Some(at) }
    }
}

/// Framebuffer rows `[top, bottom)` that changed since the encoder last
/// coded them: one band holding every change. A unit codes only these rows,
/// so every other row stays exactly as the encoder last left it (a key's
/// unit of motion never re-blurs a refined page), and a refinement re-codes
/// only them: after typed keys, 24 instead of 29 ms and 47 instead of 592
/// bytes. A unit of motion costs about the same either way; the encoder's
/// cost per picture is its source copy, border extension and frame
/// analysis, not the rows it codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Band {
    pub top: u32,
    pub bottom: u32,
}

impl Band {
    /// Every row of a framebuffer `height` rows tall.
    pub(super) fn whole(height: u32) -> Self {
        Self {
            top: 0,
            bottom: height,
        }
    }

    /// The band holding both.
    pub(super) fn union(self, other: Self) -> Self {
        Self {
            top: self.top.min(other.top),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// What of the window `visible` (framebuffer coordinates, as the coded
    /// picture keeps them) a unit coding this band covers: the window's
    /// columns on the band's rows. None means the whole picture: the band
    /// holds every visible row, or none of them (a change beside the window
    /// is coded whole rather than not at all).
    pub(super) fn within(self, visible: crate::native::display::Rect) -> Option<EncoderRegion> {
        let (x, y) = (
            u32::try_from(visible.x).ok()?,
            u32::try_from(visible.y).ok()?,
        );
        let top = self.top.max(y);
        let bottom = self.bottom.min(y + visible.height);
        (top < bottom && bottom - top < visible.height).then_some(EncoderRegion {
            x,
            y: top,
            width: visible.width,
            height: bottom - top,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key or click after a still moment is captured as soon as it
    /// paints, even right after the last picture; changes that keep coming
    /// are held to one picture a period, and a long still moment earns one
    /// picture of burst, never a run of them.
    #[test]
    fn the_rate_bounds_pictures_without_holding_back_the_first_change() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let period = period(60);
        let mut pace = Pace::default();
        assert_eq!(pace.next(), None, "the first picture is due at once");
        pace.took(start, period);
        assert_eq!(pace.next(), Some(start), "a change right after it too");
        // A scroll: each picture is taken the moment it is allowed.
        let mut read = start + ms(2);
        let mut taken = vec![read];
        pace.took(read, period);
        for _ in 0..58 {
            read = pace.next().unwrap().max(read);
            pace.took(read, period);
            taken.push(read);
        }
        let second = taken
            .iter()
            .filter(|at| **at < start + Duration::from_secs(1))
            .count();
        assert!((59..=61).contains(&second), "{second} pictures in a second");
        for pair in taken[1..].windows(2) {
            assert_eq!(pair[1] - pair[0], period, "steady damage is paced");
        }
        // Still for a second: the next change is taken at once, and so is
        // one more, but a third waits a period.
        let key = read + Duration::from_secs(1);
        assert!(pace.next().unwrap() <= key);
        pace.took(key, period);
        assert_eq!(pace.next(), Some(key));
        pace.took(key + ms(5), period);
        assert_eq!(pace.next(), Some(key + period));
    }

    /// A unit codes the window's columns on the rows that changed; a band
    /// holding every visible row, or none, is the whole picture.
    #[test]
    fn a_band_is_coded_only_where_it_meets_the_window() {
        let visible = crate::native::display::Rect {
            x: 0,
            y: 64,
            width: 1840,
            height: 1000,
        };
        let band = |top, bottom| Band { top, bottom };
        assert_eq!(
            band(100, 148).within(visible),
            Some(EncoderRegion {
                x: 0,
                y: 100,
                width: 1840,
                height: 48
            })
        );
        assert_eq!(
            band(0, 80).within(visible),
            Some(EncoderRegion {
                x: 0,
                y: 64,
                width: 1840,
                height: 16
            }),
            "cut to the window"
        );
        assert_eq!(band(0, 2048).within(visible), None, "every row");
        assert_eq!(band(1064, 1200).within(visible), None, "beside it");
        assert_eq!(band(100, 148).union(band(40, 120)), band(40, 148));
        assert_eq!(Band::whole(1888), band(0, 1888));
    }

    #[test]
    fn refinement_sweep_covers_each_visible_pixel_and_prioritizes_the_native_pointer() {
        for (width, height) in [(1, 1), (33, 17), (137, 93), (1088, 1888)] {
            let mut sweep = RefinementSweep::new(
                EncoderRegion {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                64,
                Some((width as i32 - 1, height as i32 - 1)),
            );
            let mut seen = vec![0u8; (width * height) as usize];
            let mut first = true;
            while let Some(region) = sweep.next() {
                if first {
                    assert!(region.x + region.width == width && region.y + region.height == height);
                    first = false;
                }
                for y in region.y..region.y + region.height {
                    for x in region.x..region.x + region.width {
                        seen[(y * width + x) as usize] += 1;
                    }
                }
                sweep.covered(region);
            }
            assert!(seen.iter().all(|count| *count == 1));
        }
    }

    #[test]
    fn codec_alignment_covers_neighboring_requests_without_changing_the_neutral_order() {
        let mut sweep = RefinementSweep::new(
            EncoderRegion {
                x: 0,
                y: 0,
                width: 128,
                height: 64,
            },
            32,
            Some((-1, 10)),
        );
        assert_eq!(
            sweep.next().unwrap(),
            EncoderRegion {
                x: 0,
                y: 0,
                width: 32,
                height: 32
            }
        );
        assert!(!sweep.covered(EncoderRegion {
            x: 0,
            y: 0,
            width: 64,
            height: 64
        }));
        assert_eq!(
            sweep.next().unwrap(),
            EncoderRegion {
                x: 64,
                y: 0,
                width: 32,
                height: 32
            }
        );
        assert!(sweep.covered(EncoderRegion {
            x: 64,
            y: 0,
            width: 64,
            height: 64
        }));
        assert_eq!(sweep.next(), None);
    }

    #[test]
    fn a_nonzero_visible_origin_and_native_pointer_share_framebuffer_coordinates() {
        // Actual7c helper header: framebuffer1536x2048, visible100,80
        // plus1418x1888, fresh signed pointer400,500; no DPR projection.
        let visible = EncoderRegion {
            x: 100,
            y: 80,
            width: 1418,
            height: 1888,
        };
        for pointer in [
            Some((400, 500)),
            None,
            Some((99, 500)),
            Some((-1, 500)),
            Some((1536, 2048)),
        ] {
            let mut sweep = RefinementSweep::new(visible, 64, pointer);
            let mut seen = vec![0u8; (visible.width * visible.height) as usize];
            let mut first = true;
            while let Some(region) = sweep.next() {
                assert!(region.x >= visible.x && region.y >= visible.y);
                assert!(region.x + region.width <= visible.x + visible.width);
                assert!(region.y + region.height <= visible.y + visible.height);
                if first {
                    if pointer == Some((400, 500)) {
                        assert!((region.x..region.x + region.width).contains(&400));
                        assert!((region.y..region.y + region.height).contains(&500));
                    } else {
                        assert_eq!((region.x, region.y), (visible.x, visible.y));
                    }
                    first = false;
                }
                for y in region.y..region.y + region.height {
                    for x in region.x..region.x + region.width {
                        seen[((y - visible.y) * visible.width + x - visible.x) as usize] += 1;
                    }
                }
                sweep.covered(region);
            }
            assert!(seen.iter().all(|count| *count == 1));
        }
    }

    #[test]
    fn refinement_credit_charges_the_whole_indivisible_unit_and_never_ratchets_its_allowance() {
        let now = Instant::now();
        let mut credit = RefinementCredit::default();
        assert_eq!(credit.due(500_000, now), now);
        credit.spent(500_000, 10_000, now);
        let at = credit.due(500_000, now);
        assert_eq!(at.duration_since(now), Duration::from_micros(135_000));
        assert!(
            credit.due(500_000, now + Duration::from_millis(100))
                > now + Duration::from_millis(100)
        );
        assert_eq!(credit.due(500_000, at), at);
        credit.spent(500_000, 10_000, at);
        assert_eq!(
            credit.due(500_000, at).duration_since(at),
            Duration::from_millis(160)
        );
        // Even a long idle tops up one quantum; it never admits a backlog.
        let later = at + Duration::from_secs(10);
        assert_eq!(credit.due(500_000, later), later);
        assert_eq!(credit.credit, 1562.5);
    }

    #[test]
    fn lowering_a_refinement_rate_does_not_erase_outstanding_byte_debt() {
        let now = Instant::now();
        let mut credit = RefinementCredit::default();
        credit.spent(8_000_000, 50_000, now);
        assert_eq!(
            credit.due(1_000_000, now).duration_since(now),
            Duration::from_millis(200)
        );
        assert_eq!(
            credit
                .due(1_000_000, now + Duration::from_millis(100))
                .duration_since(now),
            Duration::from_millis(200)
        );
    }
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
