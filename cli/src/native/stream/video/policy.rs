//! The producer's decisions that need no thread or socket: the coded size a
//! window is encoded at, when a still picture is refined, and at what rate a
//! viewer is served. Each is a small state machine the producer drives with
//! its own clock.

use std::time::{Duration, Instant};

/// The coded size moves in steps of this many device pixels.
const CLASS_STEP: u32 = 256;
/// Headroom a class keeps beyond the window, up to `PROBED`: a dock drag
/// that grows the window by up to one step stays in its class.
const HEADROOM: u32 = CLASS_STEP;
/// The size every viewer probed its decoder at (contract section 6).
const PROBED: u32 = 2048;
const LARGEST: u32 = 4096;
/// A class shrinks only after the window has needed one this much smaller
/// for `SHRINK_AFTER`: a drag back and forth never shrinks it.
const SHRINK_MARGIN: u32 = 2 * CLASS_STEP;
const SHRINK_AFTER: Duration = Duration::from_secs(10);

/// The class of one window dimension: rounded up to a step, plus a step of
/// headroom while that stays within the probed size.
fn class(window: u32) -> u32 {
    let rounded = window.div_ceil(CLASS_STEP) * CLASS_STEP;
    rounded.max((rounded + HEADROOM).min(PROBED)).min(LARGEST)
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

/// After the last motion picture, how long the screen must stay still
/// before it is refined: long enough that a scroll or a drag at 60 pictures
/// a second keeps its pictures cheap, short enough that the refinement
/// (about 62 ms for a whole screen of dense text on the node) ends within
/// 150 ms of the last damage.
pub(super) const STILL_AFTER: Duration = Duration::from_millis(30);

/// Whether a stream's current picture still owes its refinement, and since
/// when. A stream starts refined: nothing is owed before the first motion
/// picture.
#[derive(Debug, Default)]
pub(super) struct Refinement {
    moved_at: Option<Instant>,
}

impl Refinement {
    /// A picture of new damage was encoded at motion quality.
    pub(super) fn moved(&mut self, now: Instant) {
        self.moved_at = Some(now);
    }

    /// When the current picture is due for its refinement, if it owes one.
    pub(super) fn due(&self) -> Option<Instant> {
        self.moved_at.map(|moved| moved + STILL_AFTER)
    }

    /// The refinement was encoded.
    pub(super) fn refined(&mut self) {
        self.moved_at = None;
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
            (2049, 2304),
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

    #[test]
    fn a_class_shrinks_only_after_the_window_stayed_two_steps_smaller_for_ten_seconds() {
        let origin = Instant::now();
        let mut coded = CodedSize::new();
        assert_eq!(coded.fit((3000, 1888), origin), (3072, 2048));
        let later = |seconds: u64| origin + Duration::from_secs(seconds);
        // One step smaller: never shrinks.
        assert_eq!(coded.fit((2800, 1888), later(1)), (3072, 2048));
        assert_eq!(coded.fit((2800, 1888), later(30)), (3072, 2048));
        // Two steps smaller, briefly, then back: the clock restarts.
        assert_eq!(coded.fit((1800, 1888), later(31)), (3072, 2048));
        assert_eq!(coded.fit((2900, 1888), later(35)), (3072, 2048));
        assert_eq!(coded.fit((1800, 1888), later(36)), (3072, 2048));
        assert_eq!(coded.fit((1800, 1888), later(45)), (3072, 2048));
        assert_eq!(coded.fit((1800, 1888), later(46)), (2048, 2048));
    }

    #[test]
    fn a_still_picture_is_refined_once_and_new_damage_owes_it_again() {
        let origin = Instant::now();
        let mut refinement = Refinement::default();
        assert_eq!(refinement.due(), None, "a stream starts refined");
        refinement.moved(origin);
        let later = origin + Duration::from_millis(16);
        refinement.moved(later);
        assert_eq!(
            refinement.due(),
            Some(later + STILL_AFTER),
            "the newest motion counts"
        );
        refinement.refined();
        assert_eq!(refinement.due(), None);
        const {
            assert!(Quality::Motion.quantizer() > Quality::Final.quantizer());
        }
    }

    #[test]
    fn a_rate_is_the_viewers_own_up_to_sixty() {
        assert_eq!(rate(0), 60);
        assert_eq!(rate(30), 30);
        assert_eq!(rate(120), 60);
        assert_eq!(period(60), Duration::from_micros(16_666));
    }
}
