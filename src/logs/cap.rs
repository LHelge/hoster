//! The line-rate cap (ADR-0067 §7): a producer that writes more than
//! [`RateCap::LINES_PER_SECOND`] lines in one second has the excess dropped, and each run of
//! dropped lines (a [`Gap`]) becomes one marker record that says how many were lost.

use std::time::{SystemTime, UNIX_EPOCH};

/// Counts the lines of one producer per second of their time and decides which are kept.
///
/// Seconds are those of the lines' own times (podman's stamps), not of when they are read, so a
/// backlog read in a burst after a restart is capped as it was written. A line stamped earlier
/// than the current second counts toward it.
#[derive(Debug, Default)]
pub struct RateCap {
    /// The second being counted, since the epoch.
    second: i64,
    /// Lines kept in it.
    kept: u32,
    /// The lines dropped since the last kept one.
    gap: Option<Gap>,
}

/// A run of dropped lines, ended by the next kept line or by the end of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub dropped: u64,
    /// The time of the last line dropped: a marker is stamped with it, so it sorts inside the
    /// gap, and a cursor set to it never reads the dropped lines again.
    pub last: SystemTime,
}

impl Gap {
    /// The marker record's message.
    pub fn message(&self) -> String {
        let lines = if self.dropped == 1 { "line" } else { "lines" };
        format!(
            "{} {lines} dropped: more than {} lines a second",
            self.dropped,
            RateCap::LINES_PER_SECOND
        )
    }
}

/// What [`RateCap::admit`] decided about one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    /// Whether the line is kept.
    pub keep: bool,
    /// The gap this line ends, to mark before the line itself.
    pub ended: Option<Gap>,
}

impl RateCap {
    /// The most lines kept per second (ADR-0067 §7): a hundred times what a busy app prints
    /// while serving, and a small share of the recorder's channel.
    pub const LINES_PER_SECOND: u32 = 1_000;

    /// Counts a line written at `time`.
    pub fn admit(&mut self, time: SystemTime) -> Admission {
        let second = match time.duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            Err(_) => 0,
        };
        if second > self.second {
            self.second = second;
            self.kept = 0;
        }
        if self.kept < Self::LINES_PER_SECOND {
            self.kept += 1;
            Admission {
                keep: true,
                ended: self.gap.take(),
            }
        } else {
            let gap = self.gap.get_or_insert(Gap {
                dropped: 0,
                last: time,
            });
            gap.dropped += 1;
            gap.last = gap.last.max(time);
            Admission {
                keep: false,
                ended: None,
            }
        }
    }

    /// The open gap, when the stream ends inside one.
    pub fn take_gap(&mut self) -> Option<Gap> {
        self.gap.take()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_700_000_000_000 + millis)
    }

    #[test]
    fn the_excess_of_a_second_is_dropped_and_marked_once() {
        let mut cap = RateCap::default();
        for _ in 0..RateCap::LINES_PER_SECOND {
            assert_eq!(
                cap.admit(at(10)),
                Admission {
                    keep: true,
                    ended: None
                }
            );
        }
        for i in 0..5 {
            assert!(!cap.admit(at(20 + i)).keep);
        }
        // A line of an earlier time counts toward the current second.
        assert!(!cap.admit(at(5)).keep);
        let next = cap.admit(at(1_000));
        assert!(next.keep);
        let gap = next.ended.expect("the gap ends with the next kept line");
        assert_eq!(gap.dropped, 6);
        assert_eq!(gap.last, at(24));
        assert_eq!(
            gap.message(),
            "6 lines dropped: more than 1000 lines a second"
        );
        assert_eq!(cap.take_gap(), None);
    }

    #[test]
    fn a_gap_open_at_the_end_is_taken() {
        let mut cap = RateCap::default();
        for _ in 0..=RateCap::LINES_PER_SECOND {
            cap.admit(at(0));
        }
        let gap = cap.take_gap().unwrap();
        assert_eq!(gap.dropped, 1);
        assert_eq!(
            gap.message(),
            "1 line dropped: more than 1000 lines a second"
        );
        assert_eq!(cap.take_gap(), None);
    }
}
