//! The notes a deployment writes about its progress within a state (ADR-0056, architecture
//! §5.1): [`Note`] words the engine's, and [`NoteKind`] sorts a deployment event's text by
//! those same words and the health check's ([`ProbeOutput::is_probe`]), so a page can tag the
//! start waves, the check each service gets and the probe results apart.

use std::fmt;
use std::time::Duration;

use crate::health::{Check, Elapsed, HealthStatus, ProbeOutput};
use crate::stack::Name;

/// A note the engine writes about its progress.
pub(super) enum Note<'a> {
    /// `starting db, cache`; after a pass `db healthy after 3s 241ms; starting app`, or only
    /// `db healthy after 3s 241ms` when nothing else is ready. Never both empty.
    Wave {
        passed: Option<(&'a Name, Duration)>,
        starting: &'a [&'a str],
    },
    /// `db has never run; starting app without it` (ADR-0056 Decision 2.5).
    NeverRun {
        dependency: &'a Name,
        service: &'a Name,
    },
    /// `db (unchanged) is running; its monitor status is healthy`.
    UnchangedRunning {
        dependency: &'a Name,
        status: HealthStatus,
    },
    /// `replacing db (stop-first)`.
    Replacing(&'a Name),
    /// `starting app (held back for db)`.
    HeldBack {
        service: &'a Name,
        behind: &'a [&'a str],
    },
    /// `starting api (unchanged) again: it exited while db was replaced`.
    Restarting {
        service: &'a Name,
        replaced: &'a Name,
    },
    /// `checking db with exec: pg_isready, declared in health.exec`.
    Checking { service: &'a Name, check: &'a Check },
}

impl Note<'_> {
    const STARTING: &'static str = "starting ";
    const REPLACING: &'static str = "replacing ";
    const HEALTHY_AFTER: &'static str = " healthy after ";
    const NEVER_RUN: &'static str = " has never run; ";
    const UNCHANGED_RUNNING: &'static str = " (unchanged) is running; ";
    const CHECKING: &'static str = "checking ";
}

impl fmt::Display for Note<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let starting = Note::STARTING;
        match self {
            Self::Wave {
                passed,
                starting: names,
            } => {
                if let Some((service, took)) = passed {
                    write!(f, "{service}{}{}", Note::HEALTHY_AFTER, Elapsed(*took))?;
                    if !names.is_empty() {
                        f.write_str("; ")?;
                    }
                }
                if !names.is_empty() {
                    write!(f, "{starting}{}", names.join(", "))?;
                }
                Ok(())
            }
            Self::NeverRun {
                dependency,
                service,
            } => write!(
                f,
                "{dependency}{}{starting}{service} without it",
                Note::NEVER_RUN
            ),
            Self::UnchangedRunning { dependency, status } => write!(
                f,
                "{dependency}{}its monitor status is {status}",
                Note::UNCHANGED_RUNNING
            ),
            Self::Replacing(service) => write!(f, "{}{service} (stop-first)", Note::REPLACING),
            Self::HeldBack { service, behind } => write!(
                f,
                "{starting}{service} (held back for {})",
                behind.join(", ")
            ),
            Self::Restarting { service, replaced } => write!(
                f,
                "{starting}{service} (unchanged) again: it exited while {replaced} was replaced"
            ),
            Self::Checking { service, check } => {
                write!(f, "{}{service} with {check:#}", Note::CHECKING)
            }
        }
    }
}

/// What a note in a deployment's events is about, as its page tags it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// A start wave or a replacement, and what is checked before one ([`Note`]'s waves).
    Wave,
    /// The check a service gets and why.
    Check,
    /// A changed probe result, the verdict, or why the probes stopped.
    Probe,
}

/// A deployment event that is none of the notes [`NoteKind`] knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtherNote;

impl TryFrom<&str> for NoteKind {
    type Error = OtherNote;

    /// Sorts a deployment event's text (without an exec output tail) by how [`Note`] and the
    /// health check word it.
    fn try_from(message: &str) -> Result<Self, OtherNote> {
        if ProbeOutput::from(message).is_probe() {
            return Ok(Self::Probe);
        }
        if message.starts_with(Note::CHECKING) {
            return Ok(Self::Check);
        }
        let wave = [Note::STARTING, Note::REPLACING]
            .iter()
            .any(|prefix| message.starts_with(prefix))
            || [
                Note::HEALTHY_AFTER,
                Note::NEVER_RUN,
                Note::UNCHANGED_RUNNING,
            ]
            .iter()
            .any(|part| message.contains(part));
        if wave { Ok(Self::Wave) } else { Err(OtherNote) }
    }
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wave => "wave",
            Self::Check => "check",
            Self::Probe => "probe",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{DEFAULT_SETTLE, DEFAULT_SETTLE_POLL};

    /// Every note the engine writes is sorted by the words it is written with.
    #[test]
    fn the_engines_notes_are_sorted_by_their_words() {
        let name = |n: &str| n.parse::<Name>().unwrap();
        let (db, app) = (name("db"), name("app"));
        let check = Check::settle(DEFAULT_SETTLE, DEFAULT_SETTLE_POLL);
        let took = Duration::from_millis(3241);
        let table = [
            (
                Note::Wave {
                    passed: None,
                    starting: &["cache", "db"],
                },
                "starting cache, db",
                NoteKind::Wave,
            ),
            (
                Note::Wave {
                    passed: Some((&db, took)),
                    starting: &["app", "worker"],
                },
                "db healthy after 3s 241ms; starting app, worker",
                NoteKind::Wave,
            ),
            (
                Note::Wave {
                    passed: Some((&db, took)),
                    starting: &[],
                },
                "db healthy after 3s 241ms",
                NoteKind::Wave,
            ),
            (
                Note::NeverRun {
                    dependency: &db,
                    service: &app,
                },
                "db has never run; starting app without it",
                NoteKind::Wave,
            ),
            (
                Note::UnchangedRunning {
                    dependency: &db,
                    status: HealthStatus::Healthy,
                },
                "db (unchanged) is running; its monitor status is healthy",
                NoteKind::Wave,
            ),
            (
                Note::Replacing(&db),
                "replacing db (stop-first)",
                NoteKind::Wave,
            ),
            (
                Note::HeldBack {
                    service: &app,
                    behind: &["db"],
                },
                "starting app (held back for db)",
                NoteKind::Wave,
            ),
            (
                Note::Restarting {
                    service: &app,
                    replaced: &db,
                },
                "starting app (unchanged) again: it exited while db was replaced",
                NoteKind::Wave,
            ),
            (
                Note::Checking {
                    service: &db,
                    check: &check,
                },
                "checking db with running for 5s, the default for a service without routes or \
                 health",
                NoteKind::Check,
            ),
        ];
        for (note, text, kind) in table {
            let note = note.to_string();
            assert_eq!(note, text);
            assert_eq!(NoteKind::try_from(note.as_str()), Ok(kind), "{note}");
        }
        // The check's own events (`health::tests` covers each of them).
        let verdict = crate::health::HealthError::CannotRun("`pg_isready`: not found".into());
        assert_eq!(
            NoteKind::try_from(verdict.to_string().as_str()),
            Ok(NoteKind::Probe)
        );
        assert_eq!(
            NoteKind::try_from("cancelled by admin"),
            Err(OtherNote),
            "not a progress note"
        );
    }
}
