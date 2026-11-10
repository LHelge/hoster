//! `hoster stack check`: validates stack files offline, the way an app repository's CI runs it.
//!
//! Each file goes through [`Stack::parse`], so a file passes exactly when hoster's own parse
//! accepts it. The apply pre-flight checks that need hoster's other stacks, configuration, secret
//! store, registry or cgroups cannot run here; [`StackCheck::SKIPPED`] names them and both
//! output formats say so.

use std::fmt;
use std::path::Path;

use serde::Serialize;
use serde::ser::SerializeStruct;

use super::diagnostic::Severity;
use super::{Counted, Diagnostic, FieldPath, Stack};
use crate::routing::{Reserved, RouteClaims};

/// The result of checking one or more stack files.
#[derive(Debug)]
pub struct StackCheck {
    files: Vec<FileCheck>,
    deny_warnings: bool,
}

impl StackCheck {
    /// What the apply pre-flight checks that an offline check cannot.
    pub const SKIPPED: [&str; 5] = [
        "routes taken by another stack",
        "the reserved admin host",
        "secrets being set in the secret store",
        "third-party image tags resolving to a digest",
        "cgroup controllers for resource limits",
    ];

    /// Reads and checks each file. A file that cannot be read is an error in it, not a failure of
    /// the whole check.
    pub fn files<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> Self {
        Self::new(paths.into_iter().map(|path| FileCheck::read(path.as_ref())))
    }

    fn new(files: impl IntoIterator<Item = FileCheck>) -> Self {
        Self {
            files: files.into_iter().collect(),
            deny_warnings: false,
        }
    }

    /// Makes warnings fail the check too.
    pub fn deny_warnings(self, deny_warnings: bool) -> Self {
        Self {
            deny_warnings,
            ..self
        }
    }

    pub fn errors(&self) -> usize {
        self.files.iter().map(|f| f.count(Severity::Error)).sum()
    }

    pub fn warnings(&self) -> usize {
        self.files.iter().map(|f| f.count(Severity::Warning)).sum()
    }

    /// `Ok` when no file has an error, nor a warning under [`Self::deny_warnings`]. Errors take
    /// precedence over denied warnings.
    pub fn verdict(&self) -> Result<(), CheckFailed> {
        let invalid = self.files.iter().filter(|f| !f.outcome.valid()).count();
        let warnings = self.warnings();
        if invalid > 0 {
            Err(CheckFailed::Invalid {
                invalid,
                files: self.files.len(),
            })
        } else if self.deny_warnings && warnings > 0 {
            Err(CheckFailed::Warnings(warnings))
        } else {
            Ok(())
        }
    }
}

/// Every diagnostic in the compiler style, a summary line per file, then the skipped checks.
impl fmt::Display for StackCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.files.iter().try_for_each(|file| write!(f, "{file}"))?;
        writeln!(
            f,
            "\nnote: not checked offline, as they need the server: {}.",
            Self::SKIPPED.join("; ")
        )
    }
}

/// `{ok, errors, warnings, files: [...], skipped: [...]}`.
impl Serialize for StackCheck {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut json = serializer.serialize_struct("StackCheck", 5)?;
        json.serialize_field("ok", &self.verdict().is_ok())?;
        json.serialize_field("errors", &self.errors())?;
        json.serialize_field("warnings", &self.warnings())?;
        json.serialize_field("files", &self.files)?;
        json.serialize_field("skipped", &Self::SKIPPED)?;
        json.end()
    }
}

/// Why `hoster stack check` exits non-zero. Public only because [`StackCheck::verdict`] returns
/// it; the module does not export it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckFailed {
    #[error("stack files with errors: {invalid} of {files}")]
    Invalid { invalid: usize, files: usize },
    #[error("{} (--deny-warnings)", Counted(*.0, "warning"))]
    Warnings(usize),
}

/// The check of one file.
#[derive(Debug)]
struct FileCheck {
    /// The path as given on the command line.
    file: String,
    outcome: Outcome,
}

/// Whether a file passed, with what it reported; in file order.
#[derive(Debug)]
enum Outcome {
    Valid {
        stack: String,
        services: usize,
        warnings: Vec<Diagnostic>,
    },
    Invalid(Vec<Diagnostic>),
}

impl Outcome {
    fn valid(&self) -> bool {
        matches!(self, Outcome::Valid { .. })
    }

    fn severity(&self) -> Severity {
        match self {
            Outcome::Valid { .. } => Severity::Warning,
            Outcome::Invalid(_) => Severity::Error,
        }
    }

    fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Outcome::Valid { warnings, .. } => warnings,
            Outcome::Invalid(errors) => errors,
        }
    }
}

impl FileCheck {
    /// Reads `path` and checks it.
    fn read(path: &Path) -> Self {
        let file = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(source) => Self::new(file, &source),
            Err(err) => Self {
                file,
                outcome: Outcome::Invalid(vec![Diagnostic::whole_file(format!(
                    "cannot read the file: {err}"
                ))]),
            },
        }
    }

    /// Checks `source`, reporting it as `file`: hoster's parse, then the routes the stack
    /// claims twice itself (ADR-0012), which the apply pre-flight rejects too.
    fn new(file: impl Into<String>, source: &str) -> Self {
        let outcome = match Stack::parse(source) {
            Err(err) => Outcome::Invalid(err.diagnostics().to_vec()),
            Ok(parsed) => match RouteClaims::check(&parsed.stack, [], &Reserved::default()) {
                Err(err) => Outcome::Invalid(
                    err.collisions
                        .iter()
                        .map(|collision| Diagnostic {
                            path: FieldPath::default()
                                .key("services")
                                .key(collision.claimant.service.as_ref())
                                .key("routes"),
                            location: None,
                            message: collision.to_string(),
                        })
                        .collect(),
                ),
                Ok(()) => Outcome::Valid {
                    stack: parsed.stack.name.to_string(),
                    services: parsed.stack.services.len(),
                    warnings: parsed.warnings,
                },
            },
        };
        Self {
            file: file.into(),
            outcome,
        }
    }

    fn count(&self, severity: Severity) -> usize {
        if self.outcome.severity() == severity {
            self.outcome.diagnostics().len()
        } else {
            0
        }
    }
}

/// The file's diagnostics, one per line, then `file: …` saying whether it is valid.
impl fmt::Display for FileCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = self.outcome.severity();
        for diagnostic in self.outcome.diagnostics() {
            writeln!(f, "{}", diagnostic.in_file(&self.file, severity))?;
        }
        write!(f, "{}: ", self.file)?;
        match &self.outcome {
            Outcome::Valid {
                stack,
                services,
                warnings,
            } => {
                write!(
                    f,
                    "valid, stack `{stack}` with {}",
                    Counted(*services, "service")
                )?;
                match warnings.len() {
                    0 => writeln!(f),
                    n => writeln!(f, ", {}", Counted(n, "warning")),
                }
            }
            Outcome::Invalid(errors) => {
                writeln!(f, "invalid, {}", Counted(errors.len(), "error"))
            }
        }
    }
}

/// `{file, stack?, services?, diagnostics: [{severity, path, location, message}]}`; `stack` and
/// `services` only when the file is valid.
impl Serialize for FileCheck {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        /// A diagnostic with how serious it is.
        #[derive(Serialize)]
        struct Finding<'a> {
            severity: Severity,
            #[serde(flatten)]
            diagnostic: &'a Diagnostic,
        }

        let mut json = serializer.serialize_struct("FileCheck", 4)?;
        json.serialize_field("file", &self.file)?;
        if let Outcome::Valid {
            stack, services, ..
        } = &self.outcome
        {
            json.serialize_field("stack", stack)?;
            json.serialize_field("services", services)?;
        }
        let severity = self.outcome.severity();
        let findings: Vec<Finding<'_>> = self
            .outcome
            .diagnostics()
            .iter()
            .map(|diagnostic| Finding {
                severity,
                diagnostic,
            })
            .collect();
        json.serialize_field("diagnostics", &findings)?;
        json.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::tests::MINIMAL;

    fn invalid() -> String {
        MINIMAL
            .replace("name = \"demo\"", "name = \"Demo\"")
            .replace("port = 8080", "port = 0")
    }

    fn warning_only() -> String {
        format!("{MINIMAL}\n[services.app.volumes]\ndata = \"/data\"\n")
    }

    /// A TOML syntax error: caught by the toml parser, before validation.
    fn syntax_error() -> String {
        MINIMAL.replace("name = \"demo\"", "name = \"demo")
    }

    /// Two services on the same (host, prefix).
    fn duplicate_route() -> String {
        format!(
            "{MINIMAL}\n[services.web]\nimage = \"ghcr.io/acme/web\"\n\n\
             [[services.web.routes]]\nhost = \"demo.example.com\"\nport = 8080\n"
        )
    }

    fn check(files: &[(&str, &str)]) -> StackCheck {
        StackCheck::new(
            files
                .iter()
                .map(|(file, source)| FileCheck::new(*file, source)),
        )
    }

    #[test]
    fn a_valid_file_passes_without_diagnostics() {
        let check = check(&[("demo.toml", MINIMAL)]);
        assert_eq!((check.errors(), check.warnings()), (0, 0));
        assert_eq!(check.verdict(), Ok(()));
        assert!(
            check
                .to_string()
                .starts_with("demo.toml: valid, stack `demo` with 1 service\n"),
            "{check}"
        );
    }

    #[test]
    fn an_invalid_file_fails_with_every_error() {
        let check = check(&[("ok.toml", MINIMAL), ("bad.toml", &invalid())]);
        assert_eq!(check.errors(), 2);
        assert_eq!(
            check.verdict(),
            Err(CheckFailed::Invalid {
                invalid: 1,
                files: 2
            })
        );
        assert_eq!(
            check.verdict().unwrap_err().to_string(),
            "stack files with errors: 1 of 2"
        );
    }

    #[test]
    fn warnings_pass_unless_denied() {
        let check = check(&[("warn.toml", &warning_only())]);
        assert_eq!((check.errors(), check.warnings()), (0, 1));
        assert_eq!(check.verdict(), Ok(()));
        let denied = check.deny_warnings(true);
        assert_eq!(denied.verdict(), Err(CheckFailed::Warnings(1)));
        assert_eq!(
            denied.verdict().unwrap_err().to_string(),
            "1 warning (--deny-warnings)"
        );
    }

    #[test]
    fn a_syntax_error_is_reported_with_its_location() {
        let check = check(&[("bad.toml", &syntax_error())]);
        assert_eq!(check.errors(), 1);
        assert!(check.verdict().is_err());
        let text = check.to_string();
        assert!(text.starts_with("bad.toml:3:"), "{text}");
        assert!(text.contains(": error: "), "{text}");
    }

    #[test]
    fn a_route_claimed_twice_in_the_stack_is_an_error() {
        let check = check(&[("dup.toml", &duplicate_route())]);
        assert_eq!(check.errors(), 1);
        assert_eq!(
            check.verdict(),
            Err(CheckFailed::Invalid {
                invalid: 1,
                files: 1
            })
        );
        let text = check.to_string();
        assert!(
            text.starts_with(
                "dup.toml: error: services.web.routes: route `demo.example.com/` of service \
                 `web` of stack `demo` duplicates the same route of service `app` of stack `demo`\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn errors_take_precedence_over_denied_warnings() {
        let check =
            check(&[("bad.toml", &invalid()), ("warn.toml", &warning_only())]).deny_warnings(true);
        assert_eq!(
            check.verdict(),
            Err(CheckFailed::Invalid {
                invalid: 1,
                files: 2
            })
        );
    }

    #[test]
    fn an_unreadable_file_is_an_error_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        let check = StackCheck::files([&missing]);
        assert_eq!(check.errors(), 1);
        assert!(check.verdict().is_err());
        let text = check.to_string();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with(&format!(
                "{}: error: cannot read the file: ",
                missing.display()
            )),
            "{text}"
        );
    }

    #[test]
    fn reads_files_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.stack.toml");
        std::fs::write(&path, MINIMAL).unwrap();
        let check = StackCheck::files([&path]);
        assert_eq!(check.verdict(), Ok(()));
    }

    #[test]
    fn text_output_snapshot() {
        let check = check(&[
            ("ok.toml", MINIMAL),
            ("bad.toml", &invalid()),
            ("syntax.toml", &syntax_error()),
            ("dup.toml", &duplicate_route()),
            ("warn.toml", &warning_only()),
        ]);
        insta::assert_snapshot!(check.to_string());
    }

    #[test]
    fn json_output_snapshot() {
        let check = check(&[
            ("ok.toml", MINIMAL),
            ("bad.toml", &invalid()),
            ("syntax.toml", &syntax_error()),
            ("dup.toml", &duplicate_route()),
            ("warn.toml", &warning_only()),
        ])
        .deny_warnings(true);
        insta::assert_snapshot!(serde_json::to_string_pretty(&check).unwrap());
    }
}
