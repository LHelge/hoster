//! Diagnostics: a message tied to a field path and a TOML line/column.

use std::fmt;

/// A dotted path to a field, e.g. `services.api.routes[0].host`. Empty for the document root.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
        self.fmt_body(f)
    }
}

impl Diagnostic {
    /// `path: message`, or only the message at the document root: what follows the location in
    /// both [`Diagnostic`]'s and [`InFile`]'s display.
    fn fmt_body(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.path.is_root() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)
    }

    /// A problem with the file as a whole, such as one that cannot be read.
    pub(super) fn whole_file(message: impl Into<String>) -> Self {
        Self {
            path: FieldPath::default(),
            location: None,
            message: message.into(),
        }
    }

    /// This diagnostic in the compiler style editors and CI logs link to:
    /// `file:line:column: severity: path: message`.
    pub(super) fn in_file<'a>(&'a self, file: &'a str, severity: Severity) -> InFile<'a> {
        InFile {
            file,
            severity,
            diagnostic: self,
        }
    }
}

/// Whether a diagnostic stops a stack file from being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Severity {
    /// The file is refused.
    Error,
    /// The file applies, but probably does not do what was meant.
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        })
    }
}

/// A [`Diagnostic`] displayed with its file and severity; see [`Diagnostic::in_file`].
pub(super) struct InFile<'a> {
    file: &'a str,
    severity: Severity,
    diagnostic: &'a Diagnostic,
}

impl fmt::Display for InFile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            file,
            severity,
            diagnostic,
        } = self;
        match diagnostic.location {
            Some(Location { line, column }) => write!(f, "{file}:{line}:{column}: {severity}: ")?,
            None => write!(f, "{file}: {severity}: ")?,
        }
        diagnostic.fmt_body(f)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(path: FieldPath, location: Option<Location>) -> Diagnostic {
        Diagnostic {
            path,
            location,
            message: "broken".to_owned(),
        }
    }

    #[test]
    fn displays_in_the_compiler_style() {
        let path = FieldPath::default().key("services").key("app");
        let at = Some(Location { line: 3, column: 8 });
        let cases = [
            (
                diagnostic(path.clone(), at),
                Severity::Error,
                "a.toml:3:8: error: services.app: broken",
            ),
            (
                diagnostic(FieldPath::default(), at),
                Severity::Warning,
                "a.toml:3:8: warning: broken",
            ),
            (
                diagnostic(path, None),
                Severity::Error,
                "a.toml: error: services.app: broken",
            ),
            (
                Diagnostic::whole_file("broken"),
                Severity::Error,
                "a.toml: error: broken",
            ),
        ];
        for (diagnostic, severity, expected) in cases {
            assert_eq!(diagnostic.in_file("a.toml", severity).to_string(), expected);
        }
    }

    #[test]
    fn displays_the_same_body_without_a_file() {
        let at = Some(Location { line: 3, column: 8 });
        let path = FieldPath::default().key("name");
        assert_eq!(
            diagnostic(path.clone(), at).to_string(),
            "line 3, column 8: name: broken"
        );
        assert_eq!(diagnostic(path, None).to_string(), "name: broken");
        assert_eq!(Diagnostic::whole_file("broken").to_string(), "broken");
    }
}
