//! Request latency: a log-linear [`Histogram`] that merges and encodes for storage (ADR-0062),
//! and an [`AtomicHistogram`] the proxy's threads record into lock-free.
//!
//! A histogram has four buckets per power of two of microseconds, so a percentile is reported
//! to within 25 %. Histograms merge (`AddAssign`, `Sum`) into the histogram of all their
//! samples, so the latencies of several routes or buckets add up to the percentiles of all
//! their requests together: a stack's, a bar's or the whole proxy's. Its bucket layout is a
//! persisted format: traffic history stores histograms as the versioned bytes of
//! [`Histogram`]'s `From`/`TryFrom` conversions.
//!
//! A route's [`AtomicHistogram`] is cumulative and never cleared, so it loses nothing; the
//! traffic recorder takes the samples since its previous drain with [`Histogram::since`].

use std::array;
use std::iter::Sum;
use std::ops::AddAssign;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Sub-buckets per power of two.
///
/// Part of the persisted histogram format (ADR-0062): changing it, or [`BUCKETS`], or how
/// [`Bucket::of`] maps latencies, needs a new [`Histogram::VERSION`] that still decodes the old
/// one.
const SUB: usize = 4;
/// Buckets per histogram: exact below 4 µs, then four per octave up to 2^28 µs (about 4.5 min).
/// Anything longer lands in the last one. Part of the persisted format, as [`SUB`] is.
const BUCKETS: usize = SUB + 26 * SUB;

/// Request latencies, counted per log-linear bucket. Histograms merge (`AddAssign`, `Sum`) into
/// the histogram of all their samples, and convert to and from a compact versioned encoding
/// for storage: `Vec::<u8>::from(&histogram)` and `Histogram::try_from(bytes)`.
///
/// The encoding (ADR-0062) is the version byte ([`Self::VERSION`]), then for each non-empty
/// bucket in ascending order its index (one byte) and its count (unsigned LEB128, not zero). An
/// empty histogram is the version byte alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Histogram([u64; BUCKETS]);

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
    /// The version of the encoding this build writes, and the only one it reads.
    pub const VERSION: u8 = 1;

    /// Counts one request that took `latency`.
    pub fn record(&mut self, latency: Duration) {
        let count = &mut self.0[Bucket::of(latency).0];
        *count = count.saturating_add(1);
    }

    /// How many requests the histogram counts.
    pub fn samples(&self) -> u64 {
        self.0
            .iter()
            .fold(0, |sum, count| sum.saturating_add(*count))
    }

    /// Whether the histogram counts no request.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|count| *count == 0)
    }

    /// The median, to within 25 %; `None` without samples.
    pub fn p50(&self) -> Option<Duration> {
        self.percentile(50)
    }

    /// The 95th percentile, to within 25 %; `None` without samples.
    pub fn p95(&self) -> Option<Duration> {
        self.percentile(95)
    }

    /// The samples counted here but not in `earlier`, an earlier reading of the same
    /// cumulative counts.
    pub(super) fn since(&self, earlier: &Histogram) -> Histogram {
        Histogram(array::from_fn(|i| self.0[i].wrapping_sub(earlier.0[i])))
    }

    /// The upper bound of the bucket holding the `percent`-th percentile sample.
    fn percentile(&self, percent: u64) -> Option<Duration> {
        let samples = self.samples();
        if samples == 0 {
            return None;
        }
        let rank = u64::try_from(u128::from(samples) * u128::from(percent))
            .unwrap_or(u64::MAX)
            .div_ceil(100)
            .max(1);
        let mut seen = 0u64;
        self.0.iter().enumerate().find_map(|(index, count)| {
            seen = seen.saturating_add(*count);
            (seen >= rank).then(|| Bucket(index).upper())
        })
    }
}

impl AddAssign<&Histogram> for Histogram {
    fn add_assign(&mut self, other: &Histogram) {
        for (sum, count) in self.0.iter_mut().zip(&other.0) {
            *sum = sum.saturating_add(*count);
        }
    }
}

impl AddAssign for Histogram {
    fn add_assign(&mut self, other: Histogram) {
        *self += &other;
    }
}

impl<'a> Sum<&'a Histogram> for Histogram {
    fn sum<I: Iterator<Item = &'a Histogram>>(histograms: I) -> Self {
        histograms.fold(Histogram::default(), |mut total, histogram| {
            total += histogram;
            total
        })
    }
}

impl Sum for Histogram {
    fn sum<I: Iterator<Item = Histogram>>(histograms: I) -> Self {
        histograms.fold(Histogram::default(), |mut total, histogram| {
            total += histogram;
            total
        })
    }
}

/// The encoding of [`Histogram`]: the version byte, then (index, LEB128 count) per non-empty
/// bucket.
impl From<&Histogram> for Vec<u8> {
    fn from(histogram: &Histogram) -> Self {
        let mut bytes = vec![Histogram::VERSION];
        for (index, &count) in histogram.0.iter().enumerate() {
            if count == 0 {
                continue;
            }
            // BUCKETS is below 256, so every index fits a byte.
            bytes.push(index as u8);
            let mut rest = count;
            loop {
                let byte = (rest & 0x7f) as u8;
                rest >>= 7;
                if rest == 0 {
                    bytes.push(byte);
                    break;
                }
                bytes.push(byte | 0x80);
            }
        }
        bytes
    }
}

impl From<Histogram> for Vec<u8> {
    fn from(histogram: Histogram) -> Self {
        Vec::from(&histogram)
    }
}

/// Decodes the encoding written by `From<&Histogram> for Vec<u8>`. Anything else (another
/// version, a bucket out of range or out of order, a missing, zero or oversized count) is an
/// error.
impl TryFrom<&[u8]> for Histogram {
    type Error = HistogramDecodeError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let (&version, mut rest) = bytes.split_first().ok_or(HistogramDecodeError::Empty)?;
        if version != Histogram::VERSION {
            return Err(HistogramDecodeError::UnknownVersion(version));
        }
        let mut histogram = Histogram::default();
        let mut next = 0usize;
        let mut samples = 0u64;
        while let Some((&index, after)) = rest.split_first() {
            let bucket = usize::from(index);
            if bucket >= BUCKETS {
                return Err(HistogramDecodeError::BucketOutOfRange(index));
            }
            if bucket < next {
                return Err(HistogramDecodeError::Unordered(index));
            }
            let (count, after) = Self::leb128(after)
                .filter(|(count, _)| *count > 0)
                .ok_or(HistogramDecodeError::BadCount(index))?;
            samples = samples
                .checked_add(count)
                .ok_or(HistogramDecodeError::BadCount(index))?;
            histogram.0[bucket] = count;
            next = bucket + 1;
            rest = after;
        }
        Ok(histogram)
    }
}

impl TryFrom<Vec<u8>> for Histogram {
    type Error = HistogramDecodeError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Histogram::try_from(bytes.as_slice())
    }
}

impl Histogram {
    /// An unsigned LEB128 number at the start of `bytes` and what follows it; `None` when it is
    /// cut short or does not fit a `u64`.
    fn leb128(bytes: &[u8]) -> Option<(u64, &[u8])> {
        let mut value = 0u64;
        for (i, &byte) in bytes.iter().enumerate() {
            let shift = 7 * i as u32;
            let bits = u64::from(byte & 0x7f);
            if shift >= 64 || (shift == 63 && bits > 1) {
                return None;
            }
            value |= bits << shift;
            if byte & 0x80 == 0 {
                return Some((value, &bytes[i + 1..]));
            }
        }
        None
    }
}

/// Why bytes are not an encoded [`Histogram`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HistogramDecodeError {
    #[error("the latency histogram has no version byte")]
    Empty,
    #[error("unknown latency histogram version {0}")]
    UnknownVersion(u8),
    #[error("latency histogram bucket {0} is out of range")]
    BucketOutOfRange(u8),
    #[error("latency histogram bucket {0} is out of order")]
    Unordered(u8),
    #[error("latency histogram bucket {0} has a malformed count")]
    BadCount(u8),
}

/// A [`Histogram`] the proxy's threads record into without locking: one atomic add a sample.
#[derive(Debug)]
pub(super) struct AtomicHistogram([AtomicU64; BUCKETS]);

impl Default for AtomicHistogram {
    fn default() -> Self {
        Self(array::from_fn(|_| AtomicU64::new(0)))
    }
}

impl AtomicHistogram {
    /// Counts one request that took `latency`.
    pub(super) fn record(&self, latency: Duration) {
        self.0[Bucket::of(latency).0].fetch_add(1, Ordering::Relaxed);
    }

    /// The counts now.
    pub(super) fn load(&self) -> Histogram {
        Histogram(self.0.each_ref().map(|count| count.load(Ordering::Relaxed)))
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
    fn the_bucket_layout_is_a_persisted_format() {
        // Stored histograms (ADR-0062) depend on these: changing them needs a new VERSION.
        assert_eq!((SUB, BUCKETS, Histogram::VERSION), (4, 108, 1));
        let layout = [
            (0, 0),
            (3, 3),
            (4, 4),
            (7, 7),
            (8, 8),
            (15, 11),
            (16, 12),
            (1_000, 35),
            (65_535, 59),
            (1 << 27, 104),
            ((1 << 28) - 1, 107),
        ];
        for (micros, index) in layout {
            assert_eq!(
                Bucket::of(Duration::from_micros(micros)).0,
                index,
                "{micros} µs"
            );
        }
        let mut histogram = Histogram::default();
        histogram.record(Duration::from_micros(2));
        for _ in 0..300 {
            histogram.record(Duration::from_millis(1));
        }
        histogram.record(Duration::from_secs(3600));
        assert_eq!(
            Vec::from(&histogram),
            [1, 2, 1, 35, 0xac, 0x02, 107, 1],
            "version, then (index, LEB128 count) per non-empty bucket"
        );
    }

    #[test]
    fn histograms_round_trip_through_their_encoding() {
        let empty = Histogram::default();
        assert_eq!(Vec::from(&empty), [Histogram::VERSION]);
        assert_eq!(Histogram::try_from(&[Histogram::VERSION][..]), Ok(empty));

        let mut histogram = Histogram::default();
        for micros in [0, 1, 5, 900, 12_345, 1 << 20] {
            for _ in 0..micros % 7 + 1 {
                histogram.record(Duration::from_micros(micros));
            }
        }
        histogram.0[50] = u64::MAX / 2;
        let bytes = Vec::from(histogram);
        assert_eq!(Histogram::try_from(bytes), Ok(histogram));
    }

    #[test]
    fn corrupt_encodings_are_errors() {
        use HistogramDecodeError::*;
        let cases: [(&[u8], HistogramDecodeError); 9] = [
            (&[], Empty),
            (&[2], UnknownVersion(2)),
            (&[0, 1, 1], UnknownVersion(0)),
            (&[1, 108, 1], BucketOutOfRange(108)),
            (&[1, 255, 1], BucketOutOfRange(255)),
            (&[1, 9, 1, 3, 1], Unordered(3)),
            (&[1, 9, 1, 9, 1], Unordered(9)),
            (&[1, 4], BadCount(4)),
            (&[1, 4, 0], BadCount(4)),
        ];
        for (bytes, error) in cases {
            assert_eq!(Histogram::try_from(bytes), Err(error), "{bytes:?}");
        }
        // A count cut short, one longer than a u64, and two that overflow the total.
        assert_eq!(Histogram::try_from(&[1, 4, 0x80][..]), Err(BadCount(4)));
        let mut too_long = vec![1, 4];
        too_long.extend([0xff; 10]);
        too_long.push(0x01);
        assert_eq!(Histogram::try_from(too_long), Err(BadCount(4)));
        let mut overflow = vec![1, 4];
        overflow.extend([0xff; 9]);
        overflow.push(0x02);
        assert_eq!(Histogram::try_from(overflow), Err(BadCount(4)));
        let mut max = Histogram::default();
        max.0[4] = u64::MAX;
        let mut sum = Vec::from(&max);
        sum.extend([5, 1]);
        assert_eq!(Histogram::try_from(sum), Err(BadCount(5)));
    }

    #[test]
    fn merged_histograms_have_the_percentiles_of_all_their_samples() {
        let samples: Vec<Duration> = (0..1000u64)
            .map(|i| Duration::from_micros(i * i % 9_973 * 37 + 1))
            .collect();
        let mut all = Histogram::default();
        let mut parts = [Histogram::default(); 3];
        for (i, &latency) in samples.iter().enumerate() {
            all.record(latency);
            parts[i % 3].record(latency);
        }
        let merged: Histogram = parts.iter().sum();
        assert_eq!(merged, all);
        assert_eq!(parts.into_iter().sum::<Histogram>(), all);
        let mut added = parts[0];
        added += parts[1];
        added += &parts[2];
        assert_eq!(added, all);
        assert_eq!((merged.p50(), merged.p95()), (all.p50(), all.p95()));
        assert_eq!(merged.samples(), 1000);

        // And they bound the exact percentiles of the samples, to within 25 %.
        let mut sorted = samples.clone();
        sorted.sort();
        for (percentile, exact) in [(merged.p50(), sorted[499]), (merged.p95(), sorted[949])] {
            let percentile = percentile.unwrap();
            assert!(percentile > exact, "{percentile:?} {exact:?}");
            assert!(percentile <= exact * 5 / 4 + Duration::from_micros(1));
        }
        assert!(Histogram::default().is_empty());
        assert_eq!(Histogram::default().p50(), None);
    }

    #[test]
    fn an_atomic_histogram_counts_every_concurrent_sample() {
        let histogram = std::sync::Arc::new(AtomicHistogram::default());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let histogram = std::sync::Arc::clone(&histogram);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        histogram.record(3 * MS);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let loaded = histogram.load();
        assert_eq!(loaded.samples(), 4000);
        assert_eq!(loaded.since(&loaded), Histogram::default());
    }

    #[test]
    fn p95_sees_a_slow_tail_that_p50_does_not() {
        let mut histogram = Histogram::default();
        for _ in 0..90 {
            histogram.record(2 * MS);
        }
        for _ in 0..10 {
            histogram.record(800 * MS);
        }
        assert!(histogram.p50().unwrap() < 3 * MS, "{histogram:?}");
        assert!(histogram.p95().unwrap() >= 800 * MS, "{histogram:?}");
    }
}
