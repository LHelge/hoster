//! The notifier's rules, with no I/O (ADR-0058 §7–§9): which events mail and as what
//! ([`Triage`]), the hourly cap and its summary ([`HourlyCap`]), the back-off of a retried
//! delivery ([`Retry`]) and how often a failing provider writes a delivery-failed event
//! ([`FailureEvents`]). Every rule takes the time it runs at, so the tests drive them with a
//! fake clock.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use crate::orchestrator::EventTopic;
use crate::store::{
    Event, EventId, EventKind, NotifiedProblem, NotifierStep, ProblemKey, Severity, Timestamp,
};

/// One hour, the window of the cap and of stale events.
const HOUR: Duration = Duration::from_secs(60 * 60);

/// What one event means to the notifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verdict {
    /// The mail to send about it, if any.
    pub mail: Option<Mailing>,
    /// The problem it opens (or reminds of), recorded once its mail is delivered.
    pub opens: Option<ProblemKey>,
    /// The open problems it ends, forgotten whether or not a mail goes out.
    pub closes: Vec<ProblemKey>,
}

impl Verdict {
    /// Nothing to mail and nothing to record.
    fn silent() -> Self {
        Self {
            mail: None,
            opens: None,
            closes: Vec::new(),
        }
    }

    /// A mail about the event, recording nothing.
    fn mail() -> Self {
        Self {
            mail: Some(Mailing::Event),
            ..Self::silent()
        }
    }

    /// A mail about the event that opens `key`.
    fn opening(mailing: Mailing, key: ProblemKey) -> Self {
        Self {
            mail: Some(mailing),
            opens: Some(key),
            closes: Vec::new(),
        }
    }

    /// Forgets `closes` without a mail.
    fn closing(closes: Vec<ProblemKey>) -> Self {
        Self {
            closes,
            ..Self::silent()
        }
    }

    /// The step that stores this verdict for `event`: the cursor moves past it, the problem it
    /// opens is recorded if `delivered`, and the ones it closes are forgotten.
    pub fn step(&self, event: &Event, delivered: bool, now: Timestamp) -> NotifierStep {
        NotifierStep {
            cursor: event.id,
            notified: self
                .opens
                .iter()
                .filter(|_| delivered)
                .map(|key| NotifiedProblem {
                    key: key.clone(),
                    event: event.id,
                    at: now,
                })
                .collect(),
            closed: self.closes.clone(),
        }
    }
}

/// How an event mails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mailing {
    /// As itself: a new problem, a recovery or a notice.
    Event,
    /// As a reminder: the problem is still failing a day after its last mail.
    Reminder,
}

/// What an event is to the notifier (ADR-0058 §7).
///
/// Beyond the ADR's table, three rules keep the open problems true to what the producers still
/// know (`docs/architecture.md`, the notifier):
///
/// - A `DeployFailed` of severity warning (a cancelled deploy, or one ended by removing its
///   stack) is the operator's doing, not a failure, and is never mailed.
/// - A successful deploy, or a rollback that went live, replaces the stack's generations, so it
///   forgets their crash loops and health problems: the reconciler and the health monitor write
///   nothing more about the old ones, and a new generation's failure is a new state change.
/// - `HosterStarted` forgets every crash loop and health problem, which those two keep in
///   memory and write again if it persists; `StackRemoved` forgets every problem of the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Class {
    /// A deploy that failed or was rolled back: each is its own mail, and it marks the stack's
    /// next successful deploy as a recovery. A rollback that went live also forgets the
    /// problems of `forgets`, without a mail for them.
    DeployProblem {
        key: ProblemKey,
        forgets: Option<HousekeepingScope>,
    },
    /// A problem that is held back while it is open.
    Problem(ProblemKey),
    /// Ends the problems in the list that are open, and is mailed if one was. A success kind
    /// also mails with "also successes". A host's problem ends whichever stack it was opened
    /// under. It also forgets the problems of `forgets`, without a mail for them.
    Recovery {
        closes: Vec<ProblemKey>,
        success: bool,
        forgets: Option<HousekeepingScope>,
    },
    /// Always mailed, recording nothing.
    Always,
    /// Never mailed; forgets the problems in the list without a mail.
    Housekeeping(HousekeepingScope),
    /// Never mailed.
    Never,
}

/// Which open problems an event ends without a mail about them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HousekeepingScope {
    /// Every problem of the stack: it was removed, and its services, deploys and hosts with it.
    Stack(String),
    /// The crash loops and health problems of the stack: a deploy replaced its generations.
    Generations(String),
    /// The crash loops and health problems: hoster started again, and the reconciler and the
    /// health monitor, which keep those states in memory, write them again if they persist.
    Restart,
}

impl HousekeepingScope {
    /// Whether the scope takes in `key`.
    fn covers(&self, key: &ProblemKey) -> bool {
        let generation = matches!(
            key.kind,
            EventKind::CrashLoopEntered | EventKind::HealthChanged
        );
        match self {
            Self::Stack(stack) => key.stack.as_ref() == Some(stack),
            Self::Generations(stack) => generation && key.stack.as_ref() == Some(stack),
            Self::Restart => generation,
        }
    }
}

/// When the last mail about an open problem went out, and about which event.
#[derive(Debug, Clone, Copy)]
struct Mailed {
    at: Timestamp,
    event: EventId,
}

impl Class {
    fn of(event: &Event) -> Self {
        // The key of a problem of this kind about the event's stack and topic: its service or
        // host. Without a topic, its page stands in.
        let key = |kind: EventKind, stack: Option<String>| ProblemKey {
            kind,
            stack,
            subject: EventTopic::of(event)
                .map(str::to_owned)
                .or_else(|| event.subject.clone()),
        };
        // A service's problem, or a host's: both belong to the event's stack, so removing the
        // stack forgets them. A host's recovery ends its problem under any stack.
        let service = |kind| key(kind, event.stack.clone());
        let deploy = || ProblemKey {
            kind: EventKind::DeployFailed,
            stack: event.stack.clone(),
            subject: None,
        };
        match event.kind {
            // A cancelled deploy, or one ended by removing its stack, is the operator's doing
            // and a warning, not a failure to mail.
            EventKind::DeployFailed if event.severity == Severity::Error => Self::DeployProblem {
                key: deploy(),
                forgets: None,
            },
            // A rollback is only recorded once it is live (a failed one is `DeployFailed`), and
            // it replaced the stack's generations as a deploy does.
            EventKind::DeployRolledBack => Self::DeployProblem {
                key: deploy(),
                forgets: event.stack.clone().map(HousekeepingScope::Generations),
            },
            EventKind::DeploySucceeded => Self::Recovery {
                closes: vec![deploy()],
                success: true,
                forgets: event.stack.clone().map(HousekeepingScope::Generations),
            },
            EventKind::CrashLoopEntered => Self::Problem(service(EventKind::CrashLoopEntered)),
            EventKind::CrashLoopLeft => Self::Recovery {
                closes: vec![service(EventKind::CrashLoopEntered)],
                success: false,
                forgets: None,
            },
            EventKind::HealthChanged if event.severity == Severity::Success => Self::Recovery {
                closes: vec![service(EventKind::HealthChanged)],
                success: false,
                forgets: None,
            },
            EventKind::HealthChanged => Self::Problem(service(EventKind::HealthChanged)),
            EventKind::CertificateFailed => Self::Problem(service(EventKind::CertificateFailed)),
            EventKind::DnsMismatch => Self::Problem(service(EventKind::DnsMismatch)),
            EventKind::CertificateIssued | EventKind::CertificateRenewed => Self::Recovery {
                closes: vec![
                    service(EventKind::CertificateFailed),
                    service(EventKind::DnsMismatch),
                ],
                success: true,
                forgets: None,
            },
            // A rollback of hoster is one incident, not a state that repeats.
            EventKind::HosterRolledBack | EventKind::HosterUpdated => Self::Always,
            EventKind::StackRemoved => match &event.stack {
                Some(stack) => Self::Housekeeping(HousekeepingScope::Stack(stack.clone())),
                None => Self::Never,
            },
            EventKind::HosterStarted => Self::Housekeeping(HousekeepingScope::Restart),
            EventKind::DeployFailed
            | EventKind::DeployStarted
            | EventKind::ContainerRestarted
            | EventKind::StackApplied
            | EventKind::SecretSet
            | EventKind::SecretDeleted
            | EventKind::NotificationFailed => Self::Never,
        }
    }
}

/// Which events mail, and whether a problem is new, a repeat or due a reminder (ADR-0058 §7,
/// §8). It holds the open problems that were mailed, as the store keeps them, and is told of
/// every step stored after it was loaded.
#[derive(Debug, Clone, Default)]
pub(super) struct Triage {
    /// Open problems, by key: the last mail about each.
    open: HashMap<ProblemKey, Mailed>,
    /// Also mail every successful deploy and certificate.
    also_successes: bool,
}

impl Triage {
    /// An open problem is mailed again as a reminder this long after its last mail: when a
    /// repeat of it arrives, or else by [`Triage::reminders`].
    pub const REMINDER: Duration = Duration::from_secs(24 * 60 * 60);
    /// Events older than this when the notifier reads them are not mailed, and the problems
    /// they open are not recorded; they still close problems. So neither a restart after an
    /// outage nor a backlog sends stale mail.
    pub const STALE: Duration = HOUR;

    /// The rules over the open `problems` the store keeps.
    pub fn new(problems: impl IntoIterator<Item = NotifiedProblem>, also_successes: bool) -> Self {
        Self {
            open: problems
                .into_iter()
                .map(|p| {
                    let mailed = Mailed {
                        at: p.at,
                        event: p.event,
                    };
                    (p.key, mailed)
                })
                .collect(),
            also_successes,
        }
    }

    /// What to do with `event`, read at `now`.
    pub fn judge(&self, event: &Event, now: Timestamp) -> Verdict {
        let verdict = match Class::of(event) {
            Class::DeployProblem { key, forgets } => Verdict {
                closes: self
                    .matching(|open| forgets.as_ref().is_some_and(|scope| scope.covers(open))),
                ..Verdict::opening(Mailing::Event, key)
            },
            Class::Problem(key) => match self.open.get(&key) {
                None => Verdict::opening(Mailing::Event, key),
                Some(last) if now >= last.at + Self::REMINDER => {
                    Verdict::opening(Mailing::Reminder, key)
                }
                Some(_) => Verdict::silent(),
            },
            Class::Recovery {
                closes,
                success,
                forgets,
            } => {
                let host = |key: &ProblemKey| {
                    matches!(
                        key.kind,
                        EventKind::CertificateFailed | EventKind::DnsMismatch
                    )
                };
                let ends = |open: &ProblemKey| {
                    closes.iter().any(|key| {
                        key.kind == open.kind
                            && key.subject == open.subject
                            && (host(key) || key.stack == open.stack)
                    })
                };
                let ended = self.open.keys().any(ends);
                let mail = ended || (success && self.also_successes);
                Verdict {
                    mail: mail.then_some(Mailing::Event),
                    opens: None,
                    closes: self.matching(|open| {
                        ends(open) || forgets.as_ref().is_some_and(|scope| scope.covers(open))
                    }),
                }
            }
            Class::Always => Verdict::mail(),
            Class::Housekeeping(scope) => Verdict::closing(self.matching(|key| scope.covers(key))),
            Class::Never => Verdict::silent(),
        };
        if event.at + Self::STALE < now {
            // Too old to mail: the problem it opens was not mailed, so it is not recorded.
            return Verdict::closing(verdict.closes);
        }
        verdict
    }

    /// The open problems that are due a reminder at `now`, a day after their last mail, as
    /// that mail recorded them. A deploy problem is never reminded of: each deploy is its own
    /// mail, and the key only waits for the stack's next success.
    pub fn reminders(&self, now: Timestamp) -> Vec<NotifiedProblem> {
        let mut due: Vec<_> = self
            .open
            .iter()
            .filter(|(key, last)| {
                key.kind != EventKind::DeployFailed && now >= last.at + Self::REMINDER
            })
            .map(|(key, last)| NotifiedProblem {
                key: key.clone(),
                event: last.event,
                at: last.at,
            })
            .collect();
        due.sort_by(|a, b| (a.at, &a.key).cmp(&(b.at, &b.key)));
        due
    }

    /// The step that records a reminder about `problem` sent (or attempted) at `now`: the next
    /// one is due a day later either way, so a failing provider is not asked every minute.
    pub fn reminded(problem: &NotifiedProblem, now: Timestamp) -> NotifierStep {
        NotifierStep {
            notified: vec![NotifiedProblem {
                at: now,
                ..problem.clone()
            }],
            ..NotifierStep::to(problem.event)
        }
    }

    /// The open problems `pick` takes, in key order.
    fn matching(&self, pick: impl Fn(&ProblemKey) -> bool) -> Vec<ProblemKey> {
        let mut keys: Vec<_> = self.open.keys().filter(|key| pick(key)).cloned().collect();
        keys.sort();
        keys
    }

    /// Takes in `step`, which the store has just stored.
    pub fn settle(&mut self, step: &NotifierStep) {
        for key in &step.closed {
            self.open.remove(key);
        }
        for problem in &step.notified {
            let mailed = Mailed {
                at: problem.at,
                event: problem.event,
            };
            self.open.insert(problem.key.clone(), mailed);
        }
    }
}

/// The cap of [`HourlyCap::LIMIT`] mails in any hour (ADR-0058 §8). A mail beyond it is
/// counted instead, and once the window has room again one summary mail says how many there
/// were. The summary counts as a mail itself.
///
/// The cap lives in memory: a restart starts it afresh, and forgets the count of an unsent
/// summary.
#[derive(Debug, Clone, Default)]
pub(super) struct HourlyCap {
    /// When each mail of the last hour went out, oldest first.
    sent: VecDeque<Timestamp>,
    /// The mails held back since the last summary.
    suppressed: u32,
}

impl HourlyCap {
    /// The most mails in any hour. The summary's text says so (`mail.rs`).
    pub const LIMIT: usize = 20;

    /// Whether a mail may go out at `now`; counts it either way.
    pub fn admit(&mut self, now: Timestamp) -> bool {
        if self.room(now) && self.suppressed == 0 {
            self.sent.push_back(now);
            true
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            false
        }
    }

    /// The number of held-back mails, when a summary of them may go out at `now`. Taking it
    /// counts the summary as sent and starts the count again.
    pub fn summary(&mut self, now: Timestamp) -> Option<u32> {
        if self.suppressed == 0 || !self.room(now) {
            return None;
        }
        self.sent.push_back(now);
        Some(std::mem::take(&mut self.suppressed))
    }

    /// Whether the last hour, up to `now`, has room for another mail.
    fn room(&mut self, now: Timestamp) -> bool {
        while self.sent.front().is_some_and(|&at| at + HOUR <= now) {
            self.sent.pop_front();
        }
        self.sent.len() < Self::LIMIT
    }
}

/// The back-off of a delivery that failed with a retryable error (ADR-0058 §9): the waits
/// double from `first` up to `max`, honour the provider's `retry-after`, and the mail is dropped
/// once the next attempt would start after `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Retry {
    pub first: Duration,
    pub max: Duration,
    pub window: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(5),
            max: Duration::from_secs(60),
            window: Duration::from_secs(5 * 60),
        }
    }
}

impl Retry {
    /// How long to wait after the `failures`th failed attempt (1 for the first), of which the
    /// provider asked for `asked`; `None` once that wait would end more than `window` after the
    /// first attempt, `elapsed` ago: the mail is then dropped.
    pub fn wait(
        &self,
        failures: u32,
        asked: Option<Duration>,
        elapsed: Duration,
    ) -> Option<Duration> {
        let doublings = failures.saturating_sub(1).min(16);
        let wait = self
            .first
            .saturating_mul(1 << doublings)
            .min(self.max)
            .max(asked.unwrap_or_default());
        (elapsed.saturating_add(wait) <= self.window).then_some(wait)
    }
}

/// At most one delivery-failed event an hour while the provider keeps failing (ADR-0058 §9),
/// so a revoked key does not write one per event.
#[derive(Debug, Clone, Default)]
pub(super) struct FailureEvents {
    last: Option<Timestamp>,
}

impl FailureEvents {
    /// The shortest time between two delivery-failed events.
    pub const INTERVAL: Duration = HOUR;

    /// Whether a delivery that failed at `now` writes an event; counts it if so.
    pub fn due(&mut self, now: Timestamp) -> bool {
        let due = self.last.is_none_or(|last| last + Self::INTERVAL <= now);
        if due {
            self.last = Some(now);
        }
        due
    }
}

#[cfg(test)]
mod tests;
