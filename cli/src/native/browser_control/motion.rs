//! The shape of the agent's visible input: where the pointer is at each
//! moment of a travel, when each key goes, and how a wheel turns.
//!
//! A person watching the browser sees the input the driver really sends, so
//! this is a schedule, never an animation: the native input owner executes it
//! one helper request at a time, under its usual custody, layout proof and
//! interruption rules (`mouse.rs`, `mouse_scroll.rs`,
//! `BrowserControl::agent_native_keys`).

use std::ops::Range;
use std::time::{Duration, Instant};

use serde_json::Value;

/// One presenter frame at 60 Hz. Pointer samples and wheel notches go once
/// per frame. A viewer that negotiates 120 Hz does not change it yet.
pub(crate) const FRAME: Duration = Duration::from_micros(16_667);

/// A pointer at most this far from where an event lands, in CSS pixels, is
/// already there: the event goes without travel.
pub(crate) const REACHED_CSS_PX: f64 = 3.0;

/// The time between two keys, far faster than a person types.
pub(crate) const KEY_INTERVAL: Duration = Duration::from_millis(25);

/// A fill value longer than this many characters arrives as one visible
/// paste, the way a person enters it, instead of key by key.
pub(crate) const PASTE_ABOVE_CHARS: usize = 64;

/// One wheel notch in the helper's unit: it turns the X wheel one click per
/// 100 of delta and keeps any remainder for the wheel's next event.
pub(crate) const NOTCH_DELTA: f64 = 100.0;

/// The most delta one wheel may carry on an axis: the helper's own bound.
const MOST_WHEEL_DELTA: f64 = 32768.0;

/// How long a scroll's wheel may leave its scroller unmoved before the
/// scroll goes by script instead.
pub(crate) const WHEEL_STALL: Duration = Duration::from_millis(400);

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

/// When the next key may go: one `interval` after the previous key was
/// sent, or now when that moment has passed. A late key never makes the
/// following ones hurry.
pub(crate) fn key_due(previous: Option<Instant>, now: Instant, interval: Duration) -> Instant {
    previous.map_or(now, |previous| (previous + interval).max(now))
}

/// A run of keyboard events the helper applies in one request: one key
/// press with the events that follow it up to the next press, or one
/// paste. A paced stroke waits for its turn; `characters` is how much of
/// the requested text it enters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stroke {
    pub events: Range<usize>,
    pub paced: bool,
    pub characters: usize,
}

const MODIFIER_KEYS: [&str; 4] = ["Shift", "Control", "Alt", "Meta"];

/// Whether a keyboard event is a stroke's own press: any key other than a
/// modifier going down, or a paste. Releases and modifiers ride with it.
fn presses(event: &Value) -> bool {
    match event["eventType"].as_str() {
        Some("insertText") => true,
        Some("keyDown" | "rawKeyDown" | "char") => !event["key"]
            .as_str()
            .is_some_and(|key| MODIFIER_KEYS.contains(&key)),
        _ => false,
    }
}

/// The text a keyboard event enters, in characters.
fn entered(event: &Value) -> usize {
    match event["eventType"].as_str() {
        Some("insertText" | "keyDown" | "rawKeyDown" | "char") => event["text"]
            .as_str()
            .map_or(0, |text| text.chars().count()),
        _ => 0,
    }
}

/// A wheel of `delta` per axis as the helper events that turn it, one per
/// frame: each carries at most one notch per axis, and together they carry
/// exactly `delta`. A wheel with no delta is one event. `None` when a delta
/// is not finite or is beyond the helper's bound.
pub(crate) fn notches(delta: (f64, f64)) -> Option<Vec<(f64, f64)>> {
    let axis = |delta: f64| {
        (delta.is_finite() && delta.abs() <= MOST_WHEEL_DELTA).then(|| {
            let notch = NOTCH_DELTA.copysign(delta);
            let whole = (delta.abs() / NOTCH_DELTA).floor() as usize;
            let mut events = vec![notch; whole];
            let rest = delta - notch * whole as f64;
            if rest != 0.0 {
                events.push(rest);
            }
            events
        })
    };
    let (x, y) = (axis(delta.0)?, axis(delta.1)?);
    let count = x.len().max(y.len()).max(1);
    Some(
        (0..count)
            .map(|index| {
                (
                    x.get(index).copied().unwrap_or(0.0),
                    y.get(index).copied().unwrap_or(0.0),
                )
            })
            .collect(),
    )
}

/// How many notches move a scroller on toward its goal, `remaining` CSS
/// pixels away, when a scroll asked it to go `toward` (+1 or -1; 0 on an
/// axis it was not asked to move) and a notch moves it `per_notch`: the
/// nearest whole count, none once it is within half a notch, and none that
/// would take it back.
pub(crate) fn notches_toward(remaining: f64, toward: f64, per_notch: f64) -> u32 {
    let ahead = remaining * toward;
    if !(per_notch.is_finite() && per_notch > 0.0 && ahead.is_finite()) || ahead <= per_notch / 2.0
    {
        return 0;
    }
    (ahead / per_notch).round().min(f64::from(u32::MAX)) as u32
}

/// Splits helper keyboard events into strokes, in order and without gaps.
/// Events before the first press form one unpaced stroke.
pub(crate) fn strokes(events: &[Value]) -> Vec<Stroke> {
    let mut strokes: Vec<Stroke> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let press = presses(event);
        match strokes.last_mut() {
            Some(stroke) if !press => {
                stroke.events.end = index + 1;
                stroke.characters += entered(event);
            }
            _ => strokes.push(Stroke {
                events: index..index + 1,
                paced: press,
                characters: entered(event),
            }),
        }
    }
    strokes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// A wheel turns one notch per event, never a burst: every event carries
    /// at most one notch per axis, in order, and all of them carry exactly
    /// the delta asked for, remainder included.
    #[test]
    fn a_wheel_turns_at_most_one_notch_per_axis_per_event() {
        assert_eq!(
            notches((250.0, -120.0)),
            Some(vec![(100.0, -100.0), (100.0, -20.0), (50.0, 0.0)])
        );
        assert_eq!(notches((0.0, 300.0)), Some(vec![(0.0, 100.0); 3]));
        assert_eq!(notches((0.0, 40.0)), Some(vec![(0.0, 40.0)]));
        // A wheel with no delta still goes, as one event.
        assert_eq!(notches((0.0, 0.0)), Some(vec![(0.0, 0.0)]));
        assert_eq!(notches((-0.0, 0.0)), Some(vec![(0.0, 0.0)]));
        let longest = notches((0.0, -32768.0)).unwrap();
        assert_eq!(longest.len(), 328);
        assert!(longest.iter().all(|(x, y)| *x == 0.0 && y.abs() <= 100.0));
        assert_eq!(longest.iter().map(|(_, y)| y).sum::<f64>(), -32768.0);
        // The helper's bound and non-finite deltas refuse the wheel.
        assert_eq!(notches((0.0, 32768.5)), None);
        assert_eq!(notches((f64::NAN, 100.0)), None);
        assert_eq!(notches((0.0, f64::INFINITY)), None);
    }

    /// A scroll's notches: the nearest whole count toward its goal, none
    /// within half a notch of it, none backward, none without an estimate.
    #[test]
    fn a_scroll_needs_the_nearest_whole_number_of_notches_toward_its_goal() {
        assert_eq!(notches_toward(300.0, 1.0, 120.0), 3);
        assert_eq!(notches_toward(500.0, 1.0, 120.0), 4);
        assert_eq!(notches_toward(-500.0, -1.0, 120.0), 4);
        assert_eq!(notches_toward(61.0, 1.0, 120.0), 1);
        assert_eq!(notches_toward(60.0, 1.0, 120.0), 0);
        // An overshoot is never scrolled back, and an axis the scroll was
        // not asked to move is left alone.
        assert_eq!(notches_toward(-300.0, 1.0, 120.0), 0);
        assert_eq!(notches_toward(300.0, 0.0, 120.0), 0);
        for per_notch in [0.0, -120.0, f64::NAN, f64::INFINITY] {
            assert_eq!(notches_toward(300.0, 1.0, per_notch), 0);
        }
        assert_eq!(notches_toward(f64::INFINITY, 1.0, 120.0), 0);
    }

    #[test]
    fn keys_go_one_interval_apart_and_a_late_key_never_hurries_the_next() {
        let now = Instant::now();
        assert_eq!(key_due(None, now, KEY_INTERVAL), now);
        let previous = now - Duration::from_millis(10);
        assert_eq!(
            key_due(Some(previous), now, KEY_INTERVAL),
            previous + KEY_INTERVAL
        );
        let long_ago = now - Duration::from_millis(80);
        assert_eq!(key_due(Some(long_ago), now, KEY_INTERVAL), now);
    }

    #[test]
    fn keyboard_events_split_into_strokes_that_count_their_text() {
        let key = |event: &str, key: &str, text: Option<&str>| {
            let mut value = json!({"type":"input_keyboard","eventType":event,"key":key});
            if let Some(text) = text {
                value["text"] = json!(text);
            }
            value
        };
        let events = [
            key("keyDown", "Shift", None),
            key("keyDown", "H", Some("H")),
            key("keyUp", "H", None),
            key("keyUp", "Shift", None),
            key("keyDown", "i", Some("i")),
            key("keyUp", "i", None),
            json!({"type":"input_keyboard","eventType":"insertText","text":"é漢"}),
            key("keyDown", "Enter", None),
            key("keyUp", "Enter", None),
        ];
        assert_eq!(
            strokes(&events),
            [
                Stroke {
                    events: 0..1,
                    paced: false,
                    characters: 0
                },
                Stroke {
                    events: 1..4,
                    paced: true,
                    characters: 1
                },
                Stroke {
                    events: 4..6,
                    paced: true,
                    characters: 1
                },
                Stroke {
                    events: 6..7,
                    paced: true,
                    characters: 2
                },
                Stroke {
                    events: 7..9,
                    paced: true,
                    characters: 0
                },
            ]
        );
        assert!(strokes(&[]).is_empty());
        // A lone release (a program's key up) is one unpaced stroke.
        assert_eq!(
            strokes(&[key("keyUp", "a", None)]),
            [Stroke {
                events: 0..1,
                paced: false,
                characters: 0
            }]
        );
    }
}
