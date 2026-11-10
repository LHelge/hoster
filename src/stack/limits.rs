//! Per-service resource limits (ADR-0059 §2): `[services.<svc>.limits]` with `memory`, `pids`
//! and `cpus`.

use std::fmt;
use std::str::FromStr;

use super::ByteSize;

/// A service's resource limits. No defaults: an unset limit keeps podman's (no memory or CPU
/// cap, 2048 pids), and an empty table is the same as none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// A hard memory limit with no swap, at least [`Limits::MIN_MEMORY`].
    pub memory: Option<ByteSize>,
    /// The most processes and threads, at least 1.
    pub pids: Option<u32>,
    /// The CPU time, in CPUs.
    pub cpus: Option<Cpus>,
}

impl Limits {
    /// The smallest `memory`, a typo guard: podman itself has no floor.
    pub const MIN_MEMORY: ByteSize = ByteSize::mebibytes(8);

    /// Whether no limit is set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The limits that are set, in the order the table documents them.
    pub fn set(&self) -> impl Iterator<Item = Limit> + use<> {
        [
            self.memory.map(|_| Limit::Memory),
            self.pids.map(|_| Limit::Pids),
            self.cpus.map(|_| Limit::Cpus),
        ]
        .into_iter()
        .flatten()
    }
}

/// One key of the `limits` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Limit {
    Memory,
    Pids,
    Cpus,
}

/// The key as the stack file writes it: `memory`, `pids` or `cpus`.
impl AsRef<str> for Limit {
    fn as_ref(&self) -> &str {
        match self {
            Limit::Memory => "memory",
            Limit::Pids => "pids",
            Limit::Cpus => "cpus",
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

/// A CPU limit in CPUs, such as `1.5`: finite and from 0.01 (the kernel's smallest quota, 1 ms
/// per 100 ms period) to 1024.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cpus(f64);

impl Cpus {
    pub const MIN: f64 = 0.01;
    pub const MAX: f64 = 1024.0;
}

/// Never NaN: [`TryFrom<f64>`] only takes finite values.
impl Eq for Cpus {}

impl TryFrom<f64> for Cpus {
    type Error = String;

    fn try_from(cpus: f64) -> Result<Self, Self::Error> {
        if cpus.is_finite() && (Self::MIN..=Self::MAX).contains(&cpus) {
            Ok(Self(cpus))
        } else {
            Err(format!(
                "invalid cpus {cpus}: must be a number from {} to {}",
                Self::MIN,
                Self::MAX
            ))
        }
    }
}

impl FromStr for Cpus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cpus: f64 = s.parse().map_err(|_| {
            format!(
                "invalid cpus `{}`: expected a number such as 1.5",
                s.escape_debug()
            )
        })?;
        Self::try_from(cpus)
    }
}

impl fmt::Display for Cpus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Cpus> for f64 {
    fn from(cpus: Cpus) -> Self {
        cpus.0
    }
}

/// In nano-CPUs, podman's `NanoCpus`: round(cpus × 10⁹). Create and the config hash both use
/// it, so a value the kernel cannot tell apart from another hashes the same.
impl From<Cpus> for i64 {
    fn from(cpus: Cpus) -> Self {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "at most 1024 × 10⁹, far inside i64"
        )]
        let nano = (cpus.0 * 1e9).round() as i64;
        nano
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpus_are_bounded_and_finite() {
        for ok in [0.01, 0.5, 1.0, 1.5, 1024.0] {
            assert_eq!(f64::from(Cpus::try_from(ok).unwrap()), ok);
        }
        for bad in [0.0, 0.001, 0.009_999, -1.0, 1024.5, f64::NAN, f64::INFINITY] {
            let err = Cpus::try_from(bad).unwrap_err();
            assert!(err.contains("from 0.01 to 1024"), "{err}");
        }
        assert_eq!("1.5".parse::<Cpus>().unwrap().to_string(), "1.5");
        assert_eq!("2".parse::<Cpus>().unwrap().to_string(), "2");
        assert!("1.5 cpus".parse::<Cpus>().is_err());
        assert!("nan".parse::<Cpus>().is_err());
    }

    #[test]
    fn cpus_round_to_nano_cpus() {
        let nano = |cpus: f64| i64::from(Cpus::try_from(cpus).unwrap());
        assert_eq!(nano(1.5), 1_500_000_000);
        assert_eq!(nano(0.01), 10_000_000);
        assert_eq!(nano(1024.0), 1_024_000_000_000);
        // Below a nano-CPU the value rounds; 0.1 + 0.2 is not quite 0.3 in binary.
        assert_eq!(nano(0.1 + 0.2), 300_000_000);
        assert_eq!(nano(0.333_333_333_4), 333_333_333);
        assert_eq!(nano(0.333_333_333_6), 333_333_334);
    }

    #[test]
    fn set_limits_in_table_order() {
        assert!(Limits::default().is_empty());
        assert_eq!(Limits::default().set().count(), 0);
        let limits = Limits {
            memory: Some(Limits::MIN_MEMORY),
            pids: None,
            cpus: Some(Cpus::try_from(1.0).unwrap()),
        };
        assert!(!limits.is_empty());
        assert_eq!(
            limits.set().collect::<Vec<_>>(),
            [Limit::Memory, Limit::Cpus]
        );
        assert_eq!(Limit::Pids.to_string(), "pids");
        assert_eq!(Limits::MIN_MEMORY.to_string(), "8MiB");
    }
}
