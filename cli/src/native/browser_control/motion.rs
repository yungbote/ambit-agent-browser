//! The shape of the agent's visible input: where the pointer is at each
//! moment of a travel.
//!
//! A person watching the browser sees the input the driver really sends, so
//! this is a schedule, never an animation: the native input owner executes it
//! one helper request at a time, under its usual custody, layout proof and
//! interruption rules (`mouse.rs`, `BrowserControl::agent_native_keys`).

use std::time::Duration;

/// One presenter frame at 60 Hz. Pointer samples and wheel notches go once
/// per frame. A viewer that negotiates 120 Hz does not change it yet.
pub(crate) const FRAME: Duration = Duration::from_micros(16_667);

/// A pointer at most this far from where an event lands, in CSS pixels, is
/// already there: the event goes without travel.
pub(crate) const REACHED_CSS_PX: f64 = 3.0;

// Fitts's law (Shannon form) with a fast person's constants, at 2.5 times
// their speed, clamped so no travel is instant or dawdles.
const FITTS_A_MS: f64 = 50.0;
const FITTS_B_MS: f64 = 100.0;
const SPEEDUP: f64 = 2.5;
const SHORTEST: Duration = Duration::from_millis(60);
const LONGEST: Duration = Duration::from_millis(250);

/// How long a travel of `distance` takes toward a target whose smaller side
/// is `width`, both in the same unit: clamp((50 + 100·log2(D/W + 1)) / 2.5,
/// 60, 250) ms. A target narrower than one unit counts as one unit wide.
pub(crate) fn travel_time(distance: f64, width: f64) -> Duration {
    let difficulty = (distance.max(0.0) / width.max(1.0) + 1.0).log2();
    let millis = (FITTS_A_MS + FITTS_B_MS * difficulty) / SPEEDUP;
    if !millis.is_finite() {
        return LONGEST;
    }
    Duration::from_secs_f64(millis / 1000.0).clamp(SHORTEST, LONGEST)
}

/// The share of a travel covered after `progress` (0 to 1) of its time: the
/// minimum-jerk profile, which leaves and arrives at rest.
fn covered(progress: f64) -> f64 {
    let t = progress.clamp(0.0, 1.0);
    t * t * t * (10.0 + t * (6.0 * t - 15.0))
}

/// One straight pointer travel, in display pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Glide {
    from: (f64, f64),
    to: (f64, f64),
    time: Duration,
}

/// What one sample of a travel sends, and when the next is due on the
/// travel's clock. The last sample lands on the destination and has none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub point: (f64, f64),
    pub next: Option<Duration>,
}

impl Glide {
    /// A travel from `from` to `to` toward a target whose smaller side is
    /// `width`, or `None` when the pointer is within `reach` of `to` already.
    pub(crate) fn new(from: (f64, f64), to: (f64, f64), width: f64, reach: f64) -> Option<Self> {
        let distance = (to.0 - from.0).hypot(to.1 - from.1);
        (distance > reach).then(|| Self {
            from,
            to,
            time: travel_time(distance, width),
        })
    }

    #[cfg(test)]
    pub(crate) fn time(&self) -> Duration {
        self.time
    }

    /// Where the pointer is `elapsed` into the travel.
    pub(crate) fn at(&self, elapsed: Duration) -> (f64, f64) {
        let share = covered(elapsed.as_secs_f64() / self.time.as_secs_f64());
        (
            self.from.0 + (self.to.0 - self.from.0) * share,
            self.from.1 + (self.to.1 - self.from.1) * share,
        )
    }

    /// The sample that runs `elapsed` into the travel's clock. The clock
    /// starts one frame before the first sample, so travel begins at once.
    /// Samples fall on whole frames of that clock; one that runs late goes
    /// where the pointer belongs at the moment it runs, so a stall never
    /// slows the travel down.
    pub(crate) fn sample(&self, elapsed: Duration) -> Sample {
        if elapsed >= self.time {
            return Sample {
                point: self.to,
                next: None,
            };
        }
        let frames = elapsed.as_nanos() / FRAME.as_nanos() + 1;
        Sample {
            point: self.at(elapsed),
            next: Some(FRAME * frames as u32),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1000.0
    }

    #[test]
    fn travel_time_follows_fitts_at_two_and_a_half_times_a_fast_person() {
        // (50 + 100·log2(100/40 + 1)) / 2.5 = 92.294 ms.
        assert!((millis(travel_time(100.0, 40.0)) - 92.294).abs() < 0.001);
        // (50 + 100·log2(500/40 + 1)) / 2.5 = 170.196 ms.
        assert!((millis(travel_time(500.0, 40.0)) - 170.196).abs() < 0.001);
        // A short hop is never instant, a long one never dawdles.
        assert_eq!(travel_time(0.0, 40.0), Duration::from_millis(60));
        assert_eq!(travel_time(10.0, 40.0), Duration::from_millis(60));
        assert_eq!(travel_time(5000.0, 4.0), Duration::from_millis(250));
        // A missing or degenerate width counts as one unit.
        assert_eq!(travel_time(3000.0, 0.0), Duration::from_millis(250));
        assert_eq!(travel_time(3000.0, f64::NAN), travel_time(3000.0, 1.0));
        assert_eq!(travel_time(f64::INFINITY, 20.0), Duration::from_millis(250));
        // Wider targets are faster to reach.
        assert!(travel_time(400.0, 200.0) < travel_time(400.0, 20.0));
    }

    #[test]
    fn a_glide_leaves_and_arrives_at_rest_along_a_straight_line() {
        let glide = Glide::new((100.0, 50.0), (500.0, 350.0), 40.0, 6.0).unwrap();
        let time = glide.time();
        assert_eq!(glide.at(Duration::ZERO), (100.0, 50.0));
        assert_eq!(glide.at(time), (500.0, 350.0));
        let (x, y) = glide.at(time / 2);
        assert!((x - 300.0).abs() < 1e-9 && (y - 200.0).abs() < 1e-9);
        // Every point lies on the segment and progress never goes back.
        let mut previous = 0.0;
        for step in 0..=100 {
            let (x, y) = glide.at(time * step / 100);
            let share = (x - 100.0) / 400.0;
            assert!((y - (50.0 + 300.0 * share)).abs() < 1e-9);
            assert!(share >= previous - 1e-12);
            previous = share;
        }
        // At rest at both ends: the first and last hundredth move far less
        // than a straight-speed travel would.
        let start = glide.at(time / 100).0 - 100.0;
        let end = 500.0 - glide.at(time * 99 / 100).0;
        assert!(start < 0.05 && end < 0.05, "{start} {end}");
    }

    /// The 3 px rule: within reach the pointer is already there.
    #[test]
    fn a_pointer_within_reach_does_not_travel() {
        let reach = REACHED_CSS_PX * 2.0;
        assert!(Glide::new((10.0, 10.0), (16.0, 10.0), 40.0, reach).is_none());
        assert!(Glide::new((10.0, 10.0), (14.0, 14.0), 40.0, reach).is_none());
        assert!(Glide::new((10.0, 10.0), (16.1, 10.0), 40.0, reach).is_some());
        assert!(Glide::new((10.0, 10.0), (10.0, 10.0), 40.0, 0.0).is_none());
    }

    /// Samples fall on whole frames of the travel's clock, the first at
    /// once, and the last lands on the destination.
    #[test]
    fn samples_keep_absolute_frame_deadlines_to_the_destination() {
        let glide = Glide::new((0.0, 0.0), (3000.0, 0.0), 30.0, 6.0).unwrap();
        let mut elapsed = FRAME;
        let mut deadlines = Vec::new();
        let mut points = Vec::new();
        loop {
            let sample = glide.sample(elapsed);
            points.push(sample.point);
            let Some(next) = sample.next else { break };
            assert!(next > elapsed, "deadlines move forward");
            assert_eq!(next.as_nanos() % FRAME.as_nanos(), 0, "on the frame grid");
            deadlines.push(next);
            elapsed = next;
        }
        assert_eq!(*points.last().unwrap(), (3000.0, 0.0));
        let frames = glide.time().as_nanos().div_ceil(FRAME.as_nanos()) as usize;
        assert_eq!(points.len(), frames);
        assert!(points.windows(2).all(|pair| pair[1].0 >= pair[0].0));
        // A 250 ms travel at 60 Hz sends 15 samples.
        assert_eq!(glide.time(), Duration::from_millis(250));
        assert_eq!(points.len(), 15);
    }

    /// A sample that runs late goes where the pointer belongs now, and the
    /// next one is due on the original grid rather than a frame later.
    #[test]
    fn a_late_sample_jumps_to_the_position_for_now() {
        let glide = Glide::new((0.0, 0.0), (600.0, 0.0), 40.0, 6.0).unwrap();
        let late = FRAME * 2 + FRAME / 2;
        let sample = glide.sample(late);
        assert_eq!(sample.point, glide.at(late));
        assert_eq!(sample.next, Some(FRAME * 3));
        // Late past the end: straight to the destination, nothing after it.
        assert_eq!(
            glide.sample(glide.time() + FRAME * 5),
            Sample {
                point: (600.0, 0.0),
                next: None
            }
        );
    }
}
