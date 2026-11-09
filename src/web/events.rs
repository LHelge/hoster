//! The event log's views (ADR-0055 §5). [`EventRow`] is one event as a feed row, the
//! `event_row.html` partial every feed shares: how long ago it happened (the time itself in the
//! tooltip), a tone dot for its [`Severity`], its message, linked to what it is about when it
//! names a page, and its short mono meta line. [`RecentEvents`] is a card of the newest of them:
//! every stack's on the dashboard, one stack's on its overview ([`RecentEvents::of_stack`]).
//! Each card links to the Events page ([`EventsPage`], `page.rs`), which pages through every event
//! with filters for kind, severity and stack.
//!
//! The dashboard's live stream keeps the card it last sent: an event recorded since is added to
//! it in memory and only the card is sent again, as an `<hx-partial>` that replaces it
//! ([`RecentEvents::partial`]), not the whole overview. Anything else re-reads the card from the
//! store with the overview. A stack's card has a stream of its own ([`StackFeed`], `live.rs`),
//! which sends the card again whenever an event of that stack is recorded.

use std::time::{SystemTime, UNIX_EPOCH};

use askama::Template;

use super::stacks::{Ago, When};
use super::ui::Tone;
use crate::store::{Event, EventQuery, Severity};

mod live;
mod page;

pub(super) use self::live::StackFeed;
pub use self::page::EventsPage;

/// The tone dot of a severity: an informational event in the blue the design gives it, the
/// others as their names say.
impl From<Severity> for Tone {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Info => Tone::Progress,
            Severity::Success => Tone::Ok,
            Severity::Warning => Tone::Warn,
            Severity::Error => Tone::Fail,
        }
    }
}

/// One event in a feed.
#[derive(Debug, Template)]
#[template(path = "event_row.html")]
pub(super) struct EventRow {
    /// `5 min ago`.
    ago: String,
    /// The time itself, for `datetime` and the tooltip.
    at: String,
    severity: Severity,
    message: String,
    /// The page the event is about: a deployment, a stack, the certificates, the system.
    href: Option<String>,
    meta: Option<String>,
}

impl EventRow {
    /// `event` as of `now`.
    pub(super) fn new(event: &Event, now: SystemTime) -> Self {
        Self {
            ago: Ago::since(event.at, now).to_string(),
            at: When(event.at).to_string(),
            severity: event.severity,
            message: event.message.clone(),
            href: event
                .subject
                .as_deref()
                .and_then(Self::link)
                .map(str::to_owned),
            meta: event
                .meta
                .as_deref()
                .filter(|meta| !meta.is_empty())
                .map(str::to_owned),
        }
    }

    /// The subject as a link: only a path on this site (the producers record the page they
    /// are about), never another origin.
    fn link(subject: &str) -> Option<&str> {
        (subject.starts_with('/') && !subject.starts_with("//")).then_some(subject)
    }

    fn tone(&self) -> Tone {
        Tone::from(self.severity)
    }
}

/// A Recent events card: the newest [`RecentEvents::LEN`] events, newest first, of every stack
/// (the dashboard's) or of one.
#[derive(Debug, Clone, Template)]
#[template(path = "recent_events.html")]
pub(super) struct RecentEvents {
    /// `None` when the event log cannot be read.
    events: Option<Vec<Event>>,
    now: SystemTime,
    /// The stack whose events the card shows; every event when `None`.
    stack: Option<String>,
}

impl RecentEvents {
    /// How many events the card shows.
    pub(super) const LEN: usize = 10;
    /// The card's element id, which [`RecentEvents::partial`] targets.
    const ID: &str = "recent-events";

    /// The card of `events` (newest first, as the store pages them) as of `now`; `None` when
    /// they cannot be read.
    pub(super) fn new(events: Option<Vec<Event>>, now: SystemTime) -> Self {
        Self::of(None, events, now)
    }

    /// The card of `stack`'s `events`, as [`RecentEvents::new`].
    pub(super) fn of_stack(stack: &str, events: Option<Vec<Event>>, now: SystemTime) -> Self {
        Self::of(Some(stack.to_owned()), events, now)
    }

    fn of(stack: Option<String>, events: Option<Vec<Event>>, now: SystemTime) -> Self {
        let mut card = Self {
            events: events.is_some().then(Vec::new),
            now,
            stack,
        };
        card.add(events.unwrap_or_default());
        card
    }

    /// The query for the events of a card of `stack` (of every stack when `None`).
    pub(super) fn query(stack: Option<&str>) -> EventQuery {
        EventQuery {
            stack: stack.map(str::to_owned),
            limit: Self::LEN as u32,
            ..EventQuery::default()
        }
    }

    /// Adds `events` (in any order; one already shown, or of another stack than the card's, is
    /// skipped), keeping the newest [`Self::LEN`]. `false` when the card has no events to add
    /// to, because they could not be read: then only a fresh read shows them.
    pub(super) fn add(&mut self, events: impl IntoIterator<Item = Event>) -> bool {
        let Some(shown) = &mut self.events else {
            return false;
        };
        for event in events {
            let ours = self.stack.is_none() || event.stack == self.stack;
            if ours && !shown.iter().any(|e| e.id == event.id) {
                shown.push(event);
            }
        }
        shown.sort_by_key(|e| std::cmp::Reverse((e.at, e.id)));
        shown.truncate(Self::LEN);
        true
    }

    /// The card as of `now`, wrapped to replace the one on the page and nothing else: the SSE
    /// message that appends events live.
    pub(super) fn partial(&mut self, now: SystemTime) -> String {
        self.now = now;
        let card = self.render().unwrap_or_else(|err| {
            tracing::error!(error = %err, "rendering the recent events card failed");
            String::new()
        });
        format!(
            r##"<hx-partial hx-target="#{}" hx-swap="outerHTML">{card}</hx-partial>"##,
            Self::ID
        )
    }

    fn rows(&self) -> Vec<EventRow> {
        self.events
            .iter()
            .flatten()
            .map(|event| EventRow::new(event, self.now))
            .collect()
    }

    fn unreadable(&self) -> bool {
        self.events.is_none()
    }

    /// The Events page, filtered to the card's stack if it has one.
    fn all_url(&self) -> String {
        EventsPage::url(self.stack.as_deref())
    }

    fn id(&self) -> &'static str {
        Self::ID
    }
}

impl Default for RecentEvents {
    fn default() -> Self {
        Self::new(Some(Vec::new()), UNIX_EPOCH)
    }
}

#[cfg(test)]
mod tests;
