//! Running a command inside a container: what [`ContainerRuntime::exec`] reports, and the
//! host-side handling of an exec that outlives its timeout (ADR-0056, docs/architecture.md §5.5).
//!
//! [`ContainerRuntime::exec`]: super::ContainerRuntime::exec

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, Signal, getpgid, kill_process_group, pidfd_open};
use tokio::sync::watch;

use super::{Operation, RuntimeError};

/// An exec's command: argv[0] and its arguments, run without a shell. Never empty.
#[derive(Debug, Clone, Copy)]
pub(super) struct Argv<'a>(&'a [String]);

impl<'a> TryFrom<&'a [String]> for Argv<'a> {
    type Error = RuntimeError;

    fn try_from(argv: &'a [String]) -> Result<Self, Self::Error> {
        if argv.is_empty() {
            return Err(RuntimeError::Api {
                operation: Operation::Exec,
                message: "an exec needs a command".to_owned(),
            });
        }
        Ok(Self(argv))
    }
}

impl AsRef<[String]> for Argv<'_> {
    fn as_ref(&self) -> &[String] {
        self.0
    }
}

/// How one exec in a container ended.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "consumed by the exec health check")]
pub enum ExecOutcome {
    /// The command ran and exited with `code`; 0 is a pass.
    Exited { code: i64, output: ExecOutput },
    /// Podman could not execute argv[0] at all (missing, not executable, a directory). `reason`
    /// is podman's OCI runtime error, e.g. `runc: exec failed: unable to start container
    /// process: exec: "pg_isready": executable file not found in $PATH: OCI runtime attempted to
    /// invoke a command that was not found`. `code` is podman's: 125, 126 or 127.
    CannotRun {
        code: i64,
        reason: String,
        output: ExecOutput,
    },
    /// The command was still running at the timeout. The runtime tries to end it (SIGTERM, then
    /// SIGKILL to its process group) in the background; if it cannot, the exec is abandoned and
    /// runs until it ends or the container stops. `exec` tells when that happens.
    TimedOut {
        output: ExecOutput,
        exec: RunningExec,
    },
}

#[allow(dead_code, reason = "consumed by the exec health check")]
impl ExecOutcome {
    /// Podman's exit codes for "the command could not be invoked" (125) and "found but not
    /// executable" / "not found" (126, 127). A shell or script returns 126/127 too, for an inner
    /// command, so the code alone does not decide.
    const CANNOT_RUN_CODES: [i64; 3] = [125, 126, 127];

    /// The outcome of an exec that ended with `code` and printed `output`: [`Self::CannotRun`]
    /// when the code is one of podman's and the only output is podman's own one-line OCI runtime
    /// error (`Error: … OCI runtime …`), [`Self::Exited`] otherwise. A shell's
    /// `sh: pg_isready: not found` with 127 is an ordinary failure.
    pub fn finished(code: i64, output: ExecOutput) -> Self {
        let text = output.as_ref().trim();
        let podman_error = text
            .strip_prefix("Error: ")
            .filter(|rest| !rest.contains('\n') && rest.contains("OCI "));
        match podman_error {
            Some(reason) if Self::CANNOT_RUN_CODES.contains(&code) => Self::CannotRun {
                code,
                reason: reason.to_owned(),
                output,
            },
            _ => Self::Exited { code, output },
        }
    }

    /// Whether the check passed: the command exited with 0.
    pub fn passed(&self) -> bool {
        matches!(self, Self::Exited { code: 0, .. })
    }

    /// The exit code, unless the exec timed out.
    pub fn code(&self) -> Option<i64> {
        match self {
            Self::Exited { code, .. } | Self::CannotRun { code, .. } => Some(*code),
            Self::TimedOut { .. } => None,
        }
    }

    /// The tail of what the command wrote to stdout and stderr.
    pub fn output(&self) -> &ExecOutput {
        match self {
            Self::Exited { output, .. }
            | Self::CannotRun { output, .. }
            | Self::TimedOut { output, .. } => output,
        }
    }
}

/// The last [`ExecOutput::MAX_BYTES`] of an exec's stdout and stderr, interleaved as they were
/// written, decoded lossily as UTF-8. Podman's `Error: read unixpacket …` trailer, which it adds
/// to some failed execs, is left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput(String);

impl ExecOutput {
    /// How much output is kept (architecture §5.5).
    pub const MAX_BYTES: usize = 4096;
}

impl AsRef<str> for ExecOutput {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExecOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ExecOutput {
    fn from(text: &str) -> Self {
        let mut tail = OutputTail::default();
        tail.push(text.as_bytes());
        tail.into()
    }
}

/// The ring an exec's output is collected in while it runs.
#[derive(Debug, Default)]
pub(super) struct OutputTail(VecDeque<u8>);

impl OutputTail {
    /// The line podman appends when the exec's attach socket closes under it (podman 6.1.4:
    /// `Error: read unixpacket @->/proc/self/fd/13/attach: read: connection reset by peer`).
    const TRAILER: &str = "Error: read unixpacket ";

    /// Appends `bytes`, dropping the oldest beyond [`ExecOutput::MAX_BYTES`].
    pub(super) fn push(&mut self, bytes: &[u8]) {
        let keep = bytes.len().min(ExecOutput::MAX_BYTES);
        self.0.extend(&bytes[bytes.len() - keep..]);
        let excess = self.0.len().saturating_sub(ExecOutput::MAX_BYTES);
        self.0.drain(..excess);
    }

    /// The output so far.
    pub(super) fn snapshot(&self) -> ExecOutput {
        let bytes: Vec<u8> = self.0.iter().copied().collect();
        // The ring may have cut a character in two at its start.
        let start = bytes
            .iter()
            .position(|b| (b & 0xC0) != 0x80)
            .unwrap_or(bytes.len());
        let text = String::from_utf8_lossy(&bytes[start..]);
        let kept: Vec<&str> = text
            .split_inclusive('\n')
            .filter(|line| !line.starts_with(Self::TRAILER))
            .collect();
        ExecOutput(kept.concat())
    }
}

impl From<OutputTail> for ExecOutput {
    fn from(tail: OutputTail) -> Self {
        tail.snapshot()
    }
}

/// An exec that outlived its timeout, until it ends: killed by the runtime, finished on its
/// own, or ended with its container. Cloning it watches the same exec.
///
/// The health check counts the ones still running to cap abandoned execs per container
/// (architecture §5.5).
#[derive(Debug, Clone)]
pub struct RunningExec(watch::Receiver<()>);

/// Held by whoever knows when a [`RunningExec`] ends; dropping it ends it.
#[derive(Debug)]
pub struct ExecGuard(#[allow(dead_code, reason = "held only to be dropped")] watch::Sender<()>);

impl RunningExec {
    /// A running exec, and the guard that ends it when dropped.
    pub fn new() -> (Self, ExecGuard) {
        let (sender, receiver) = watch::channel(());
        (Self(receiver), ExecGuard(sender))
    }

    /// Whether the exec is still running.
    #[allow(dead_code, reason = "consumed by the exec health check")]
    pub fn is_running(&self) -> bool {
        self.0.has_changed().is_ok()
    }

    /// Resolves once the exec has ended.
    #[allow(dead_code, reason = "consumed by the exec health check")]
    pub async fn ended(&self) {
        let mut receiver = self.0.clone();
        // Nothing is ever sent: `changed` only returns once the guard is dropped.
        while receiver.changed().await.is_ok() {}
    }
}

/// The host process of a running exec, held through a pidfd so a reused PID is never signalled.
///
/// Rootless podman runs an exec as its own process-group leader, with a host PID that exec
/// inspect reports. Signalling it needs `CAP_KILL` over its user: an exec running as container
/// root runs as hoster's own uid, one running as another container user runs as a subuid, which
/// hoster's unit (`PrivateUsers=yes`) cannot signal. Such an exec is abandoned.
#[derive(Debug)]
pub(super) struct ExecProcess {
    pid: Pid,
    pidfd: OwnedFd,
}

impl ExecProcess {
    /// How long a timed-out exec gets between SIGTERM and SIGKILL.
    pub(super) const GRACE: Duration = Duration::from_secs(1);

    /// Opens a pidfd for the exec's host PID, as exec inspect reported it.
    pub(super) fn open(pid: i64) -> io::Result<Self> {
        let pid = i32::try_from(pid)
            .ok()
            .filter(|&pid| pid > 0)
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no PID"))?;
        let pidfd = pidfd_open(pid, PidfdFlags::empty())?;
        Ok(Self { pid, pidfd })
    }

    /// The host PID.
    pub(super) fn pid(&self) -> i64 {
        self.pid.as_raw_nonzero().get().into()
    }

    /// Whether the process is still alive: its pidfd is not readable yet. A failed poll counts
    /// as alive.
    fn is_alive(&self) -> bool {
        let mut fds = [PollFd::new(&self.pidfd, PollFlags::IN)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        !poll(&mut fds, Some(&now)).is_ok_and(|_| fds[0].revents().contains(PollFlags::IN))
    }

    /// Ends the exec's process group: SIGTERM, then SIGKILL after [`Self::GRACE`], also when
    /// the leader has exited by then, so a member that ignores SIGTERM goes too.
    ///
    /// The group is read first and the pidfd checked after, so the group id is the live
    /// leader's own and not a reused PID's; a group stays reserved while any member lives, and
    /// once none does the SIGKILL finds no group (`ESRCH`). Linux hands out PIDs cyclically, so
    /// the id being taken by a new group within the grace would need the whole PID space to wrap
    /// in that second. Fails without signalling anything when the process no longer leads its
    /// group or cannot be signalled (`EPERM` for an exec running as a subuid).
    pub(super) async fn terminate(self) -> io::Result<()> {
        let group = getpgid(Some(self.pid))?;
        if !self.is_alive() {
            return Ok(());
        }
        if group != self.pid {
            return Err(io::Error::other("the exec does not lead its process group"));
        }
        kill_process_group(group, Signal::TERM)?;
        tokio::time::sleep(Self::GRACE).await;
        match kill_process_group(group, Signal::KILL) {
            // Every member exited since the SIGTERM.
            Err(rustix::io::Errno::SRCH) => Ok(()),
            other => Ok(other?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podmans_own_error_is_cannot_run() {
        let not_found = "Error: runc: exec failed: unable to start container process: exec: \
                         \"pg_isready\": executable file not found in $PATH: OCI runtime \
                         attempted to invoke a command that was not found\n";
        let outcome = ExecOutcome::finished(127, ExecOutput::from(not_found));
        let ExecOutcome::CannotRun { reason, .. } = &outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(outcome.code(), Some(127));
        assert!(reason.starts_with("runc: exec failed"), "{reason}");
        assert!(reason.contains("executable file not found"), "{reason}");
        assert!(!outcome.passed());

        let denied = "Error: runc: exec failed: unable to start container process: exec: \
                      \"/etc/passwd\": permission denied: OCI permission denied\n";
        assert!(matches!(
            ExecOutcome::finished(126, denied.into()),
            ExecOutcome::CannotRun { .. }
        ));
        let directory = "Error: runc: exec failed: unable to start container process: exec: \
                         \"/etc\": is a directory: OCI runtime error\n";
        assert!(matches!(
            ExecOutcome::finished(125, directory.into()),
            ExecOutcome::CannotRun { .. }
        ));
        let crun = "Error: crun: executable file not found in $PATH: No such file or directory: \
                    OCI runtime attempted to invoke a command that was not found";
        assert!(matches!(
            ExecOutcome::finished(127, crun.into()),
            ExecOutcome::CannotRun { .. }
        ));
    }

    #[test]
    fn a_scripts_126_or_127_is_an_ordinary_failure() {
        for (code, output) in [
            (127, "sh: pg_isready: not found\n"),
            (126, "sh: /data/check: Permission denied\n"),
            (1, "Error: something OCI something\n"),
            // Podman's error plus the command's own output is not podman's error alone.
            (127, "starting\nError: x OCI runtime error\n"),
            (127, ""),
        ] {
            let outcome = ExecOutcome::finished(code, output.into());
            assert!(
                matches!(outcome, ExecOutcome::Exited { code: c, .. } if c == code),
                "{code} {output:?}: {outcome:?}"
            );
        }
        assert!(ExecOutcome::finished(0, "accepting connections\n".into()).passed());
        assert_eq!(
            ExecOutcome::finished(2, ExecOutput::default()).code(),
            Some(2)
        );
        assert!(!ExecOutcome::finished(2, ExecOutput::default()).passed());
    }

    #[test]
    fn output_keeps_the_tail_and_drops_podmans_trailer() {
        let mut tail = OutputTail::default();
        tail.push(b"Error: runc: exec failed: x: OCI runtime error\n");
        tail.push(
            b"Error: read unixpacket @->/proc/self/fd/13/attach: read: connection reset by peer\n",
        );
        assert_eq!(
            ExecOutput::from(tail).as_ref(),
            "Error: runc: exec failed: x: OCI runtime error\n"
        );

        let mut tail = OutputTail::default();
        tail.push(&[b'a'; 3000]);
        tail.push(&[b'b'; 3000]);
        let output = tail.snapshot();
        assert_eq!(output.as_ref().len(), ExecOutput::MAX_BYTES);
        assert!(output.as_ref().ends_with(&"b".repeat(3000)));
        // One push larger than the ring keeps its own tail.
        let mut tail = OutputTail::default();
        tail.push(&[b'c'; 10_000]);
        assert_eq!(tail.snapshot().as_ref(), "c".repeat(ExecOutput::MAX_BYTES));
    }

    #[test]
    fn a_character_cut_by_the_ring_is_dropped_not_garbled() {
        let mut tail = OutputTail::default();
        // "ä" is two bytes; 4095 of them leave the ring starting mid-character.
        tail.push("ä".repeat(2048).as_bytes());
        tail.push(b"!");
        let output = tail.snapshot();
        assert!(!output.as_ref().contains('\u{FFFD}'), "{output}");
        assert!(output.as_ref().ends_with("ä!"));
        assert_eq!(output.as_ref().len(), ExecOutput::MAX_BYTES - 1);
    }

    #[tokio::test]
    async fn a_running_exec_ends_when_its_guard_is_dropped() {
        let (exec, guard) = RunningExec::new();
        let watcher = exec.clone();
        assert!(exec.is_running() && watcher.is_running());
        let ended = tokio::spawn(async move { watcher.ended().await });
        tokio::task::yield_now().await;
        assert!(!ended.is_finished());
        drop(guard);
        ended.await.unwrap();
        assert!(!exec.is_running());
        exec.ended().await;
    }

    #[tokio::test]
    async fn terminating_ends_the_whole_process_group() {
        // A shell leading its own group, with a child in it: both must go.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i64::from(child.id().unwrap());
        let process = ExecProcess::open(pid).unwrap();
        assert_eq!(process.pid(), pid);
        assert!(process.is_alive());
        tokio::time::timeout(Duration::from_secs(10), process.terminate())
            .await
            .expect("terminate within the grace")
            .unwrap();
        let status = child.wait().await.unwrap();
        assert!(!status.success(), "{status:?}");
        let group = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        // The group is gone once its last member is (the orphaned sleep is reaped by init).
        let gone = tokio::time::timeout(Duration::from_secs(10), async {
            while rustix::process::test_kill_process_group(group).is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the group's sleep survived");
    }

    #[test]
    fn a_process_that_does_not_lead_its_group_is_not_signalled() {
        // This test process is not its group's leader under cargo test's harness threads, but
        // a plain child spawned without `process_group(0)` certainly is not.
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        let process = ExecProcess::open(i64::from(child.id())).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(process.terminate());
        assert!(result.is_err(), "{result:?}");
        assert!(child.try_wait().unwrap().is_none(), "it was signalled");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn a_member_that_ignores_sigterm_is_killed_after_the_grace() {
        // The leader dies on SIGTERM at once; its child ignores it.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sh -c 'trap \"\" TERM; sleep 300' & sleep 300"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i64::from(child.id().unwrap());
        let group = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        // Let the inner shell set its trap.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let process = ExecProcess::open(pid).unwrap();
        tokio::time::timeout(Duration::from_secs(10), process.terminate())
            .await
            .expect("terminate within the grace")
            .unwrap();
        child.wait().await.unwrap();
        let gone = tokio::time::timeout(Duration::from_secs(10), async {
            while rustix::process::test_kill_process_group(group).is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the member that ignored SIGTERM survived");
    }

    #[test]
    fn an_exec_needs_a_command() {
        let empty: &[String] = &[];
        assert!(matches!(
            Argv::try_from(empty),
            Err(RuntimeError::Api {
                operation: Operation::Exec,
                ..
            })
        ));
        let argv = ["true".to_owned()];
        assert_eq!(Argv::try_from(&argv[..]).unwrap().as_ref(), &argv[..]);
    }

    #[test]
    fn no_pid_is_no_process() {
        assert!(ExecProcess::open(0).is_err());
        assert!(ExecProcess::open(-1).is_err());
        assert!(ExecProcess::open(i64::MAX).is_err());
    }
}
