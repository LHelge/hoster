use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::store::{EventId, EventKind};

/// 2026-11-10 14:03:27 UTC.
const AT_SECS: u64 = 1_794_319_407;

pub(crate) fn at() -> Timestamp {
    Timestamp::from(UNIX_EPOCH + Duration::from_secs(AT_SECS))
}

pub(crate) fn letterhead() -> Letterhead {
    Letterhead::new(
        "srv-01".parse().unwrap(),
        "https://hoster.example.com".parse().unwrap(),
    )
}

/// A failed deploy of `notes`, linking to its deployment.
pub(crate) fn problem() -> Event {
    Event {
        id: EventId::from(42),
        at: at(),
        kind: EventKind::DeployFailed,
        severity: Severity::Error,
        stack: Some("notes".into()),
        subject: Some("/stacks/notes/deployments/41".into()),
        message: "notes deploy failed: web did not become healthy".into(),
        meta: Some("a91f3c2 · webhook · ghcr.io/acme/notes-api".into()),
    }
}

/// `notes`' web service out of crash loop, with no page of its own.
pub(crate) fn recovery() -> Event {
    Event {
        id: EventId::from(43),
        kind: EventKind::CrashLoopLeft,
        severity: Severity::Success,
        subject: None,
        message: "notes: web left crash loop".into(),
        meta: None,
        ..problem()
    }
}

fn mail(notice: Notice<'_>) -> Mail {
    Mail::try_from((&letterhead(), notice)).unwrap()
}

/// Every `http://`, `https://` and protocol-relative `//` reference in `html`, by its position.
fn urls(html: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for pattern in ["http://", "https://", "\"//", "'//", "(//"] {
        for (at, _) in html.match_indices(pattern) {
            let rest = &html[at..];
            let end = rest[1..]
                .find(|c: char| c == '"' || c == '\'' || c == '<' || c.is_whitespace())
                .map_or(rest.len(), |end| end + 1);
            found.push(&rest[..end]);
        }
    }
    found
}

/// The HTML is something a mail client keeps: inline styles only, no SVG, no remote resource,
/// and no URL but the one button's.
fn assert_mail_safe(html: &str, link: &str) {
    for banned in [
        "<style",
        "<svg",
        "<img",
        "<link",
        "<script",
        "@font-face",
        "src=",
        "url(",
    ] {
        assert!(!html.contains(banned), "{banned} in {html}");
    }
    assert_eq!(urls(html), [link], "{html}");
    assert_eq!(html.matches("<a ").count(), 1, "one button: {html}");
}

#[test]
fn a_problem_reads_as_the_event_with_its_page() {
    let event = problem();
    let mail = mail(Notice::from(&event));
    assert_eq!(
        mail.subject,
        "[srv-01] notes deploy failed: web did not become healthy"
    );
    let link = "https://hoster.example.com/stacks/notes/deployments/41";
    assert_mail_safe(&mail.html, link);
    assert!(mail.html.contains(&format!("href=\"{link}\"")));
    assert!(mail.html.contains(">error</td>"), "{}", mail.html);
    assert!(
        mail.html
            .contains("a91f3c2 · webhook · ghcr.io/acme/notes-api")
    );
    assert!(mail.html.contains("2026-11-10 14:03 UTC"));
    // The tone cell is the fail colour, the label on its tint.
    assert!(mail.html.contains("bgcolor=\"#ba0d33\""), "{}", mail.html);
    assert!(mail.html.contains("background-color: #eadae2"));
    assert!(mail.html.contains("hoster · srv-01"));
    assert!(mail.html.contains("(hoster.example.com)"));

    assert_eq!(
        mail.text,
        "[error] notes deploy failed: web did not become healthy\n\
         a91f3c2 · webhook · ghcr.io/acme/notes-api\n\
         2026-11-10 14:03 UTC\n\
         \n\
         Open in hoster: https://hoster.example.com/stacks/notes/deployments/41\n\
         \n\
         --\n\
         Sent by hoster on srv-01 (hoster.example.com).\n\
         Notifications are set in Settings → Notifications."
    );
}

#[test]
fn a_recovery_without_a_page_links_to_its_stack() {
    let event = recovery();
    let mail = mail(Notice::from(&event));
    assert_eq!(mail.subject, "[srv-01] notes: web left crash loop");
    let link = "https://hoster.example.com/stacks/notes";
    assert_mail_safe(&mail.html, link);
    assert!(mail.html.contains(">Open notes</a>"));
    assert!(mail.html.contains(">success</td>"));
    assert!(mail.html.contains("bgcolor=\"#2c6e1d\""));
    assert!(
        mail.text
            .contains("Open notes: https://hoster.example.com/stacks/notes\n")
    );
    assert!(
        mail.text
            .starts_with("[success] notes: web left crash loop\n2026-11-10 14:03 UTC\n")
    );
}

#[test]
fn an_event_of_no_stack_and_no_page_links_to_the_events_page() {
    let event = Event {
        stack: None,
        subject: None,
        kind: EventKind::HosterRolledBack,
        severity: Severity::Warning,
        message: "hoster rolled back to v0.4.0".into(),
        ..problem()
    };
    let mail = mail(Notice::from(&event));
    assert_mail_safe(&mail.html, "https://hoster.example.com/activity");
    assert!(mail.html.contains(">Open the Events page</a>"));
    assert!(mail.html.contains("bgcolor=\"#a53d01\""));
}

#[test]
fn a_reminder_says_it_is_still_failing() {
    let event = problem();
    let mail = mail(Notice::Reminder(&event));
    assert_eq!(
        mail.subject,
        "[srv-01] notes deploy failed: web did not become healthy"
    );
    assert_mail_safe(
        &mail.html,
        "https://hoster.example.com/stacks/notes/deployments/41",
    );
    assert!(mail.html.contains(">still failing</td>"));
    assert!(mail.html.contains("still failing a day after"));
    assert!(mail.text.starts_with("[still failing] notes deploy failed"));
}

#[test]
fn the_summary_counts_the_events_and_links_to_the_events_page() {
    let mail = mail(Notice::Summary { more: 7, at: at() });
    assert_eq!(mail.subject, "[srv-01] 7 more events since the last mail");
    let link = "https://hoster.example.com/activity";
    assert_mail_safe(&mail.html, link);
    assert!(mail.html.contains(">hourly cap</td>"));
    assert!(mail.html.contains("at most 20 notification mails an hour"));
    assert!(
        mail.text
            .contains(&format!("Open the Events page: {link}\n"))
    );

    let one = super::tests::mail(Notice::Summary { more: 1, at: at() });
    assert_eq!(one.subject, "[srv-01] 1 more event since the last mail");
}

#[test]
fn the_test_message_links_to_the_notification_settings() {
    let mail = mail(Notice::Test { at: at() });
    assert_eq!(
        mail.subject,
        "[srv-01] Notifications from hoster reach this address"
    );
    let link = "https://hoster.example.com/settings/notifications";
    assert_mail_safe(&mail.html, link);
    assert!(mail.html.contains(">test</td>"));
    assert!(
        mail.text
            .contains("test message from Settings → Notifications")
    );
    assert!(
        mail.text
            .contains(&format!("Open notification settings: {link}\n"))
    );
}

#[test]
fn hostile_event_text_is_escaped_in_the_html_and_kept_off_the_subject_line() {
    let event = Event {
        message:
            "pull of <script>alert(\"x\")</script> & 'friends' failed\r\nBcc: evil@example.org"
                .into(),
        meta: Some("<img src=https://evil.example/x.png>".into()),
        stack: Some("notes".into()),
        subject: Some("/stacks/notes\" onclick=\"alert(1)".into()),
        ..problem()
    };
    let mail = mail(Notice::from(&event));
    // The subject path stays on the admin host, percent-encoded by the URL parser.
    let link = mail
        .html
        .split("href=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    assert!(
        link.starts_with("https://hoster.example.com/stacks/notes"),
        "{link}"
    );
    assert!(!link.contains('"'), "{link}");
    assert!(!mail.html.contains("\" onclick"), "{}", mail.html);
    assert!(!mail.html.contains("<script>"));
    assert!(!mail.html.contains("<img"));
    assert!(
        mail.html.contains("&#60;script&#62;alert(&#34;x&#34;)"),
        "{}",
        mail.html
    );
    assert!(mail.html.contains("&#38; &#39;friends&#39;"));
    assert!(
        mail.html
            .contains("&#60;img src=https://evil.example/x.png&#62;")
    );
    assert_eq!(mail.html.matches("<a ").count(), 1);

    assert!(!mail.subject.contains(['\r', '\n']), "{:?}", mail.subject);
    assert_eq!(
        mail.subject,
        "[srv-01] pull of <script>alert(\"x\")</script> & 'friends' failed Bcc: evil@example.org"
    );
    // The plain-text part is not HTML, so it keeps the text as it is.
    assert!(
        mail.text
            .contains("<script>alert(\"x\")</script> & 'friends'")
    );
}

#[test]
fn a_subject_that_names_another_origin_falls_back_to_the_stack() {
    for subject in [
        "//evil.example/x",
        "/\\evil.example/x",
        "https://evil.example/x",
        "stacks/notes",
    ] {
        let event = Event {
            subject: Some(subject.into()),
            ..problem()
        };
        let mail = mail(Notice::from(&event));
        assert_mail_safe(&mail.html, "https://hoster.example.com/stacks/notes");
    }
}

#[test]
fn a_long_subject_is_cut() {
    let event = Event {
        message: "x".repeat(500),
        ..problem()
    };
    let mail = mail(Notice::from(&event));
    assert_eq!(mail.subject.chars().count(), MAX_SUBJECT_CHARS);
    assert!(mail.subject.ends_with('…'));
    // The body keeps the whole message.
    assert!(mail.text.contains(&"x".repeat(500)));
}

#[test]
fn the_mail_addresses_a_message() {
    let event = problem();
    let mail = mail(Notice::from(&event));
    let message = mail.clone().into_message(
        "hoster@example.com".parse().unwrap(),
        "ops@example.org".parse().unwrap(),
        "event-42".parse().unwrap(),
    );
    assert_eq!(message.subject, mail.subject);
    assert_eq!(message.text, mail.text);
    assert_eq!(message.html, mail.html);
    assert_eq!(message.to.as_ref(), "ops@example.org");
    assert_eq!(message.idempotency_key.as_ref(), "event-42");
}

#[test]
fn an_admin_origin_with_a_port_keeps_it_in_links_and_footer() {
    let letterhead = Letterhead::new(
        "srv-01".parse().unwrap(),
        "http://127.0.0.1:8080".parse().unwrap(),
    );
    let event = recovery();
    let mail = Mail::try_from((&letterhead, Notice::from(&event))).unwrap();
    assert_mail_safe(&mail.html, "http://127.0.0.1:8080/stacks/notes");
    assert!(mail.text.contains("(127.0.0.1:8080)"));
}

/// The mail's copy of the Latte tokens matches `assets/hoster.css`, and its tints are the
/// status colours at 10 % over `surface`, as `color-mix` makes them in the UI.
#[test]
fn the_mail_tokens_are_the_latte_tokens_of_the_stylesheet() {
    let css = include_str!("../../../assets/hoster.css");
    let latte = &css[css.find("/* Catppuccin Latte. */").unwrap()..];
    let latte = &latte[..latte.find("\n}\n").unwrap()];
    let token = |name: &str| -> String {
        latte
            .lines()
            .find_map(|line| {
                let rest = line.trim().strip_prefix(&format!("--{name}:"))?;
                Some(rest.trim().get(..7)?.to_owned())
            })
            .unwrap_or_else(|| panic!("--{name} in hoster.css"))
    };
    let t = &Tokens::LATTE;
    for (name, copy) in [
        ("bg", t.bg),
        ("surface", t.surface),
        ("border", t.border),
        ("text", t.text),
        ("muted", t.muted),
        ("accent", t.accent),
        ("on-accent", t.on_accent),
        ("ok", t.ok),
        ("progress", t.progress),
        ("warn", t.warn),
        ("fail", t.fail),
    ] {
        assert_eq!(copy, token(name), "--{name}");
    }

    let rgb = |hex: &str| -> [f64; 3] {
        std::array::from_fn(|i| {
            f64::from(u8::from_str_radix(&hex[1 + 2 * i..3 + 2 * i], 16).unwrap())
        })
    };
    let surface = rgb(t.surface);
    for (colour, tint) in [
        (t.ok, t.ok_tint),
        (t.progress, t.progress_tint),
        (t.warn, t.warn_tint),
        (t.fail, t.fail_tint),
    ] {
        let colour = rgb(colour);
        let mixed: [u8; 3] =
            std::array::from_fn(|i| (0.1 * colour[i] + 0.9 * surface[i]).round() as u8);
        let mixed = format!("#{:02x}{:02x}{:02x}", mixed[0], mixed[1], mixed[2]);
        assert_eq!(tint, mixed);
    }
}
