//! Route keys: [`Host`] and [`NormalizedPrefix`] (ADR-0009). Shared with the routing table.

use std::fmt;
use std::str::FromStr;

/// A lowercase fully qualified domain name without port, scheme or trailing dot. No wildcards
/// (ADR-0016) and no IP addresses: a host must be something ACME can issue a certificate for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Host(String);

impl FromStr for Host {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let reject = |why: &str| Err(format!("invalid host `{s}`: {why}"));
        if s.is_empty() {
            return reject("must not be empty");
        }
        if s.contains("://") {
            return reject("must not include a scheme");
        }
        if s.contains('/') {
            return reject("must not include a path; use `prefix`");
        }
        if s.contains(':') {
            return reject("must not include a port");
        }
        if s.contains('*') {
            return reject("wildcard hosts are not supported");
        }
        if s.ends_with('.') {
            return reject("must not end with a dot");
        }
        if s.chars().any(|c| c.is_ascii_uppercase()) {
            return reject("must be lowercase");
        }
        if s.len() > 253 {
            return reject("must be at most 253 characters");
        }
        let labels: Vec<&str> = s.split('.').collect();
        if labels.len() < 2 {
            return reject("must be a fully qualified domain name");
        }
        for label in &labels {
            if label.is_empty() {
                return reject("must not contain empty labels");
            }
            if label.len() > 63 {
                return reject("labels must be at most 63 characters");
            }
            if !label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return reject("labels may only contain a-z, 0-9 and `-`");
            }
            if label.starts_with('-') || label.ends_with('-') {
                return reject("labels must not start or end with `-`");
            }
        }
        if labels
            .last()
            .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()))
        {
            return reject("IP addresses are not allowed; use a DNS name");
        }
        Ok(Self(s.to_owned()))
    }
}

impl AsRef<str> for Host {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A route path prefix split into segments, so that `/api` matches `/api/x` but not `/apix`
/// (ADR-0009). Two prefixes collide exactly when their segments are equal (ADR-0012).
///
/// Parsing is strict rather than forgiving: it must start with `/`, has no trailing `/` except
/// the root, and contains no `?`, `#`, `%`, empty, `.` or `..` segments, so every accepted
/// string has exactly one spelling.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NormalizedPrefix {
    segments: Vec<String>,
}

impl NormalizedPrefix {
    /// The root prefix `/`, which matches every path.
    pub fn root() -> Self {
        Self::default()
    }

    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// The path segments, without slashes. Empty for the root.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    fn is_segment_char(c: char) -> bool {
        // RFC 3986 pchar without percent-encoding.
        c.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:@".contains(c)
    }
}

impl FromStr for NormalizedPrefix {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let reject = |why: &str| Err(format!("invalid prefix `{s}`: {why}"));
        let Some(rest) = s.strip_prefix('/') else {
            return reject("must start with `/`");
        };
        if s.contains('?') || s.contains('#') {
            return reject("must not contain `?` or `#`");
        }
        if s.contains("//") {
            return reject("must not contain `//`");
        }
        if rest.is_empty() {
            return Ok(Self::root());
        }
        if rest.ends_with('/') {
            return reject("must not end with `/`");
        }
        if s.contains('%') {
            return reject("must not contain percent-encoding");
        }
        let mut segments = Vec::new();
        for segment in rest.split('/') {
            if segment == "." || segment == ".." {
                return reject("must not contain `.` or `..` segments");
            }
            if let Some(c) = segment.chars().find(|&c| !Self::is_segment_char(c)) {
                return Err(format!(
                    "invalid prefix `{s}`: character {c:?} is not allowed"
                ));
            }
            segments.push(segment.to_owned());
        }
        Ok(Self { segments })
    }
}

impl fmt::Display for NormalizedPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            return f.write_str("/");
        }
        self.segments
            .iter()
            .try_for_each(|segment| write!(f, "/{segment}"))
    }
}
