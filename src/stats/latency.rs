//! Request latency over a rolling window, recorded lock-free from the proxy's threads.
//!
//! The window is a ring of [`LatencyWindow::SLOTS`] time slots of [`LatencyWindow::SLOT`] each.
//! A slot holds a log-linear histogram (four buckets per power of two of microseconds, so a
//! percentile is reported to within 25 %); the first sample of a new slot period clears the slot
//! it reuses. Reading sums the slots of the last [`LatencyWindow::WINDOW`].
//!
//! A [`Latency`] keeps the summed histogram beside its percentiles, so the latencies of several
//! routes add up (`Sum`) to the percentiles of all their requests together: a stack's, or the
//! whole proxy's.
//!
//! Recording and clearing race benignly: a sample added while another thread clears its slot
//! may be lost. That is fine for a dashboard and keeps the hot path to two atomic adds.

use std::array;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Sub-buckets per power of two.
const SUB: usize = 4;
/// Buckets per slot: exact below 4 µs, then four per octave up to 2^28 µs (about 4.5 min).
/// Anything longer lands in the last one.
const BUCKETS: usize = SUB + 26 * SUB;

/// Latency samples of the recent past.
#[derive(Debug)]
pub(super) struct LatencyWindow {
    slots: [Slot; LatencyWindow::SLOTS],
}

#[derive(Debug)]
struct Slot {
    /// The slot period (time since the base divided by [`LatencyWindow::SLOT`]) plus one that
    /// the buckets count; 0 for a slot never used.
    period: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

/// p50 and p95 of the requests of the last [`LatencyWindow::WINDOW`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Latency {
    /// How many requests the percentiles are taken over.
    pub samples: u64,
    /// `None` without samples.
    pub p50: Option<Duration>,
    pub p95: Option<Duration>,
    /// The samples behind the percentiles, for adding latencies up.
    histogram: Histogram,
}

/// Samples per bucket.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Histogram([u64; BUCKETS]);

impl Default for Histogram {
    fn default() -> Self {
        Histogram([0; BUCKETS])
    }
}

/// Only the buckets that hold samples, by index.
impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.iter().enumerate().filter(|(_, count)| **count > 0))
            .finish()
    }
}

impl Histogram {
    /// The upper bound of the bucket holding the `percent`-th percentile sample.
    fn percentile(&self, samples: u64, percent: u64) -> Option<Duration> {
        if samples == 0 {
            return None;
        }
        let rank = (samples * percent).div_ceil(100).max(1);
        let mut seen = 0;
        self.0.iter().enumerate().find_map(|(index, count)| {
            seen += count;
            (seen >= rank).then(|| Bucket(index).upper())
        })
    }
}

/// The percentiles of a histogram.
impl From<Histogram> for Latency {
    fn from(histogram: Histogram) -> Self {
        let samples = histogram.0.iter().sum();
        Latency {
            samples,
            p50: histogram.percentile(samples, 50),
            p95: histogram.percentile(samples, 95),
            histogram,
        }
    }
}

/// The latency of all the requests together: their histograms summed, the percentiles taken
/// again.
impl<'a> std::iter::Sum<&'a Latency> for Latency {
    fn sum<I: Iterator<Item = &'a Latency>>(latencies: I) -> Self {
        let mut total = Histogram::default();
        for latency in latencies {
            for (sum, count) in total.0.iter_mut().zip(&latency.histogram.0) {
                *sum += count;
            }
        }
        Latency::from(total)
    }
}

impl Default for LatencyWindow {
    fn default() -> Self {
        Self {
            slots: array::from_fn(|_| Slot {
                period: AtomicU64::new(0),
                buckets: array::from_fn(|_| AtomicU64::new(0)),
            }),
        }
    }
}

impl LatencyWindow {
    /// The length of one slot.
    pub(super) const SLOT: Duration = Duration::from_secs(30);
    /// How many slots the ring holds.
    pub(super) const SLOTS: usize = 10;
    /// How far back the percentiles look: up to this long (the current slot is partial).
    pub(super) const WINDOW: Duration =
        Duration::from_secs(Self::SLOT.as_secs() * Self::SLOTS as u64);

    /// Records a request that took `latency`, finished `at` after the base instant.
    pub(super) fn record(&self, at: Duration, latency: Duration) {
        let period = Self::period(at);
        let slot = &self.slots[(period % Self::SLOTS as u64) as usize];
        let tag = period + 1;
        let seen = slot.period.load(Ordering::Acquire);
        if seen != tag {
            if seen > tag {
                // A sample from a period the slot has already moved past.
                return;
            }
            if slot
                .period
                .compare_exchange(seen, tag, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                for bucket in &slot.buckets {
                    bucket.store(0, Ordering::Relaxed);
                }
            } else if slot.period.load(Ordering::Acquire) != tag {
                return;
            }
        }
        slot.buckets[Bucket::of(latency).0].fetch_add(1, Ordering::Relaxed);
    }

    /// The percentiles of the samples of the window that ends `at` after the base instant.
    pub(super) fn summary(&self, at: Duration) -> Latency {
        let current = Self::period(at) + 1;
        let mut counts = Histogram::default();
        for slot in &self.slots {
            let tag = slot.period.load(Ordering::Acquire);
            if tag == 0 || tag > current || tag + Self::SLOTS as u64 <= current {
                continue;
            }
            for (sum, bucket) in counts.0.iter_mut().zip(&slot.buckets) {
                *sum += bucket.load(Ordering::Relaxed);
            }
        }
        Latency::from(counts)
    }

    fn period(at: Duration) -> u64 {
        at.as_secs() / Self::SLOT.as_secs()
    }
}

/// A histogram bucket index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bucket(usize);

impl Bucket {
    /// The bucket a latency falls into.
    fn of(latency: Duration) -> Self {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        if micros < SUB as u64 {
            return Bucket(micros as usize);
        }
        // `micros` is at least 4, so the octave is at least 2.
        let octave = 63 - micros.leading_zeros() as usize;
        let sub = ((micros >> (octave - 2)) & (SUB as u64 - 1)) as usize;
        Bucket((SUB + (octave - 2) * SUB + sub).min(BUCKETS - 1))
    }

    /// The smallest latency above every latency in this bucket.
    fn upper(self) -> Duration {
        let micros = if self.0 < SUB {
            self.0 as u64 + 1
        } else {
            let octave = (self.0 - SUB) / SUB + 2;
            let sub = ((self.0 - SUB) % SUB) as u64;
            (SUB as u64 + sub + 1) << (octave - 2)
        };
        Duration::from_micros(micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn buckets_are_ordered_and_bound_their_latencies() {
        let mut last = Bucket(0);
        for micros in [
            0,
            1,
            3,
            4,
            5,
            7,
            8,
            9,
            15,
            16,
            100,
            999,
            1_000,
            65_535,
            1 << 27,
        ] {
            let latency = Duration::from_micros(micros);
            let bucket = Bucket::of(latency);
            assert!(bucket.0 >= last.0, "{micros} µs");
            assert!(
                bucket.upper() > latency,
                "{micros} µs below {:?}",
                bucket.upper()
            );
            // Four buckets per octave: the bound is less than 25 % (or 1 µs) above the value.
            let slack = bucket.upper() - latency;
            assert!(
                slack <= latency / 4 + Duration::from_micros(1),
                "{micros} µs"
            );
            last = bucket;
        }
        assert_eq!(Bucket::of(Duration::from_secs(3600)).0, BUCKETS - 1);
        assert_eq!(Bucket::of(Duration::MAX).0, BUCKETS - 1);
    }

    #[test]
    fn an_empty_window_has_no_percentiles() {
        let window = LatencyWindow::default();
        assert_eq!(window.summary(Duration::ZERO), Latency::default());
    }

    #[test]
    fn percentiles_of_a_uniform_spread() {
        let window = LatencyWindow::default();
        for ms in 1..=100 {
            window.record(Duration::from_secs(1), ms * MS);
        }
        let summary = window.summary(Duration::from_secs(2));
        assert_eq!(summary.samples, 100);
        let p50 = summary.p50.unwrap();
        let p95 = summary.p95.unwrap();
        assert!(p50 > 50 * MS && p50 <= 50 * MS * 5 / 4, "{p50:?}");
        assert!(p95 > 95 * MS && p95 <= 95 * MS * 5 / 4, "{p95:?}");
    }

    #[test]
    fn p95_sees_a_slow_tail_that_p50_does_not() {
        let window = LatencyWindow::default();
        for _ in 0..90 {
            window.record(Duration::ZERO, 2 * MS);
        }
        for _ in 0..10 {
            window.record(Duration::ZERO, 800 * MS);
        }
        let summary = window.summary(Duration::ZERO);
        assert!(summary.p50.unwrap() < 3 * MS, "{summary:?}");
        assert!(summary.p95.unwrap() >= 800 * MS, "{summary:?}");
    }

    #[test]
    fn latencies_add_up_to_the_percentiles_of_all_their_samples() {
        let fast = LatencyWindow::default();
        let slow = LatencyWindow::default();
        for _ in 0..90 {
            fast.record(Duration::ZERO, 2 * MS);
        }
        for _ in 0..10 {
            slow.record(Duration::ZERO, 800 * MS);
        }
        let parts = [fast.summary(Duration::ZERO), slow.summary(Duration::ZERO)];
        let all: Latency = parts.iter().sum();
        assert_eq!(all.samples, 100);
        // Neither route alone has these: fast's p95 is 2 ms, slow's p50 800 ms.
        assert!(all.p50.unwrap() < 3 * MS, "{all:?}");
        assert!(all.p95.unwrap() >= 800 * MS, "{all:?}");
        assert_eq!(
            std::iter::empty::<&Latency>().sum::<Latency>(),
            Latency::default()
        );
    }

    #[test]
    fn samples_spread_over_the_window_count_until_they_age_out() {
        let window = LatencyWindow::default();
        let slot = LatencyWindow::SLOT;
        for i in 0..LatencyWindow::SLOTS as u32 {
            window.record(slot * i, 10 * MS);
        }
        let end = slot * (LatencyWindow::SLOTS as u32 - 1);
        assert_eq!(window.summary(end).samples, 10);
        // One slot later the oldest has aged out; the window is never longer than WINDOW.
        assert_eq!(window.summary(end + slot).samples, 9);
        assert_eq!(window.summary(end + LatencyWindow::WINDOW).samples, 0);
    }

    #[test]
    fn a_reused_slot_forgets_its_old_samples() {
        let window = LatencyWindow::default();
        window.record(Duration::ZERO, 900 * MS);
        // Same slot of the ring, one whole window later.
        let later = LatencyWindow::WINDOW + Duration::from_secs(1);
        window.record(later, MS);
        let summary = window.summary(later);
        assert_eq!(summary.samples, 1);
        assert!(summary.p95.unwrap() <= 2 * MS, "{summary:?}");
        // A late sample of the old period is dropped instead of reviving it.
        window.record(Duration::ZERO, 900 * MS);
        assert_eq!(window.summary(later).samples, 1);
    }

    #[test]
    fn concurrent_recording_counts_every_sample_within_a_slot() {
        let window = std::sync::Arc::new(LatencyWindow::default());
        // The slot is taken over first: a sample racing its clearing may be lost by design.
        window.record(Duration::from_secs(5), 3 * MS);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let window = std::sync::Arc::clone(&window);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        window.record(Duration::from_secs(5), 3 * MS);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(window.summary(Duration::from_secs(5)).samples, 4001);
    }
}
