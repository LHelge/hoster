//! Diagnostics: a message tied to a field path and a TOML line/column.

use std::fmt;

/// A dotted path to a field, e.g. `services.api.routes[0].host`. Empty for the document root.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath(String);

impl FieldPath {
    /// The path of a table key below this one.
    pub fn key(&self, key: &str) -> Self {
        let bare = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        let segment = if bare {
            key.to_owned()
        } else {
            format!("{key:?}")
        };
        if self.0.is_empty() {
            Self(segment)
        } else {
            Self(format!("{}.{segment}", self.0))
        }
    }

    /// The path of an array element below this one.
    pub fn index(&self, index: usize) -> Self {
        Self(format!("{}[{index}]", self.0))
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&serde_path_to_error::Path> for FieldPath {
    fn from(path: &serde_path_to_error::Path) -> Self {
        use serde_path_to_error::Segment;
        // `Spanned<T>` deserializes through a private wrapper struct; hide its field names.
        path.iter()
            .filter(|segment| {
                !matches!(segment, Segment::Map { key } if key.starts_with("$__serde_spanned_private"))
            })
            .fold(FieldPath::default(), |acc, segment| match segment {
                Segment::Seq { index } => acc.index(*index),
                Segment::Map { key } => acc.key(key),
                Segment::Enum { variant } => acc.key(variant),
                Segment::Unknown => acc.key("?"),
            })
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A 1-based line and column (in characters) in the stack file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

impl Location {
    /// Locates a byte offset in `source`. Offsets past the end clamp to the end.
    pub fn of(source: &str, offset: usize) -> Self {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &source[..offset];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        Self {
            line: before.matches('\n').count() + 1,
            column: before[line_start..].chars().count() + 1,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// One problem (error or warning) in a stack file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: FieldPath,
    pub location: Option<Location>,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(location) = self.location {
            write!(f, "{location}: ")?;
        }
        if !self.path.is_root() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)
    }
}

/// A stack file that failed to parse or validate. Holds at least one diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct StackError {
    diagnostics: Vec<Diagnostic>,
}

impl StackError {
    pub(super) fn new(mut diagnostics: Vec<Diagnostic>) -> Self {
        debug_assert!(!diagnostics.is_empty());
        diagnostics.sort_by_key(|d| d.location);
        Self { diagnostics }
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

impl fmt::Display for StackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.diagnostics.as_slice() {
            [single] => write!(f, "invalid stack file: {single}"),
            all => {
                write!(f, "invalid stack file ({} errors):", all.len())?;
                all.iter().try_for_each(|d| write!(f, "\n  {d}"))
            }
        }
    }
}
