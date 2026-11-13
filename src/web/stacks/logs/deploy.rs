//! The logs tab's `deploy` source: the stack's deployment events over the range, in time order
//! across its deployments, read from `hoster.db` ([`DeploymentLogQuery`]), not the log store,
//! so history older than the log store's retention is listed too. A row is the time, the
//! deployment (linking its page), the transition or the step a note was written in, and the
//! message or failure reason, worded as the deployment's page words them. Grep matches the
//! message; the range, paging and following work as for the containers.
//!
//! Following waits for the orchestrator to report a deployment of the stack moved on
//! `ui_events` (as a deployment's page does, `deployments.rs`), then reads the events after the
//! newest it sent.

use std::time::SystemTime;

use tokio::sync::broadcast;

use crate::health::ProbeOutput;
use crate::orchestrator::{NoteKind, UiEvent};
use crate::store::{
    DeploymentLogEntry, DeploymentLogQuery, DeploymentState, Level, PageAt, RecordKey, Timestamp,
};
use crate::web::logs::{
    EmptyNote, LogMount, LogParams, LogSource, LogsView, SourceEntry, SourcePlan, SourceRead,
    SourceTail,
};
use crate::web::stacks::Stacks;
use crate::web::stacks::deployments::DeploymentView;

/// The deploy source of one stack.
pub(super) struct DeployLog {
    stacks: Stacks,
    stack: String,
}

impl DeployLog {
    pub(super) fn new(stacks: Stacks, stack: String) -> Self {
        Self { stacks, stack }
    }

    /// The view of `mount` with the options of `params`.
    pub(super) async fn view(&self, mount: LogMount, params: &LogParams) -> LogsView {
        let now = SystemTime::now();
        let plan = SourcePlan::new(params, now);
        let read = match plan.reads() {
            true => self.read(self.query(&plan)).await,
            false => Ok(Default::default()),
        };
        let empty = EmptyNote {
            text: "No deployments in this range.".to_owned(),
            link: "deployments".to_owned(),
            href: format!("{}/deployments", super::StackView::url(&self.stack)),
        };
        LogsView::listing(mount, params, now, LogSource::Deploy, plan, read, empty)
    }

    fn query(&self, plan: &SourcePlan) -> DeploymentLogQuery {
        DeploymentLogQuery {
            stack: self.stack.clone(),
            range: plan.range.clone(),
            text: plan.text.clone(),
            page: plan.page,
            limit: plan.limit,
        }
    }

    async fn read(
        &self,
        query: DeploymentLogQuery,
    ) -> Result<crate::store::Page<SourceEntry>, String> {
        match self.stacks.0.deployment_log(&query).await {
            Ok(page) => Ok(crate::store::Page {
                entries: page.entries.iter().map(|e| self.entry(e)).collect(),
                older: page.older,
                newer: page.newer,
                scan_stopped: page.scan_stopped,
            }),
            Err(err) => {
                tracing::error!(stack = %self.stack, error = %err, "reading the deploy log failed");
                Err(format!("Cannot read the deployment history: {err}"))
            }
        }
    }

    /// The follow stream's reader for the options of `params`, waiting on `events`
    /// (subscribed before the page's stream read anything).
    pub(super) fn tail(
        self,
        params: &LogParams,
        events: broadcast::Receiver<UiEvent>,
    ) -> DeployTail {
        let plan = SourcePlan::new(params, SystemTime::now());
        let mut query = self.query(&plan);
        // Every event from the start of the range on, however far its time is ahead.
        query.range.end = Timestamp::from_millis(i64::MAX);
        DeployTail {
            log: self,
            query,
            events,
        }
    }

    /// `entry` as its row shows it.
    fn entry(&self, entry: &DeploymentLogEntry) -> SourceEntry {
        let event = &entry.event;
        let message = event.message.as_deref().unwrap_or_default();
        let text = ProbeOutput::from(message).text;
        let note = event.from == Some(event.to);
        let step = if note {
            match NoteKind::try_from(text) {
                Ok(kind) => format!("{} · {kind}", event.to),
                Err(_) => event.to.to_string(),
            }
        } else {
            match event.from {
                Some(from) => format!("{from} → {}", event.to),
                None => format!("→ {}", event.to),
            }
        };
        let level = match event.to {
            _ if note => None,
            DeploymentState::Failed | DeploymentState::Rejected => Some(Level::Error),
            DeploymentState::Aborting | DeploymentState::LiveWithWarnings => Some(Level::Warn),
            _ => None,
        };
        let id = i64::from(entry.deployment);
        SourceEntry {
            key: entry.key(),
            label: format!("#{}", entry.deployment),
            href: Some(DeploymentView::url(&self.stack, entry.deployment)),
            tone: usize::try_from(id.rem_euclid(4)).unwrap_or_default(),
            level,
            step: Some(step),
            message: text.to_owned(),
        }
    }
}

/// What the deploy source's follow stream reads.
pub(super) struct DeployTail {
    log: DeployLog,
    query: DeploymentLogQuery,
    events: broadcast::Receiver<UiEvent>,
}

#[async_trait::async_trait]
impl SourceTail for DeployTail {
    async fn changed(&mut self) -> bool {
        loop {
            match self.events.recv().await {
                Ok(UiEvent::Deployment { stack, .. }) if stack == self.log.stack => return true,
                Ok(_) => {}
                // It may have missed a change.
                Err(broadcast::error::RecvError::Lagged(_)) => return true,
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    }

    async fn after(&mut self, cursor: RecordKey, limit: u32) -> Result<SourceRead, String> {
        let mut query = self.query.clone();
        query.page = PageAt::After(cursor);
        query.limit = limit;
        let page = self.log.read(query).await?;
        let last = page.entries.last().map(|e| e.key);
        Ok(SourceRead {
            more: page.scan_stopped.or(last.filter(|_| page.newer)),
            entries: page.entries,
        })
    }
}
