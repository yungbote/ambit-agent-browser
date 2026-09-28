//! How long the agent's input was paced: the time its glides, wheel notches
//! and key cadence took, which the agent channel reports per step as
//! `motionUs`. Paced input marks itself with a `Span` for as long as it runs;
//! whoever wants the time runs the work inside `measured`. Spans nest (a
//! wheel's travel to where it turns runs inside the wheel's own span) and
//! count once, and a span outside any measurement costs nothing.

use std::cell::Cell;
use std::future::Future;
use std::time::{Duration, Instant};

tokio::task_local! {
    static PACED: Paced;
}

/// The paced time of one measurement: the union of its spans.
#[derive(Default)]
struct Paced {
    /// Spans open now.
    open: Cell<u32>,
    /// When the outermost open span began.
    since: Cell<Option<Instant>>,
    total: Cell<Duration>,
}

impl Paced {
    fn begin(&self, now: Instant) {
        if self.open.get() == 0 {
            self.since.set(Some(now));
        }
        self.open.set(self.open.get() + 1);
    }

    fn end(&self, now: Instant) {
        let open = self.open.get().saturating_sub(1);
        self.open.set(open);
        if open == 0 {
            if let Some(since) = self.since.take() {
                self.total
                    .set(self.total.get() + now.saturating_duration_since(since));
            }
        }
    }
}

/// Runs `work`, and says how long input inside it was paced.
pub(crate) async fn measured<T>(work: impl Future<Output = T>) -> (T, Duration) {
    PACED
        .scope(Paced::default(), async move {
            let output = work.await;
            (output, PACED.with(|paced| paced.total.get()))
        })
        .await
}

/// Paced input under way, from `begin` until dropped.
pub(crate) struct Span {
    measured: bool,
}

impl Span {
    pub(crate) fn begin() -> Self {
        let measured = PACED.try_with(|paced| paced.begin(Instant::now())).is_ok();
        Self { measured }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if self.measured {
            let _ = PACED.try_with(|paced| paced.end(Instant::now()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paced_time_is_the_union_of_the_spans() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let paced = Paced::default();
        // A wheel from 5 to 45 ms, with its travel from 15 to 35 ms inside it.
        paced.begin(at(5));
        paced.begin(at(15));
        paced.end(at(35));
        paced.end(at(45));
        // Keys from 52 to 55 ms.
        paced.begin(at(52));
        paced.end(at(55));
        assert_eq!(paced.total.get(), Duration::from_millis(43));
    }

    #[tokio::test]
    async fn a_measurement_counts_only_the_spans_inside_it() {
        {
            let _unmeasured = Span::begin();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let ((), idle) = measured(tokio::time::sleep(Duration::from_millis(5))).await;
        assert_eq!(idle, Duration::ZERO);
        let ((), paced) = measured(async {
            let _span = Span::begin();
            tokio::time::sleep(Duration::from_millis(5)).await;
        })
        .await;
        assert!(paced >= Duration::from_millis(5), "{paced:?}");
    }
}
