//! Sizes in a stack file (ADR-0059 §3): `shm_size`, `tmpfs` and, with the limits, `memory`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A number of bytes, written as a whole number and a unit: `B`, `KB`/`MB`/`GB` (10³, 10⁶, 10⁹)
/// or `KiB`/`MiB`/`GiB` (2¹⁰, 2²⁰, 2³⁰), case-sensitive, with no fraction and no space, e.g.
/// `"512MiB"`. At most `i64::MAX` bytes, podman's field type.
///
/// A bare number and Docker's `512m` are errors: the first has no unit, the second is ambiguous
/// between decimal and binary. Any floor (1 MiB for `shm_size` and `tmpfs`) is the field's rule;
/// `"0B"` is a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(u64);

impl ByteSize {
    /// One mebibyte, the smallest `shm_size` or `tmpfs` size.
    pub const MIB: ByteSize = ByteSize::mebibytes(1);
    /// The largest size: podman's size fields are `i64`.
    pub const MAX: ByteSize = ByteSize(i64::MAX as u64);

    /// `n` MiB. `n` is small enough: at most `i64::MAX` bytes.
    pub const fn mebibytes(n: u64) -> Self {
        Self(n << 20)
    }

    /// Each unit and its bytes, in the order [`fmt::Display`] tries them: largest first, and the
    /// binary unit before the decimal one of the same rank.
    const UNITS: [(&str, u64); 7] = [
        ("GiB", 1 << 30),
        ("GB", 1_000_000_000),
        ("MiB", 1 << 20),
        ("MB", 1_000_000),
        ("KiB", 1 << 10),
        ("KB", 1_000),
        ("B", 1),
    ];

    /// The binary unit Docker's single-letter (or `kb`-style) suffix means, for the suggestion.
    fn docker_unit(unit: &str) -> Option<&'static str> {
        match unit.to_ascii_lowercase().as_str() {
            "k" | "kb" | "kib" => Some("KiB"),
            "m" | "mb" | "mib" => Some("MiB"),
            "g" | "gb" | "gib" => Some("GiB"),
            _ => None,
        }
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: String| format!("invalid size `{}`: {reason}", s.escape_debug());
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (number, unit) = s.split_at(end);
        if number.is_empty() {
            return Err(invalid(
                "expected a whole number and a unit, e.g. \"512MiB\"".into(),
            ));
        }
        if unit.is_empty() {
            return Err(invalid(format!(
                "a size needs a unit, e.g. \"{number}MiB\""
            )));
        }
        let Some(&(_, scale)) = Self::UNITS.iter().find(|(name, _)| *name == unit) else {
            let reason = if unit.starts_with(['.', ',']) {
                "a size is a whole number; use a smaller unit, e.g. \"1536MiB\" for 1.5 GiB"
                    .to_owned()
            } else if unit.starts_with(char::is_whitespace) {
                format!(
                    "write the number and the unit without a space, e.g. \"{number}{}\"",
                    unit.trim_start()
                )
            } else if let Some(binary) = Self::docker_unit(unit) {
                let decimal = binary.replace('i', "");
                format!(
                    "units are case-sensitive: write \"{number}{binary}\" (binary, what Docker's \
                     `{unit}` means) or \"{number}{decimal}\" (decimal)"
                )
            } else {
                format!("unknown unit `{unit}`; expected B, KB, MB, GB, KiB, MiB or GiB")
            };
            return Err(invalid(reason));
        };
        number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .filter(|bytes| *bytes <= Self::MAX.0)
            .map(Self)
            .ok_or_else(|| invalid(format!("more than {} bytes", Self::MAX.0)))
    }
}

/// In the largest unit that divides the size exactly, binary first: `"256MiB"`, `"2GB"`,
/// `"1000B"`. Parses back to the same bytes.
impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0B");
        }
        let (unit, scale) = Self::UNITS
            .iter()
            .find(|(_, scale)| self.0.is_multiple_of(*scale))
            .expect("every size is a whole number of bytes");
        write!(f, "{}{unit}", self.0 / scale)
    }
}

/// At most [`ByteSize::MAX`] bytes.
impl TryFrom<u64> for ByteSize {
    type Error = String;

    fn try_from(bytes: u64) -> Result<Self, Self::Error> {
        if bytes <= Self::MAX.0 {
            Ok(Self(bytes))
        } else {
            Err(format!("{bytes} bytes is more than {} bytes", Self::MAX.0))
        }
    }
}

impl From<ByteSize> for u64 {
    fn from(size: ByteSize) -> Self {
        size.0
    }
}

/// Lossless: a size is at most `i64::MAX` bytes.
impl From<ByteSize> for i64 {
    fn from(size: ByteSize) -> Self {
        size.0 as i64
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::ByteSize;

    fn bytes(s: &str) -> u64 {
        u64::from(s.parse::<ByteSize>().unwrap_or_else(|e| panic!("{s}: {e}")))
    }

    #[track_caller]
    fn error(s: &str) -> String {
        s.parse::<ByteSize>()
            .expect_err(&format!("{s} should not parse"))
    }

    #[test]
    fn every_unit_parses() {
        for (text, expected) in [
            ("0B", 0),
            ("1B", 1),
            ("512B", 512),
            ("3KB", 3_000),
            ("3MB", 3_000_000),
            ("3GB", 3_000_000_000),
            ("3KiB", 3 << 10),
            ("3MiB", 3 << 20),
            ("3GiB", 3 << 30),
            ("0256MiB", 256 << 20),
        ] {
            assert_eq!(bytes(text), expected, "{text}");
        }
    }

    #[test]
    fn bytes_convert_up_to_the_maximum() {
        assert_eq!(ByteSize::try_from(1 << 20), Ok(ByteSize::MIB));
        assert_eq!(ByteSize::try_from(i64::MAX as u64), Ok(ByteSize::MAX));
        assert!(ByteSize::try_from(i64::MAX as u64 + 1).is_err());
        assert_eq!(ByteSize::mebibytes(8).to_string(), "8MiB");
    }

    #[test]
    fn display_picks_the_largest_exact_unit_and_round_trips() {
        for (value, expected) in [
            (0, "0B"),
            (1, "1B"),
            (999, "999B"),
            (1_000, "1KB"),
            (1_024, "1KiB"),
            (1_536, "1536B"),
            (2_048_000, "2000KiB"),
            (1_000_000, "1MB"),
            (1 << 20, "1MiB"),
            (65_536_000, "64000KiB"),
            (3 << 29, "1536MiB"),
            (1 << 30, "1GiB"),
            (2_000_000_000, "2GB"),
            // Both divide: binary wins.
            (1_000_000_000 * (1 << 30), "1000000000GiB"),
            (i64::MAX as u64, "9223372036854775807B"),
        ] {
            let size = format!("{value}B").parse::<ByteSize>().unwrap();
            assert_eq!(size.to_string(), expected, "{value}");
            assert_eq!(bytes(&size.to_string()), value, "{expected} round-trips");
        }
    }

    #[test]
    fn bare_numbers_and_docker_units_suggest_mib() {
        assert_eq!(
            error("512"),
            "invalid size `512`: a size needs a unit, e.g. \"512MiB\""
        );
        for docker in ["512m", "512M", "512mb", "512Mb", "512mib"] {
            let err = error(docker);
            assert!(err.contains("\"512MiB\""), "{docker}: {err}");
            assert!(err.contains("\"512MB\""), "{docker}: {err}");
        }
        assert!(error("1g").contains("\"1GiB\""));
        assert!(error("4k").contains("\"4KiB\""));
    }

    #[test]
    fn malformed_sizes_are_rejected() {
        for (text, needle) in [
            ("", "expected a whole number and a unit"),
            ("MiB", "expected a whole number and a unit"),
            ("-1MiB", "expected a whole number and a unit"),
            ("+1MiB", "expected a whole number and a unit"),
            (" 1MiB", "expected a whole number and a unit"),
            ("1.5GiB", "whole number"),
            ("1,5GiB", "whole number"),
            ("512 MiB", "without a space, e.g. \"512MiB\""),
            ("1TiB", "unknown unit `TiB`"),
            ("1kB", "units are case-sensitive"),
            ("1MiB ", "unknown unit `MiB `"),
            ("1bytes", "unknown unit `bytes`"),
        ] {
            let err = error(text);
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }

    #[test]
    fn sizes_above_i64_max_overflow() {
        assert_eq!(bytes("9223372036854775807B"), i64::MAX as u64);
        assert_eq!(i64::from(ByteSize::MAX), i64::MAX);
        for text in [
            "9223372036854775808B",
            "8589934592GiB",
            "18446744073709551616B",
            "99999999999999999999999GiB",
        ] {
            assert!(
                error(text).contains("more than 9223372036854775807 bytes"),
                "{text}"
            );
        }
        assert_eq!(bytes("8589934591GiB"), 8_589_934_591 << 30);
    }

    #[test]
    fn serde_goes_through_from_str() {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct Holder {
            size: ByteSize,
        }
        let holder: Holder = toml::from_str("size = \"256MiB\"").unwrap();
        assert_eq!(u64::from(holder.size), 256 << 20);
        assert_eq!(
            toml::to_string(&holder).unwrap().trim(),
            "size = \"256MiB\""
        );
        let err = toml::from_str::<Holder>("size = \"256m\"")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("\"256MiB\""), "{err}");
    }
}
