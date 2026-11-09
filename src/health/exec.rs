//! Exec checks (ADR-0056, docs/architecture.md §5.5): one probe at a time per container, at
//! most [`ExecSlots::MAX_ABANDONED`] timed-out execs left running in it, and the text a result
//! becomes in deployment events.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::select_all;
use tokio::time::Instant;

use crate::runtime::{ContainerId, ContainerRuntime, ExecOutcome, RunningExec, RuntimeError};

/// The exec probes of every container, shared by deployments and the health monitor. Cloning
/// it shares the same slots.
///
/// Each container has one slot: a probe holds it while its exec runs, so two never run at once
/// in one container (a probe overlapping the next interval waits for it, at most the exec's
/// `timeout`). The slot also keeps the execs that outlived their timeout and are still running
/// (podman cannot always end them, `PrivateUsers=yes` and a non-root container user); at
/// [`Self::MAX_ABANDONED`] of them a deploy probe waits for one to end, bounded by its deadline
/// and given up as soon as the container stops, and a monitor probe starts none.
#[derive(Debug, Clone, Default)]
pub struct ExecSlots(Arc<Mutex<HashMap<ContainerId, Slot>>>);

type Slot = Arc<tokio::sync::Mutex<Vec<RunningExec>>>;

/// What a probe does when its container has [`ExecSlots::MAX_ABANDONED`] execs still running.
#[derive(Debug, Clone, Copy)]
pub(super) enum AtCap {
    /// Wait for one of them to end, until then.
    WaitUntil(Instant),
    /// Start none: the check is stuck.
    Refuse,
}

/// Why no exec ran or what it failed with.
#[derive(Debug)]
pub(super) enum SlotError {
    /// The container has this many timed-out execs still running.
    Stuck(usize),
    Runtime(RuntimeError),
}

impl ExecSlots {
    /// Timed-out execs that may still run in one container before probes stop starting new
    /// ones (architecture §5.5).
    pub const MAX_ABANDONED: usize = 3;

    /// How often a deploy probe waiting at the cap looks whether the container still runs.
    const CAP_POLL: Duration = Duration::from_secs(1);

    /// Runs `argv` in `container` through `runtime`, once the container's slot is free and it
    /// is below the cap of abandoned execs. Waiting at the cap ends with
    /// [`RuntimeError::NotRunning`] once an inspection finds the container stopped.
    pub(super) async fn run<R: ContainerRuntime>(
        &self,
        runtime: &R,
        container: &ContainerId,
        argv: &[String],
        timeout: Duration,
        at_cap: AtCap,
    ) -> Result<ExecOutcome, SlotError> {
        let slot = self.slot(container);
        let mut abandoned = slot.lock().await;
        abandoned.retain(RunningExec::is_running);
        while abandoned.len() >= Self::MAX_ABANDONED {
            let AtCap::WaitUntil(deadline) = at_cap else {
                return Err(SlotError::Stuck(abandoned.len()));
            };
            let one_ended = select_all(abandoned.iter().map(|exec| Box::pin(exec.ended())));
            let look = deadline.min(Instant::now() + Self::CAP_POLL);
            if tokio::time::timeout_at(look, one_ended).await.is_err() {
                if Instant::now() >= deadline {
                    return Err(SlotError::Stuck(abandoned.len()));
                }
                let info = runtime
                    .inspect(container)
                    .await
                    .map_err(SlotError::Runtime)?;
                if !info.state.is_running() {
                    return Err(SlotError::Runtime(RuntimeError::NotRunning {
                        name: container.to_string(),
                    }));
                }
            }
            abandoned.retain(RunningExec::is_running);
        }
        let outcome = runtime
            .exec(container, argv, timeout)
            .await
            .map_err(SlotError::Runtime)?;
        if let ExecOutcome::TimedOut { exec, .. } = &outcome {
            abandoned.push(exec.clone());
        }
        Ok(outcome)
    }

    /// The timed-out execs of `container` still running.
    #[cfg(test)]
    pub fn abandoned(&self, container: &ContainerId) -> usize {
        self.0
            .lock()
            .expect("exec slots poisoned")
            .get(container)
            .and_then(|slot| {
                slot.try_lock()
                    .ok()
                    .map(|a| a.iter().filter(|e| e.is_running()).count())
            })
            .unwrap_or(0)
    }

    /// `container`'s slot. Slots nobody holds and without running execs are dropped, so
    /// removed containers do not pile up.
    fn slot(&self, container: &ContainerId) -> Slot {
        let mut slots = self.0.lock().expect("exec slots poisoned");
        slots.retain(|_, slot| {
            Arc::strong_count(slot) > 1
                || slot.try_lock().map_or(true, |abandoned| {
                    abandoned.iter().any(RunningExec::is_running)
                })
        });
        Arc::clone(slots.entry(container.clone()).or_default())
    }
}

/// An exec check's command as events show it: `pg_isready -U postgres`, with an argument that
/// is empty or has spaces or quotes quoted.
pub(super) struct Command<'a>(pub(super) &'a [String]);

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, arg) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            let plain = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| !c.is_whitespace() && !c.is_control() && c != '"' && c != '\'');
            if plain {
                f.write_str(arg)?;
            } else {
                write!(f, "\"{}\"", arg.escape_debug())?;
            }
        }
        Ok(())
    }
}

/// The end of an exec's (redacted) output as one escaped line for a deployment event: the last
/// [`Self::MAX_CHARS`] characters, trailing whitespace dropped, `…` in front when cut.
pub(super) struct Tail<'a>(pub(super) &'a str);

impl Tail<'_> {
    /// How much of the output a deployment event shows (architecture §5.5).
    pub(super) const MAX_CHARS: usize = 256;

    pub(super) fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Display for Tail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.0.trim_end();
        let count = text.chars().count();
        let (cut, start) = match count.checked_sub(Self::MAX_CHARS) {
            Some(skip) if skip > 0 => (true, text.char_indices().nth(skip).map_or(0, |(i, _)| i)),
            _ => (false, 0),
        };
        write!(
            f,
            "\"{}{}\"",
            if cut { "…" } else { "" },
            text[start..].escape_debug()
        )
    }
}

/// A changed probe result as a deployment event records it: `health probe <check>: <result>`,
/// followed by `, output "<tail>"` (a [`Tail`]) for an exec check that printed something.
/// [`ProbeOutput`] reads the tail back.
pub(super) struct ProbeLine<'a> {
    pub(super) check: &'a dyn fmt::Display,
    pub(super) result: &'a str,
    /// The formatted [`Tail`], if any.
    pub(super) tail: Option<&'a str>,
}

impl ProbeLine<'_> {
    const PREFIX: &'static str = "health probe ";
    /// What separates the result from the quoted tail.
    const OUTPUT: &'static str = ", output ";
}

impl fmt::Display for ProbeLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}: {}", ProbeLine::PREFIX, self.check, self.result)?;
        if let Some(tail) = self.tail {
            write!(f, "{}{tail}", ProbeLine::OUTPUT)?;
        }
        Ok(())
    }
}

/// A deployment event's text with the exec output tail of a probe result split off and
/// unescaped, so a page can show the output apart (collapsed) from the result. Any other
/// event, or a tail that does not read back, is all `text`.
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeOutput<'a> {
    /// The event without its tail: `health probe exec: pg_isready -U app: exit 2`.
    pub text: &'a str,
    /// The output tail as the command printed it (`…` in front when cut).
    pub output: Option<String>,
}

impl<'a> From<&'a str> for ProbeOutput<'a> {
    fn from(message: &'a str) -> Self {
        let split = || {
            message.strip_prefix(ProbeLine::PREFIX)?;
            // The tail escapes every quote, so the last `, output "` with its quote unescaped
            // starts it.
            let at = message.rfind(&format!("{}\"", ProbeLine::OUTPUT))?;
            let quoted = &message[at + ProbeLine::OUTPUT.len()..];
            let output = Self::unescape(quoted.strip_prefix('"')?.strip_suffix('"')?)?;
            Some((&message[..at], output))
        };
        match split() {
            Some((text, output)) => Self {
                text,
                output: Some(output),
            },
            None => Self {
                text: message,
                output: None,
            },
        }
    }
}

impl ProbeOutput<'_> {
    /// Whether a health check wrote the event: a changed probe result, the verdict, or why
    /// the probes stopped (the exec cannot run, the container stopped, inspect failed).
    pub fn is_probe(&self) -> bool {
        let text = self.text;
        [
            ProbeLine::PREFIX,
            super::Check::VERDICT,
            super::HealthError::CANNOT_RUN,
            super::HealthError::INSPECT,
        ]
        .iter()
        .any(|prefix| text.starts_with(prefix))
            || (text.starts_with("container ")
                && text.ends_with(super::HealthError::BEFORE_HEALTHY))
    }

    /// The inverse of `str::escape_debug`, which [`Tail`] writes; `None` for anything it
    /// never writes (an unknown escape, or a bare quote).
    fn unescape(escaped: &str) -> Option<String> {
        let mut text = String::with_capacity(escaped.len());
        let mut chars = escaped.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return None,
                '\\' => text.push(match chars.next()? {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '0' => '\0',
                    c @ ('\\' | '"' | '\'') => c,
                    'u' => {
                        if chars.next()? != '{' {
                            return None;
                        }
                        let hex: String = chars.by_ref().take_while(|&c| c != '}').collect();
                        char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
                    }
                    _ => return None,
                }),
                c => text.push(c),
            }
        }
        Some(text)
    }
}

/// Why podman could not run an exec check, from its OCI runtime error: `` `pg_isready`:
/// executable file not found in $PATH ``.
pub(super) struct CannotRun<'a> {
    pub(super) argv0: &'a str,
    /// Podman's text, e.g. `runc: exec failed: unable to start container process: exec:
    /// "pg_isready": executable file not found in $PATH: OCI runtime attempted to invoke a
    /// command that was not found`.
    pub(super) reason: &'a str,
}

impl fmt::Display for CannotRun<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = self.reason.trim();
        let reason = reason.split(": OCI ").next().unwrap_or(reason);
        let named = format!("exec: \"{}\": ", self.argv0);
        let reason = match reason.find(&named) {
            Some(at) => &reason[at + named.len()..],
            None => ["runc: ", "crun: "]
                .iter()
                .find_map(|prefix| reason.strip_prefix(prefix))
                .unwrap_or(reason),
        };
        write!(f, "`{}`: {reason}", self.argv0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_quote_what_needs_it() {
        let argv = |args: &[&str]| args.iter().map(|&a| a.to_owned()).collect::<Vec<_>>();
        assert_eq!(
            Command(&argv(&["pg_isready", "-U", "postgres"])).to_string(),
            "pg_isready -U postgres"
        );
        assert_eq!(
            Command(&argv(&["sh", "-c", "curl -f localhost", ""])).to_string(),
            r#"sh -c "curl -f localhost" """#
        );
    }

    #[test]
    fn the_tail_is_one_escaped_line_of_the_last_characters() {
        assert_eq!(
            Tail("accepting connections\n").to_string(),
            r#""accepting connections""#
        );
        assert_eq!(Tail("a\n\"b\"\n").to_string(), r#""a\n\"b\"""#);
        let long = format!("{}{}", "x".repeat(10), "ä".repeat(Tail::MAX_CHARS));
        assert_eq!(
            Tail(&long).to_string(),
            format!("\"…{}\"", "ä".repeat(Tail::MAX_CHARS))
        );
        let exact = "y".repeat(Tail::MAX_CHARS);
        assert_eq!(Tail(&exact).to_string(), format!("\"{exact}\""));
        assert!(Tail(" \n").is_empty());
    }

    #[test]
    fn probe_output_reads_the_tail_back() {
        let check = "exec: sh -c \"pg_isready, output \\\"x\\\"\"";
        for output in [
            "accepting connections",
            "a \"quoted\", output \"x\" line\nand\ttabs \\ back\\slash 'single'",
            "nul \0 bell \u{7} zero-width-joiner \u{200d} é ä 日本",
        ] {
            let tail = Tail(output).to_string();
            let line = ProbeLine {
                check: &check,
                result: "exit 1",
                tail: Some(&tail),
            }
            .to_string();
            assert_eq!(
                ProbeOutput::from(line.as_str()),
                ProbeOutput {
                    text: &format!("health probe {check}: exit 1"),
                    output: Some(output.to_owned()),
                },
                "{line}"
            );
        }
        // Cut: the ellipsis stays in front.
        let long = format!("{}\n{}", "x".repeat(10), "y".repeat(Tail::MAX_CHARS));
        let tail = Tail(&long).to_string();
        let line = ProbeLine {
            check: &"exec: check",
            result: "exit 1",
            tail: Some(&tail),
        }
        .to_string();
        assert_eq!(
            ProbeOutput::from(line.as_str()).output,
            Some(format!("…{}", "y".repeat(Tail::MAX_CHARS)))
        );
        // Without a tail, or another event: all text.
        for message in [
            "health probe exec: check: exit 1",
            "checking db with exec: check, output \"x\"",
            "health probe exec: check: exit 1, output \"bad \\q escape\"",
            "health probe exec: check: exit 1, output \"unterminated",
        ] {
            assert_eq!(
                ProbeOutput::from(message),
                ProbeOutput {
                    text: message,
                    output: None
                }
            );
        }
    }

    #[test]
    fn cannot_run_reads_podmans_reason() {
        let runc = "runc: exec failed: unable to start container process: exec: \
                    \"pg_isready\": executable file not found in $PATH: OCI runtime attempted \
                    to invoke a command that was not found";
        assert_eq!(
            CannotRun {
                argv0: "pg_isready",
                reason: runc
            }
            .to_string(),
            "`pg_isready`: executable file not found in $PATH"
        );
        let crun = "crun: executable file not found in $PATH: No such file or directory: OCI \
                    runtime attempted to invoke a command that was not found";
        assert_eq!(
            CannotRun {
                argv0: "check",
                reason: crun
            }
            .to_string(),
            "`check`: executable file not found in $PATH: No such file or directory"
        );
        let directory = "runc: exec failed: unable to start container process: exec: \"/etc\": \
                         is a directory: OCI runtime error";
        assert_eq!(
            CannotRun {
                argv0: "/etc",
                reason: directory
            }
            .to_string(),
            "`/etc`: is a directory"
        );
    }
}
