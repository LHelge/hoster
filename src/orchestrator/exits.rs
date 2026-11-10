//! Naming why a container stopped (ADR-0059 §6): the exit reason of a reconciler restart
//! (ADR-0028) and of a container that exits while a deploy probes it (ADR-0038).
//!
//! Podman's `State.OOMKilled` is sticky for a container's lifetime (architecture §1.8): once
//! the OOM killer hit its cgroup, every later exit reports it, also after a `start`. So hoster
//! keeps the flag it last saw per container ([`OomFlags`]): false at create, read before each
//! start it does and when it first sees the container running (adoption at startup included).
//! An exit hoster did not cause then reads:
//!
//! - exit 137 with the flag true: "OOM-killed (limit 512MiB)", since an unsent SIGKILL is almost
//!   always the OOM killer, whatever the flag was before;
//! - any other exit where the flag turned true since hoster last saw it: "exited 1 after an OOM
//!   kill (limit 512MiB)";
//! - anything else as before: "exit 1".
//!
//! "(no memory limit)" stands in for the limit when the service sets none. The flags live in
//! memory only; after a hoster restart a container is first seen again, so an OOM in a run
//! hoster did not see start is named only if it exits 137.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tokio::time::Instant;

use super::Orchestrator;
use crate::runtime::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerState, ServiceGeneration,
};
use crate::stack::ByteSize;

/// How a run ended, as inspect reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Exit {
    pub(super) state: ContainerState,
    /// `State.OOMKilled`, sticky for the container's lifetime.
    pub(super) oom_killed: bool,
}

impl From<&ContainerInfo> for Exit {
    fn from(info: &ContainerInfo) -> Self {
        Self {
            state: info.state,
            oom_killed: info.oom_killed,
        }
    }
}

impl Exit {
    /// The SIGKILL exit status, what a PID 1 the OOM killer ended exits with.
    const SIGKILL: i64 = 137;

    /// Why the run ended, given the flag hoster last saw for the container (`None` when it has
    /// not seen it since it started) and the memory limit it was created with.
    pub(super) fn reason(self, seen: Option<bool>, limit: Option<ByteSize>) -> ExitReason {
        let limit = MemoryLimit(limit);
        if !self.oom_killed {
            return ExitReason::Exit(self.state);
        }
        match self.state {
            ContainerState::Exited(Some(Self::SIGKILL)) => ExitReason::OomKilled(limit),
            state if seen == Some(false) => ExitReason::AfterOomKill { state, limit },
            state => ExitReason::Exit(state),
        }
    }
}

/// Why a container stopped, or why the reconciler's restart of it did not take. Its `Display`
/// is what the restart event, the crash-loop detail, the dashboard warning and a deploy abort
/// say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Ended without an OOM kill hoster can name: `exit 1`, or the state podman reports.
    Exit(ContainerState),
    /// Exit 137 with `OOMKilled` true.
    OomKilled(MemoryLimit),
    /// Another exit, after `OOMKilled` turned true since hoster last saw the container.
    AfterOomKill {
        state: ContainerState,
        limit: MemoryLimit,
    },
    /// The restart failed with this start error, e.g. a cgroup controller undelegated after
    /// apply (HTTP 500, the container left `created`).
    StartFailed(String),
}

impl ExitReason {
    /// Whether it names an OOM kill.
    pub fn is_oom(&self) -> bool {
        matches!(self, Self::OomKilled(_) | Self::AfterOomKill { .. })
    }
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exit(ContainerState::Exited(Some(code))) => write!(f, "exit {code}"),
            Self::Exit(state) => write!(f, "{state}"),
            Self::OomKilled(limit) => write!(f, "OOM-killed ({limit})"),
            Self::AfterOomKill {
                state: ContainerState::Exited(Some(code)),
                limit,
            } => write!(f, "exited {code} after an OOM kill ({limit})"),
            Self::AfterOomKill { state, limit } => write!(f, "{state} after an OOM kill ({limit})"),
            Self::StartFailed(err) => write!(f, "cannot start: {err}"),
        }
    }
}

/// A service's memory limit as an exit reason names it: `limit 512MiB`, or `no memory limit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimit(pub Option<ByteSize>);

impl fmt::Display for MemoryLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(limit) => write!(f, "limit {limit}"),
            None => f.write_str("no memory limit"),
        }
    }
}

/// The `OOMKilled` flag hoster last saw per container, with when it saw it.
#[derive(Debug, Default)]
pub(super) struct OomFlags(BTreeMap<ServiceGeneration, (bool, Instant)>);

impl OomFlags {
    fn see(&mut self, key: &ServiceGeneration, flag: bool) {
        self.0.insert(key.clone(), (flag, Instant::now()));
    }

    fn forget(&mut self, key: &ServiceGeneration) {
        self.0.remove(key);
    }

    fn get(&self, key: &ServiceGeneration) -> Option<bool> {
        self.0.get(key).map(|&(flag, _)| flag)
    }

    /// Keeps the flags of the containers `present` in a listing made at `listed_at`, and those
    /// seen since: a container created after the listing is not in it.
    pub(super) fn retain(&mut self, present: &BTreeSet<ServiceGeneration>, listed_at: Instant) {
        self.0
            .retain(|key, &mut (_, at)| present.contains(key) || at >= listed_at);
    }
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// Hoster just created `key`'s container: its flag is false.
    pub(super) fn created(&self, key: &ServiceGeneration) {
        self.reconciler().oom.see(key, false);
    }

    /// Hoster sees `key`'s container `id` running: the first time, it reads the flag (the
    /// listing never reports it), e.g. for a container adopted at startup.
    pub(super) async fn seen_running(&self, key: &ServiceGeneration, id: &ContainerId) {
        if self.reconciler().oom.get(key).is_none() {
            self.read_flag(id).await;
        }
    }

    /// Reads `id`'s flag before hoster starts it, so an OOM before this start is not blamed on
    /// a later exit.
    pub(super) async fn before_start(&self, id: &ContainerId) {
        self.read_flag(id).await;
    }

    async fn read_flag(&self, id: &ContainerId) {
        match self.runtime.inspect(id).await {
            Ok(info) => {
                if let Ok(key) = ServiceGeneration::try_from(&info) {
                    self.reconciler().oom.see(&key, info.oom_killed);
                }
            }
            Err(err) => tracing::debug!(container = %id, %err, "cannot read the OOMKilled flag"),
        }
    }

    /// Why `key`'s container stopped as `exit` (from inspect), with the service's memory
    /// `limit`; the flag it reported becomes the one hoster last saw. `exit` is `None` when
    /// inspect failed and only `listed`, the listing's state, is known: the flag is forgotten.
    pub(super) fn exit_reason(
        &self,
        key: &ServiceGeneration,
        exit: Option<Exit>,
        listed: ContainerState,
        limit: Option<ByteSize>,
    ) -> ExitReason {
        let mut reconciler = self.reconciler();
        let Some(exit) = exit else {
            reconciler.oom.forget(key);
            return ExitReason::Exit(listed);
        };
        let reason = exit.reason(reconciler.oom.get(key), limit);
        reconciler.oom.see(key, exit.oom_killed);
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit(code: i64, oom_killed: bool) -> Exit {
        Exit {
            state: ContainerState::Exited(Some(code)),
            oom_killed,
        }
    }

    const LIMIT: Option<ByteSize> = Some(ByteSize::mebibytes(512));

    #[test]
    fn a_137_with_the_flag_is_an_oom_kill_whatever_was_seen() {
        for seen in [None, Some(false), Some(true)] {
            let reason = exit(137, true).reason(seen, LIMIT);
            assert_eq!(reason.to_string(), "OOM-killed (limit 512MiB)", "{seen:?}");
            assert!(reason.is_oom());
        }
        assert_eq!(
            exit(137, true).reason(None, None).to_string(),
            "OOM-killed (no memory limit)"
        );
    }

    #[test]
    fn another_exit_names_the_oom_only_when_the_flag_turned_true() {
        let reason = exit(0, true).reason(Some(false), LIMIT);
        assert_eq!(
            reason.to_string(),
            "exited 0 after an OOM kill (limit 512MiB)"
        );
        assert!(reason.is_oom());
        assert_eq!(
            exit(1, true).reason(Some(false), None).to_string(),
            "exited 1 after an OOM kill (no memory limit)"
        );
        // Sticky and already seen, or never seen (adopted with the flag true): not an OOM.
        for seen in [Some(true), None] {
            let reason = exit(1, true).reason(seen, LIMIT);
            assert_eq!(reason.to_string(), "exit 1", "{seen:?}");
            assert!(!reason.is_oom());
        }
    }

    #[test]
    fn an_exit_without_the_flag_reads_as_before() {
        assert_eq!(
            exit(137, false).reason(Some(false), LIMIT).to_string(),
            "exit 137"
        );
        assert_eq!(exit(1, false).reason(None, LIMIT).to_string(), "exit 1");
        let unknown = Exit {
            state: ContainerState::Exited(None),
            oom_killed: true,
        };
        assert_eq!(
            unknown.reason(Some(false), LIMIT).to_string(),
            "exited after an OOM kill (limit 512MiB)"
        );
        assert_eq!(
            ExitReason::Exit(ContainerState::Created).to_string(),
            "created"
        );
        assert_eq!(
            ExitReason::StartFailed("podman: boom".into()).to_string(),
            "cannot start: podman: boom"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flags_of_containers_missing_from_a_listing_are_dropped_unless_newer() {
        let key = |generation| ServiceGeneration {
            stack: "notes".parse().unwrap(),
            service: "api".parse().unwrap(),
            generation,
        };
        let mut flags = OomFlags::default();
        flags.see(&key(1), true);
        flags.see(&key(2), false);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let listed_at = Instant::now();
        // Created after the listing: not in it, but kept.
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        flags.see(&key(3), false);
        flags.retain(&BTreeSet::from([key(2)]), listed_at);
        assert_eq!(flags.get(&key(1)), None);
        assert_eq!(flags.get(&key(2)), Some(false));
        assert_eq!(flags.get(&key(3)), Some(false));
    }
}
