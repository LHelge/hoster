//! The deploy state machine (docs/architecture.md §5.1) as a pure function:
//! `(state, event) -> (state, effects)`.
//!
//! [`DeploymentState::on`] is the whole machine. It knows nothing about podman, the store or the
//! proxy: each transition says which [`Effect`]s to run, and the deploy task's interpreter runs
//! them and feeds the [`Event`] they produce back in. The states, edges and effects are final;
//! milestones only change how an effect is carried out (M3: the built-in health check, no
//! `RouteTable` publish, immediate drain; M6: real probes, the swap and the inflight drain).

use std::fmt;

use crate::store::DeploymentState;

/// Something that happened to a deployment: the input of the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `Received`: every target (digest, config hash) is already running.
    Unchanged,
    /// `Received`: another deployment of the stack is active.
    Busy,
    /// `Received`: there is something to deploy.
    Accepted,
    /// `Queued`: the active deployment ended; this one is next.
    Dequeued,
    /// `Queued`: a newer request for the stack replaced this one (ADR-0040).
    Superseded,
    /// `Validating`: the stack, its routes or the port allocation was refused.
    Invalid(String),
    /// `Validating`: generation assigned and ports reserved.
    Validated,
    /// `Pulling`: every changed image is available.
    Pulled,
    /// `Starting`: every new start-first container runs.
    Started,
    /// `HealthChecking`: every new start-first container is healthy.
    Healthy,
    /// `Replacing`: every stop-first service is replaced and healthy.
    Replaced,
    /// `Pulling`, `Starting`, `HealthChecking` or `Replacing` failed.
    Failed(String),
    /// `Aborting`: the stack is back as it was; carries why the deployment failed.
    Aborted(String),
    /// `Switching`: routes published and per-service pointers committed.
    Switched,
    /// `Draining`: the old upstreams are idle, or the drain timeout passed.
    Drained,
    /// `CleaningUp`: the old generation is gone.
    CleanedUp,
    /// `CleaningUp`: something of the old generation could not be removed.
    CleanupFailed(String),
}

/// Work a transition asks for. Effects are data; the deploy task's interpreter carries them out.
/// The work effect of a state produces the [`Event`] that moves it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Record the transition with a timestamp (the store's transition log).
    Persist {
        to: DeploymentState,
        message: Option<String>,
    },
    /// Compare the target with what runs → [`Event::Unchanged`] or [`Event::Accepted`].
    CheckRunning,
    /// Re-validate the stack, check route collisions, assign the generation, reserve ports.
    Validate,
    /// Pull every changed image by digest.
    PullImages,
    /// Ensure the stack network, then create and start the new start-first containers.
    StartContainers,
    /// Probe the new start-first containers.
    Probe,
    /// Replace the stop-first services one at a time (ADR-0048).
    ReplaceStopFirst,
    /// Publish the `RouteTable` pointing at the new upstreams (M3: nothing to publish).
    PublishRoutes,
    /// Write every per-service current pointer in one transaction: the commit point.
    Commit,
    /// Wait for the old upstreams to go idle (M3: completes at once).
    Drain,
    /// Stop and remove the old generation, release its ports.
    CleanUp,
    /// Undo the deployment: remove new containers, restart stopped ones, release ports.
    Abort { reason: String },
    /// The deployment is over: free the stack's deploy slot.
    Finish,
}

/// The result of one step of the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub to: DeploymentState,
    pub effects: Vec<Effect>,
}

/// An event that has no edge from the current state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no transition from {from} on {event:?}")]
pub struct InvalidTransition {
    pub from: DeploymentState,
    pub event: Event,
}

impl DeploymentState {
    /// The machine: where `event` leads from this state, and what to do on arrival.
    pub fn on(self, event: Event) -> Result<Transition, InvalidTransition> {
        use DeploymentState as S;
        use Event as E;
        let (to, message) = match (self, &event) {
            (S::Received, E::Unchanged) => (S::Noop, None),
            (S::Received, E::Busy) => (S::Queued, Some("another deployment is active".into())),
            (S::Received, E::Accepted) => (S::Validating, None),
            (S::Queued, E::Dequeued) => (S::Received, None),
            (S::Queued, E::Superseded) => {
                (S::Superseded, Some("replaced by a newer deployment".into()))
            }
            (S::Validating, E::Invalid(reason)) => (S::Rejected, Some(reason.clone())),
            (S::Validating, E::Validated) => (S::Pulling, None),
            (S::Pulling, E::Pulled) => (S::Starting, None),
            (S::Starting, E::Started) => (S::HealthChecking, None),
            (S::HealthChecking, E::Healthy) => (S::Replacing, None),
            (S::Replacing, E::Replaced) => (S::Switching, None),
            (S::Pulling | S::Starting | S::HealthChecking | S::Replacing, E::Failed(reason)) => {
                (S::Aborting, Some(reason.clone()))
            }
            (S::Aborting, E::Aborted(reason)) => (S::Failed, Some(reason.clone())),
            (S::Switching, E::Switched) => (S::Draining, None),
            (S::Draining, E::Drained) => (S::CleaningUp, None),
            (S::CleaningUp, E::CleanedUp) => (S::Live, None),
            (S::CleaningUp, E::CleanupFailed(reason)) => {
                (S::LiveWithWarnings, Some(reason.clone()))
            }
            _ => return Err(InvalidTransition { from: self, event }),
        };
        let work = match to {
            S::Received => vec![Effect::CheckRunning],
            S::Validating => vec![Effect::Validate],
            S::Pulling => vec![Effect::PullImages],
            S::Starting => vec![Effect::StartContainers],
            S::HealthChecking => vec![Effect::Probe],
            S::Replacing => vec![Effect::ReplaceStopFirst],
            S::Switching => vec![Effect::PublishRoutes, Effect::Commit],
            S::Draining => vec![Effect::Drain],
            S::CleaningUp => vec![Effect::CleanUp],
            S::Aborting => vec![Effect::Abort {
                reason: message.clone().unwrap_or_default(),
            }],
            // A queued deployment waits for `Dequeued`; a superseded one never held the slot.
            S::Queued | S::Superseded => Vec::new(),
            S::Noop | S::Rejected | S::Failed | S::Live | S::LiveWithWarnings => {
                vec![Effect::Finish]
            }
        };
        let mut effects = vec![Effect::Persist { to, message }];
        effects.extend(work);
        Ok(Transition { to, effects })
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason)
            | Self::Failed(reason)
            | Self::Aborted(reason)
            | Self::CleanupFailed(reason) => f.write_str(reason),
            other => fmt::Debug::fmt(other, f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DeploymentState as S;

    const ALL_STATES: [S; 17] = [
        S::Received,
        S::Queued,
        S::Noop,
        S::Validating,
        S::Rejected,
        S::Pulling,
        S::Starting,
        S::HealthChecking,
        S::Replacing,
        S::Switching,
        S::Draining,
        S::CleaningUp,
        S::Aborting,
        S::Failed,
        S::Live,
        S::LiveWithWarnings,
        S::Superseded,
    ];

    fn all_events() -> Vec<Event> {
        let r = || "boom".to_owned();
        vec![
            Event::Unchanged,
            Event::Busy,
            Event::Accepted,
            Event::Dequeued,
            Event::Superseded,
            Event::Invalid(r()),
            Event::Validated,
            Event::Pulled,
            Event::Started,
            Event::Healthy,
            Event::Replaced,
            Event::Failed(r()),
            Event::Aborted(r()),
            Event::Switched,
            Event::Drained,
            Event::CleanedUp,
            Event::CleanupFailed(r()),
        ]
    }

    fn go(from: S, event: Event) -> Transition {
        from.on(event).unwrap()
    }

    fn work(t: &Transition) -> &[Effect] {
        match t.effects.split_first() {
            Some((Effect::Persist { to, .. }, rest)) => {
                assert_eq!(*to, t.to, "the first effect persists the new state");
                rest
            }
            other => panic!("transition does not persist first: {other:?}"),
        }
    }

    fn message(t: &Transition) -> Option<&str> {
        match &t.effects[0] {
            Effect::Persist { message, .. } => message.as_deref(),
            _ => unreachable!(),
        }
    }

    // One test per edge of the §5.1 diagram.

    #[test]
    fn received_unchanged_is_noop() {
        let t = go(S::Received, Event::Unchanged);
        assert_eq!(t.to, S::Noop);
        assert_eq!(work(&t), [Effect::Finish]);
    }

    #[test]
    fn received_busy_is_queued_without_work() {
        let t = go(S::Received, Event::Busy);
        assert_eq!(t.to, S::Queued);
        assert!(work(&t).is_empty());
    }

    #[test]
    fn queued_dequeued_is_received_and_rechecks() {
        let t = go(S::Queued, Event::Dequeued);
        assert_eq!(t.to, S::Received);
        assert_eq!(work(&t), [Effect::CheckRunning]);
    }

    #[test]
    fn queued_superseded_ends_without_freeing_the_slot() {
        let t = go(S::Queued, Event::Superseded);
        assert_eq!(t.to, S::Superseded);
        assert!(work(&t).is_empty());
        assert!(t.to.is_terminal());
    }

    #[test]
    fn received_accepted_validates() {
        let t = go(S::Received, Event::Accepted);
        assert_eq!(t.to, S::Validating);
        assert_eq!(work(&t), [Effect::Validate]);
    }

    #[test]
    fn validating_invalid_is_rejected_with_reason() {
        let t = go(S::Validating, Event::Invalid("route collision".into()));
        assert_eq!(t.to, S::Rejected);
        assert_eq!(message(&t), Some("route collision"));
        assert_eq!(work(&t), [Effect::Finish]);
    }

    #[test]
    fn validating_validated_pulls() {
        let t = go(S::Validating, Event::Validated);
        assert_eq!(t.to, S::Pulling);
        assert_eq!(work(&t), [Effect::PullImages]);
    }

    #[test]
    fn pulling_pulled_starts() {
        let t = go(S::Pulling, Event::Pulled);
        assert_eq!(t.to, S::Starting);
        assert_eq!(work(&t), [Effect::StartContainers]);
    }

    #[test]
    fn starting_started_health_checks() {
        let t = go(S::Starting, Event::Started);
        assert_eq!(t.to, S::HealthChecking);
        assert_eq!(work(&t), [Effect::Probe]);
    }

    #[test]
    fn health_checking_healthy_replaces_stop_first() {
        let t = go(S::HealthChecking, Event::Healthy);
        assert_eq!(t.to, S::Replacing);
        assert_eq!(work(&t), [Effect::ReplaceStopFirst]);
    }

    #[test]
    fn replacing_replaced_switches_routes_then_commits() {
        let t = go(S::Replacing, Event::Replaced);
        assert_eq!(t.to, S::Switching);
        assert_eq!(work(&t), [Effect::PublishRoutes, Effect::Commit]);
    }

    #[test]
    fn failures_before_the_switch_abort() {
        for from in [S::Pulling, S::Starting, S::HealthChecking, S::Replacing] {
            let t = go(from, Event::Failed("exited".into()));
            assert_eq!(t.to, S::Aborting, "{from}");
            assert_eq!(message(&t), Some("exited"));
            assert_eq!(
                work(&t),
                [Effect::Abort {
                    reason: "exited".into()
                }]
            );
        }
    }

    #[test]
    fn aborting_aborted_fails_with_reason() {
        let t = go(S::Aborting, Event::Aborted("pull failed".into()));
        assert_eq!(t.to, S::Failed);
        assert_eq!(message(&t), Some("pull failed"));
        assert_eq!(work(&t), [Effect::Finish]);
    }

    #[test]
    fn switching_switched_drains() {
        let t = go(S::Switching, Event::Switched);
        assert_eq!(t.to, S::Draining);
        assert_eq!(work(&t), [Effect::Drain]);
    }

    #[test]
    fn draining_drained_cleans_up() {
        let t = go(S::Draining, Event::Drained);
        assert_eq!(t.to, S::CleaningUp);
        assert_eq!(work(&t), [Effect::CleanUp]);
    }

    #[test]
    fn cleaning_up_ends_live() {
        let t = go(S::CleaningUp, Event::CleanedUp);
        assert_eq!(t.to, S::Live);
        assert_eq!(work(&t), [Effect::Finish]);
    }

    #[test]
    fn cleanup_failure_ends_live_with_warnings() {
        let t = go(S::CleaningUp, Event::CleanupFailed("remove failed".into()));
        assert_eq!(t.to, S::LiveWithWarnings);
        assert_eq!(message(&t), Some("remove failed"));
        assert_eq!(work(&t), [Effect::Finish]);
    }

    #[test]
    fn terminal_states_have_no_edges() {
        for from in ALL_STATES.into_iter().filter(|s| s.is_terminal()) {
            for event in all_events() {
                assert!(from.on(event).is_err(), "{from} has an edge");
            }
        }
    }

    #[test]
    fn exactly_the_diagram_edges_exist() {
        let edges: usize = ALL_STATES
            .iter()
            .map(|s| {
                all_events()
                    .into_iter()
                    .filter(|e| s.on(e.clone()).is_ok())
                    .count()
            })
            .sum();
        // The 19 edges of §5.1 (four of them into Aborting) plus Queued → Superseded.
        assert_eq!(edges, 20);
    }

    #[test]
    fn switching_cannot_abort() {
        // Once the pointers are written there is no way back (ADR-0048).
        for from in [S::Switching, S::Draining, S::CleaningUp] {
            assert!(from.on(Event::Failed("x".into())).is_err());
        }
    }

    #[test]
    fn a_full_happy_path_walks_every_work_state() {
        let mut state = S::Received;
        for event in [
            Event::Accepted,
            Event::Validated,
            Event::Pulled,
            Event::Started,
            Event::Healthy,
            Event::Replaced,
            Event::Switched,
            Event::Drained,
            Event::CleanedUp,
        ] {
            state = state.on(event).unwrap().to;
        }
        assert_eq!(state, S::Live);
    }
}
