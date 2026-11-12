//! `GET /stacks/{name}/events`: a stack overview's Recent events card as `text/event-stream`
//! (ADR-0055 §5). The card is sent at once, then again whenever an event of the stack is
//! recorded ([`UiEvent::Event`]), with the new events added to the card last sent. A lagged
//! receiver may have missed some, so the card is read from the store again then, and when it
//! could not be read before. With nothing recorded the card is sent again at least every
//! [`StackFeed::REFRESH`], so its "5 min ago" times keep up. The stream ends with an `end`
//! event when the session ends, after replacing the card with a notice.
//!
//! The same stream keeps the services table's CPU and Memory cells live (ADR-0066 §9): after
//! each tick of the resource sampler it sends `<hx-partial>`s that replace the content of the
//! cells of the stack's containers ([`UsageFeed`]), rendered from the sampler's cache alone. A
//! message of only partials swaps nothing into the events card: the SSE extension swaps each
//! message with `swapEmpty:false`, and htmx takes the partials out before deciding whether
//! anything is left for the element itself.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast;

use super::RecentEvents;
use crate::orchestrator::UiEvent;
use crate::store::Event;
use crate::web::auth::{Auth, Session};
use crate::web::stacks::{Stacks, UsageFeed};

/// One stack's Recent events card, read and followed, and its containers' usage.
pub(in crate::web) struct StackFeed {
    stacks: Stacks,
    stack: String,
    usage: Option<UsageFeed>,
}

/// What the feed waited for.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// These events of the stack were recorded, oldest first.
    Events(Vec<Event>),
    /// Time passed with nothing recorded.
    Refresh,
    /// The receiver lagged and may have missed events.
    Reread,
    /// The orchestrator is gone: nothing is recorded any more.
    Closed,
}

impl StackFeed {
    /// The named event after the last update; the page closes the connection on it.
    const END: &str = "end";
    /// How often an idle stream sends a comment, so a closed connection is noticed.
    const KEEP_ALIVE: Duration = Duration::from_secs(15);
    /// How often a stream checks that its session is still signed in.
    const SESSION_CHECK: Duration = if cfg!(test) {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(30)
    };
    /// The longest the card goes without being sent again.
    const REFRESH: Duration = Duration::from_secs(60);
    /// What replaces the card once the session has ended.
    const SIGNED_OUT: &str = r#"<p class="flash flash-error">Signed out: <a href="/">sign in again</a> to see the live events.</p>"#;

    /// The feed of `stack`.
    pub(in crate::web) fn new(stacks: Stacks, stack: &str) -> Self {
        Self {
            stacks,
            stack: stack.to_owned(),
            usage: None,
        }
    }

    /// The feed, also sending `usage` after every sampler tick.
    pub(in crate::web) fn with_usage(self, usage: UsageFeed) -> Self {
        Self {
            usage: Some(usage),
            ..self
        }
    }

    /// The URL of `stack`'s stream.
    pub(in crate::web) fn url(stack: &str) -> String {
        format!("/stacks/{stack}/events")
    }

    /// The card as the store has it now; unreadable when the log cannot be read.
    pub(in crate::web) async fn read(&self) -> RecentEvents {
        let events = self
            .stacks
            .event_page(&RecentEvents::query(Some(&self.stack)))
            .await
            .map(|page| page.events)
            .inspect_err(|err| {
                tracing::warn!(stack = %self.stack, error = %err, "cannot read the stack's events");
            })
            .ok();
        RecentEvents::of_stack(&self.stack, events, SystemTime::now())
    }

    /// The stream as the response, following `events` (subscribed before the stack was looked
    /// up, so nothing in between is missed) until `session` ends.
    pub(in crate::web) fn respond(
        self,
        events: broadcast::Receiver<UiEvent>,
        auth: Arc<Auth>,
        session: Session,
    ) -> Response {
        let signed_out = async move {
            if let Err(err) = auth.session_ended(&session, Self::SESSION_CHECK).await {
                tracing::error!(error = %err, "checking the session of a stack's event stream failed");
            }
        };
        Sse::new(self.stream(events, signed_out))
            .keep_alive(KeepAlive::new().interval(Self::KEEP_ALIVE))
            .into_response()
    }

    /// The card now, then once per change, and the usage partials after every sampler tick,
    /// until the orchestrator is gone or `cut` resolves (which sends [`Self::SIGNED_OUT`]
    /// first); then `end`.
    fn stream(
        mut self,
        events: broadcast::Receiver<UiEvent>,
        cut: impl Future<Output = ()> + Send + 'static,
    ) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
        /// One item of the merged stream: an update, or the end (with a last notice).
        enum Item {
            Update(String),
            Stop(Option<String>),
        }
        let ticks = match self.usage.take() {
            Some(usage) => Self::ticks(usage).map(Item::Update).boxed(),
            None => stream::empty().boxed(),
        };
        let updates = stream::unfold(Some((self, events, None)), |state| async move {
            let (this, mut events, card): (Self, _, Option<RecentEvents>) = state?;
            let mut card = match card {
                None => this.read().await,
                Some(mut card) => {
                    let current = match this.changed(&mut events).await {
                        Change::Closed => return None,
                        Change::Events(new) => card.add(new),
                        Change::Refresh => !card.unreadable(),
                        Change::Reread => false,
                    };
                    // A card that could not be read has nothing to add to.
                    if current { card } else { this.read().await }
                }
            };
            card.now = SystemTime::now();
            let html = card.render().unwrap_or_else(|err| {
                tracing::error!(error = %err, "rendering a stack's events failed");
                String::new()
            });
            Some((html, Some((this, events, Some(card)))))
        });
        let merged = stream::select(
            stream::select(
                updates
                    .map(Item::Update)
                    .chain(stream::once(std::future::ready(Item::Stop(None)))),
                ticks,
            ),
            stream::once(cut).map(|()| Item::Stop(Some(Self::SIGNED_OUT.to_owned()))),
        )
        .boxed();
        // Unfolded rather than taken while, so the stream ends right at its stop instead of
        // waiting on whichever side has not finished.
        stream::unfold(Some(merged), |merged| async move {
            let mut merged = merged?;
            match merged.next().await? {
                Item::Update(html) => Some((html, Some(merged))),
                Item::Stop(Some(notice)) => Some((notice, None)),
                Item::Stop(None) => None,
            }
        })
        .map(|html| SseEvent::default().data(html))
        .chain(stream::once(std::future::ready(
            SseEvent::default().event(Self::END).data(""),
        )))
        .map(Ok)
    }

    /// The usage partials after each sampler tick, from the cache alone; a tick that finds
    /// none of the stack's containers sends nothing. Ends only if the sampler's cache is gone.
    fn ticks(usage: UsageFeed) -> impl Stream<Item = String> + Send + 'static {
        let changes = usage.changes();
        stream::unfold((usage, changes), |(usage, mut changes)| async move {
            changes.changed().await.ok()?;
            let partials = usage.partials();
            Some((partials, (usage, changes)))
        })
        .filter_map(std::future::ready)
    }

    /// Waits for the next event of the stack (or at most [`Self::REFRESH`]), then takes the
    /// ones already queued with it, so a burst sends the card once. Everything else on the
    /// channel is skipped.
    async fn changed(&self, events: &mut broadcast::Receiver<UiEvent>) -> Change {
        let deadline = tokio::time::Instant::now() + Self::REFRESH;
        let mut recorded = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Err(_) => return Change::Refresh,
                Ok(Ok(UiEvent::Event(event))) if self.ours(&event) => {
                    recorded.push(event);
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => return Change::Reread,
                Ok(Err(broadcast::error::RecvError::Closed)) => return Change::Closed,
            }
        }
        loop {
            match events.try_recv() {
                Ok(UiEvent::Event(event)) if self.ours(&event) => recorded.push(event),
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => return Change::Reread,
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => {
                    return Change::Events(recorded);
                }
            }
        }
    }

    /// Whether `event` is about the feed's stack.
    fn ours(&self, event: &Event) -> bool {
        event.stack.as_deref() == Some(self.stack.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EventId, EventKind, Severity, Timestamp};
    use crate::web::stacks::tests::FakeStacks;

    fn event(id: i64, stack: &str) -> Event {
        Event {
            id: EventId::from(id),
            at: Timestamp::from(std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
            kind: EventKind::StackApplied,
            severity: Severity::Info,
            stack: Some(stack.into()),
            subject: None,
            message: format!("applied {stack} #{id}"),
            meta: None,
        }
    }

    fn feed() -> StackFeed {
        let fake: Arc<dyn crate::web::stacks::StackControl> = Arc::new(FakeStacks::default());
        StackFeed::new(Stacks::from(fake), "notes")
    }

    #[tokio::test]
    async fn only_the_stacks_events_are_taken_and_a_burst_is_taken_whole() {
        let (sender, mut events) = broadcast::channel(16);
        sender.send(UiEvent::Event(event(1, "blog"))).unwrap();
        sender.send(UiEvent::Event(event(2, "notes"))).unwrap();
        sender.send(UiEvent::Event(event(3, "blog"))).unwrap();
        sender.send(UiEvent::Event(event(4, "notes"))).unwrap();
        assert_eq!(
            feed().changed(&mut events).await,
            Change::Events(vec![event(2, "notes"), event(4, "notes")])
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_lagged_receiver_reads_again_and_a_closed_one_ends() {
        let (sender, mut events) = broadcast::channel(1);
        sender.send(UiEvent::Event(event(1, "notes"))).unwrap();
        sender.send(UiEvent::Event(event(2, "notes"))).unwrap();
        assert_eq!(feed().changed(&mut events).await, Change::Reread);
        // What the channel kept is still taken.
        assert_eq!(
            feed().changed(&mut events).await,
            Change::Events(vec![event(2, "notes")])
        );
        drop(sender);
        assert_eq!(feed().changed(&mut events).await, Change::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_feed_refreshes() {
        let (sender, mut events) = broadcast::channel(16);
        sender.send(UiEvent::Event(event(1, "blog"))).unwrap();
        assert_eq!(feed().changed(&mut events).await, Change::Refresh);
    }
}
