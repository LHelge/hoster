//! The event row and the Recent events card, rendered on their own.

use std::time::Duration;

use super::*;
use crate::store::{EventId, EventKind, Timestamp};

const NOW: Duration = Duration::from_secs(1_800_000_000);

fn now() -> SystemTime {
    UNIX_EPOCH + NOW
}

/// Event `id`, `ago` before [`now`].
fn event(id: i64, ago: Duration, severity: Severity) -> Event {
    Event {
        id: EventId::from(id),
        at: Timestamp::from(now() - ago),
        kind: EventKind::DeploySucceeded,
        severity,
        stack: Some("notes".into()),
        subject: Some(format!("/stacks/notes/deployments/{id}")),
        message: format!("notes deployed <#{id}>"),
        meta: Some("a91f3c2 · webhook".into()),
    }
}

fn row(event: &Event) -> String {
    EventRow::new(event, now()).render().unwrap()
}

fn compact(html: &str) -> String {
    html.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn each_severity_has_its_tone_dot_and_says_it_in_words() {
    for (severity, tone) in [
        (Severity::Info, "progress"),
        (Severity::Success, "ok"),
        (Severity::Warning, "warn"),
        (Severity::Error, "fail"),
    ] {
        let html = row(&event(1, Duration::from_secs(300), severity));
        assert!(
            html.contains(&format!(
                r#"<span class="dot event-dot dot-{tone}" aria-hidden="true"></span>"#
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r#"<span class="visually-hidden">{severity}: </span>"#
            )),
            "{html}"
        );
        assert!(!html.contains("style="), "{html}");
    }
}

#[test]
fn a_row_links_its_subject_and_shows_its_time_and_meta() {
    let html = compact(&row(&event(7, Duration::from_secs(300), Severity::Success)));
    assert!(
        html.contains(r#"<time class="event-time" datetime="2027-01-15T07:55:00Z" title="2027-01-15T07:55:00Z">5 min ago</time>"#),
        "{html}"
    );
    // The message is escaped like any text.
    assert!(
        html.contains(r#"<a href="/stacks/notes/deployments/7">notes deployed &#60;#7&#62;</a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="event-meta">a91f3c2 · webhook</span>"#),
        "{html}"
    );
}

#[test]
fn a_row_without_a_page_to_link_is_plain_text() {
    for subject in [None, Some("notes"), Some("//evil.example/x"), Some("")] {
        let mut event = event(1, Duration::ZERO, Severity::Info);
        event.subject = subject.map(str::to_owned);
        event.meta = None;
        let html = row(&event);
        assert!(!html.contains("<a "), "{subject:?}: {html}");
        assert!(
            html.contains("notes deployed &#60;#1&#62;</span>"),
            "{html}"
        );
        assert!(html.contains(">just now</time>"), "{html}");
        assert!(!html.contains("event-meta"), "{html}");
    }
}

#[test]
fn an_empty_card_says_so() {
    let html = RecentEvents::new(Some(Vec::new()), now()).render().unwrap();
    assert!(html.contains("No events yet."), "{html}");
    assert!(!html.contains("events-feed"), "{html}");

    let html = RecentEvents::new(None, now()).render().unwrap();
    assert!(
        html.contains("The events cannot be read right now."),
        "{html}"
    );
}

#[test]
fn the_card_keeps_the_newest_events_first() {
    let events: Vec<Event> = (1..=12)
        .map(|id| event(id, Duration::from_secs(600 - id as u64), Severity::Info))
        .rev()
        .collect();
    let mut card = RecentEvents::new(Some(events), now());
    let html = card.render().unwrap();
    assert_eq!(
        html.matches("<li class=\"event-row\">").count(),
        RecentEvents::LEN
    );
    assert!(html.contains("/deployments/12\""), "{html}");
    assert!(!html.contains("/deployments/2\""), "{html}");

    // A new event goes on top and the oldest falls off; one already shown is not doubled.
    let newer = event(13, Duration::ZERO, Severity::Error);
    assert!(card.add([
        newer.clone(),
        event(12, Duration::from_secs(588), Severity::Info)
    ]));
    let html = card.partial(now());
    assert!(
        html.starts_with(r##"<hx-partial hx-target="#recent-events" hx-swap="outerHTML"><section class="card" id="recent-events""##),
        "{html}"
    );
    assert!(html.ends_with("</section></hx-partial>"), "{html}");
    assert_eq!(
        html.matches("<li class=\"event-row\">").count(),
        RecentEvents::LEN
    );
    assert_eq!(html.matches("/deployments/12\"").count(), 1, "{html}");
    let first = html.find("/deployments/13\"").unwrap();
    assert!(first < html.find("/deployments/12\"").unwrap(), "{html}");
    assert!(!html.contains("/deployments/3\""), "{html}");
}

#[test]
fn an_unreadable_card_cannot_be_added_to() {
    let mut card = RecentEvents::new(None, now());
    assert!(!card.add([event(1, Duration::ZERO, Severity::Info)]));
}
