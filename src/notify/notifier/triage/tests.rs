use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::store::EventId;

const MINUTE: Duration = Duration::from_secs(60);

/// 2026-11-10 12:00:00 UTC, the fake clock's start.
fn t0() -> Timestamp {
    Timestamp::from(UNIX_EPOCH + Duration::from_secs(1_794_312_000))
}

fn event(id: i64, kind: EventKind, severity: Severity, message: &str) -> Event {
    Event {
        id: EventId::from(id),
        at: t0(),
        kind,
        severity,
        stack: Some("notes".into()),
        subject: Some("/stacks/notes".into()),
        message: message.into(),
        meta: None,
    }
}

fn crash_loop(id: i64, service: &str) -> Event {
    event(
        id,
        EventKind::CrashLoopEntered,
        Severity::Error,
        &format!("notes/{service} is crash looping"),
    )
}

fn crash_loop_left(id: i64, service: &str) -> Event {
    event(
        id,
        EventKind::CrashLoopLeft,
        Severity::Success,
        &format!("notes/{service} stopped crash looping"),
    )
}

fn cert(id: i64, kind: EventKind, host: &str) -> Event {
    let (severity, message) = match kind {
        EventKind::CertificateFailed => (
            Severity::Error,
            format!("Certificate for {host} failed: rate limited"),
        ),
        EventKind::CertificateRenewed => (
            Severity::Success,
            format!("Renewed the certificate for {host}"),
        ),
        _ => (
            Severity::Success,
            format!("Issued a certificate for {host}"),
        ),
    };
    Event {
        subject: Some("/certificates".into()),
        ..event(id, kind, severity, &message)
    }
}

fn deploy(id: i64, kind: EventKind, severity: Severity) -> Event {
    Event {
        subject: Some(format!("/stacks/notes/deployments/{id}")),
        ..event(id, kind, severity, "Deployment of notes")
    }
}

/// Judges `event` at `now` and, as the notifier does once the mail went out, settles it.
fn run(triage: &mut Triage, event: &Event, now: Timestamp) -> Verdict {
    let verdict = triage.judge(event, now);
    triage.settle(&verdict.step(event, verdict.mail.is_some(), now));
    verdict
}

fn mailed(verdict: &Verdict) -> Option<Mailing> {
    verdict.mail
}

#[test]
fn a_problem_mails_once_and_its_recovery_closes_it() {
    let mut triage = Triage::default();
    let first = run(&mut triage, &crash_loop(1, "web"), t0());
    assert_eq!(mailed(&first), Some(Mailing::Event));
    assert_eq!(
        first.opens,
        Some(ProblemKey {
            kind: EventKind::CrashLoopEntered,
            stack: Some("notes".into()),
            subject: Some("notes/web".into()),
        })
    );
    let repeat = run(&mut triage, &crash_loop(2, "web"), t0() + MINUTE);
    assert_eq!(repeat, Verdict::silent());
    let recovery = run(&mut triage, &crash_loop_left(3, "web"), t0() + 2 * MINUTE);
    assert_eq!(mailed(&recovery), Some(Mailing::Event));
    assert_eq!(recovery.closes, [first.opens.unwrap()]);
    // Closed: the next occurrence is a new problem.
    let again = run(&mut triage, &crash_loop(4, "web"), t0() + 3 * MINUTE);
    assert_eq!(mailed(&again), Some(Mailing::Event));
}

#[test]
fn services_of_one_stack_are_separate_problems() {
    let mut triage = Triage::default();
    assert!(run(&mut triage, &crash_loop(1, "web"), t0()).mail.is_some());
    assert!(
        run(&mut triage, &crash_loop(2, "worker"), t0())
            .mail
            .is_some()
    );
    let left = run(&mut triage, &crash_loop_left(3, "worker"), t0());
    assert_eq!(left.closes.len(), 1);
    assert_eq!(left.closes[0].subject.as_deref(), Some("notes/worker"));
    // web is still open.
    assert_eq!(
        run(&mut triage, &crash_loop(4, "web"), t0()),
        Verdict::silent()
    );
}

#[test]
fn a_recovery_of_a_problem_that_was_not_mailed_is_silent() {
    let triage = Triage::default();
    assert_eq!(
        triage.judge(&crash_loop_left(1, "web"), t0()),
        Verdict::silent()
    );
    let healthy = event(
        2,
        EventKind::HealthChanged,
        Severity::Success,
        "notes/web is healthy again",
    );
    assert_eq!(triage.judge(&healthy, t0()), Verdict::silent());
}

#[test]
fn a_repeat_after_the_reminder_interval_mails_a_reminder() {
    let mut triage = Triage::default();
    let fail = |id| cert(id, EventKind::CertificateFailed, "app.example.com");
    run(&mut triage, &fail(1), t0());
    let late = t0() + Triage::REMINDER - MINUTE;
    let repeat = Event {
        at: late,
        ..fail(2)
    };
    assert_eq!(run(&mut triage, &repeat, late), Verdict::silent());
    let due = t0() + Triage::REMINDER;
    let reminder = run(&mut triage, &Event { at: due, ..fail(3) }, due);
    assert_eq!(mailed(&reminder), Some(Mailing::Reminder));
    // The reminder restarts the interval.
    let next = due + Triage::REMINDER - MINUTE;
    assert_eq!(
        run(
            &mut triage,
            &Event {
                at: next,
                ..fail(4)
            },
            next
        ),
        Verdict::silent()
    );
}

#[test]
fn a_certificate_recovery_is_mailed_only_for_a_host_whose_failure_was() {
    let mut triage = Triage::default();
    run(
        &mut triage,
        &cert(1, EventKind::CertificateFailed, "app.example.com"),
        t0(),
    );
    let other = cert(2, EventKind::CertificateIssued, "www.example.com");
    assert_eq!(triage.judge(&other, t0()), Verdict::silent());
    let renewed = run(
        &mut triage,
        &cert(3, EventKind::CertificateRenewed, "app.example.com"),
        t0(),
    );
    assert_eq!(mailed(&renewed), Some(Mailing::Event));
    assert_eq!(renewed.closes[0].kind, EventKind::CertificateFailed);
    // With "also successes" every certificate mails.
    let triage = Triage::new([], true);
    assert_eq!(mailed(&triage.judge(&other, t0())), Some(Mailing::Event));
}

#[test]
fn a_dns_mismatch_is_held_back_and_ended_by_the_certificate() {
    let mut triage = Triage::default();
    let mismatch = |id| Event {
        kind: EventKind::DnsMismatch,
        severity: Severity::Warning,
        message: "DNS for app.example.com does not point at this server".into(),
        ..cert(id, EventKind::DnsMismatch, "app.example.com")
    };
    assert!(run(&mut triage, &mismatch(1), t0()).mail.is_some());
    assert_eq!(run(&mut triage, &mismatch(2), t0()), Verdict::silent());
    let issued = run(
        &mut triage,
        &cert(3, EventKind::CertificateIssued, "app.example.com"),
        t0(),
    );
    assert_eq!(mailed(&issued), Some(Mailing::Event));
    assert_eq!(issued.closes[0].kind, EventKind::DnsMismatch);
}

#[test]
fn every_deploy_problem_mails_and_the_next_success_is_its_recovery() {
    let mut triage = Triage::default();
    let failed = run(
        &mut triage,
        &deploy(1, EventKind::DeployFailed, Severity::Error),
        t0(),
    );
    assert_eq!(mailed(&failed), Some(Mailing::Event));
    let again = run(
        &mut triage,
        &deploy(2, EventKind::DeployRolledBack, Severity::Success),
        t0(),
    );
    assert_eq!(mailed(&again), Some(Mailing::Event), "not held back");
    let succeeded = run(
        &mut triage,
        &deploy(3, EventKind::DeploySucceeded, Severity::Success),
        t0(),
    );
    assert_eq!(mailed(&succeeded), Some(Mailing::Event));
    assert_eq!(succeeded.closes.len(), 1);
    // The next success has no problem to end.
    let next = deploy(4, EventKind::DeploySucceeded, Severity::Success);
    assert_eq!(triage.judge(&next, t0()), Verdict::silent());
    assert_eq!(
        mailed(&Triage::new([], true).judge(&next, t0())),
        Some(Mailing::Event)
    );
}

#[test]
fn a_cancelled_deploy_is_not_mailed() {
    let triage = Triage::default();
    let cancelled = deploy(1, EventKind::DeployFailed, Severity::Warning);
    assert_eq!(triage.judge(&cancelled, t0()), Verdict::silent());
}

#[test]
fn the_fixed_sets_are_followed() {
    let triage = Triage::new([], true);
    let always = [EventKind::HosterUpdated, EventKind::HosterRolledBack];
    for kind in always {
        let verdict = triage.judge(&event(1, kind, Severity::Info, "hoster"), t0());
        assert_eq!(mailed(&verdict), Some(Mailing::Event), "{kind}");
        assert_eq!(verdict.opens, None);
    }
    let never = [
        EventKind::DeployStarted,
        EventKind::ContainerRestarted,
        EventKind::StackApplied,
        EventKind::SecretSet,
        EventKind::SecretDeleted,
        EventKind::NotificationFailed,
        EventKind::StackRemoved,
        EventKind::HosterStarted,
    ];
    for kind in never {
        let verdict = triage.judge(&event(1, kind, Severity::Warning, "x"), t0());
        assert_eq!(verdict.mail, None, "{kind}");
    }
    let unhealthy = event(
        1,
        EventKind::HealthChanged,
        Severity::Warning,
        "notes/web is unhealthy",
    );
    assert_eq!(
        mailed(&triage.judge(&unhealthy, t0())),
        Some(Mailing::Event)
    );
}

#[test]
fn stale_events_are_not_mailed_but_still_close_problems() {
    let mut triage = Triage::default();
    run(&mut triage, &crash_loop(1, "web"), t0());
    let later = t0() + Triage::STALE + MINUTE;
    let stale_left = run(&mut triage, &crash_loop_left(2, "web"), later);
    assert_eq!(stale_left.mail, None);
    assert_eq!(stale_left.closes.len(), 1);
    let stale_problem = triage.judge(&crash_loop(3, "web"), later);
    assert_eq!(stale_problem, Verdict::silent(), "not mailed, not recorded");
    // Exactly one hour old is still mailed.
    let edge = triage.judge(&crash_loop(4, "web"), t0() + Triage::STALE);
    assert_eq!(mailed(&edge), Some(Mailing::Event));
}

#[test]
fn removing_a_stack_forgets_its_problems_silently() {
    let mut triage = Triage::default();
    run(&mut triage, &crash_loop(1, "web"), t0());
    run(
        &mut triage,
        &deploy(2, EventKind::DeployFailed, Severity::Error),
        t0(),
    );
    run(
        &mut triage,
        &cert(3, EventKind::CertificateFailed, "app.example.com"),
        t0(),
    );
    let removed = run(
        &mut triage,
        &event(4, EventKind::StackRemoved, Severity::Info, "Removed notes"),
        t0(),
    );
    assert_eq!(removed.mail, None);
    assert_eq!(removed.closes.len(), 3, "the host's certificate with them");
    assert!(run(&mut triage, &crash_loop(5, "web"), t0()).mail.is_some());
    let fail = cert(6, EventKind::CertificateFailed, "app.example.com");
    assert!(run(&mut triage, &fail, t0()).mail.is_some());
}

#[test]
fn a_successful_deploy_forgets_the_old_generations_problems() {
    let mut triage = Triage::default();
    run(&mut triage, &crash_loop(1, "web"), t0());
    let unhealthy = event(
        2,
        EventKind::HealthChanged,
        Severity::Warning,
        "notes/worker is unhealthy",
    );
    run(&mut triage, &unhealthy, t0());
    run(
        &mut triage,
        &cert(3, EventKind::CertificateFailed, "app.example.com"),
        t0(),
    );
    let other = Event {
        stack: Some("blog".into()),
        message: "blog/web is crash looping".into(),
        ..crash_loop(4, "web")
    };
    run(&mut triage, &other, t0());
    let succeeded = run(
        &mut triage,
        &deploy(5, EventKind::DeploySucceeded, Severity::Success),
        t0(),
    );
    assert_eq!(succeeded.mail, None, "forgetting them is no recovery mail");
    let forgotten: Vec<_> = succeeded.closes.iter().map(|key| key.kind).collect();
    assert_eq!(
        forgotten,
        [EventKind::CrashLoopEntered, EventKind::HealthChanged]
    );
    // The new generation's crash loop is a new problem; blog's is still open.
    assert!(run(&mut triage, &crash_loop(6, "web"), t0()).mail.is_some());
    let again = Event {
        id: EventId::from(7),
        ..other
    };
    assert_eq!(run(&mut triage, &again, t0()), Verdict::silent());
}

#[test]
fn a_rollback_forgets_the_retired_generations_problems() {
    let mut triage = Triage::default();
    run(&mut triage, &crash_loop(1, "web"), t0());
    run(
        &mut triage,
        &cert(2, EventKind::CertificateFailed, "app.example.com"),
        t0(),
    );
    let rolled_back = run(
        &mut triage,
        &deploy(3, EventKind::DeployRolledBack, Severity::Success),
        t0(),
    );
    assert_eq!(mailed(&rolled_back), Some(Mailing::Event));
    assert_eq!(
        rolled_back.opens.as_ref().map(|key| key.kind),
        Some(EventKind::DeployFailed)
    );
    let forgotten: Vec<_> = rolled_back.closes.iter().map(|key| key.kind).collect();
    assert_eq!(forgotten, [EventKind::CrashLoopEntered]);
    // No "still failing" reminder about the retired generation's crash loop.
    let due: Vec<_> = triage
        .reminders(t0() + Triage::REMINDER)
        .into_iter()
        .map(|problem| problem.key.kind)
        .collect();
    assert_eq!(due, [EventKind::CertificateFailed]);
    // The rolled-back generation's own crash loop is a new problem.
    assert!(run(&mut triage, &crash_loop(4, "web"), t0()).mail.is_some());
}

#[test]
fn a_hosts_recovery_ends_its_problem_under_another_stack() {
    let mut triage = Triage::default();
    run(
        &mut triage,
        &cert(1, EventKind::CertificateFailed, "app.example.com"),
        t0(),
    );
    let issued = Event {
        stack: Some("blog".into()),
        ..cert(2, EventKind::CertificateIssued, "app.example.com")
    };
    let verdict = run(&mut triage, &issued, t0());
    assert_eq!(mailed(&verdict), Some(Mailing::Event));
    assert_eq!(verdict.closes[0].stack.as_deref(), Some("notes"));
}

#[test]
fn an_open_problem_is_reminded_of_a_day_after_its_last_mail() {
    let mut triage = Triage::default();
    run(&mut triage, &crash_loop(1, "web"), t0());
    run(
        &mut triage,
        &deploy(2, EventKind::DeployFailed, Severity::Error),
        t0(),
    );
    assert!(
        triage
            .reminders(t0() + Triage::REMINDER - MINUTE)
            .is_empty()
    );
    let due = t0() + Triage::REMINDER;
    let reminders = triage.reminders(due);
    assert_eq!(reminders.len(), 1, "a deploy problem is not reminded of");
    assert_eq!(reminders[0].key.kind, EventKind::CrashLoopEntered);
    assert_eq!(reminders[0].event, EventId::from(1));
    let step = Triage::reminded(&reminders[0], due);
    assert_eq!(step.notified[0].at, due);
    triage.settle(&step);
    assert!(triage.reminders(due + Triage::REMINDER - MINUTE).is_empty());
    assert_eq!(triage.reminders(due + Triage::REMINDER).len(), 1);
}

#[test]
fn a_restart_forgets_crash_loops_and_health_but_not_certificates() {
    let mut triage = Triage::default();
    run(&mut triage, &crash_loop(1, "web"), t0());
    run(
        &mut triage,
        &cert(2, EventKind::CertificateFailed, "app.example.com"),
        t0(),
    );
    let started = Event {
        stack: None,
        ..event(
            3,
            EventKind::HosterStarted,
            Severity::Info,
            "hoster started",
        )
    };
    let verdict = run(&mut triage, &started, t0());
    assert_eq!(verdict.closes.len(), 1);
    assert_eq!(verdict.closes[0].kind, EventKind::CrashLoopEntered);
}

#[test]
fn an_undelivered_problem_is_not_recorded() {
    let mut triage = Triage::default();
    let problem = crash_loop(1, "web");
    let verdict = triage.judge(&problem, t0());
    let step = verdict.step(&problem, false, t0());
    assert!(step.notified.is_empty());
    triage.settle(&step);
    assert!(triage.judge(&crash_loop(2, "web"), t0()).mail.is_some());
}

#[test]
fn the_cap_holds_back_mails_beyond_twenty_an_hour_and_then_sends_one_summary() {
    let mut cap = HourlyCap::default();
    for i in 0..HourlyCap::LIMIT {
        assert!(cap.admit(t0() + Duration::from_secs(i as u64)), "mail {i}");
    }
    assert_eq!(cap.summary(t0() + MINUTE), None, "nothing held back yet");
    for _ in 0..30 {
        assert!(!cap.admit(t0() + MINUTE));
    }
    assert_eq!(cap.summary(t0() + 30 * MINUTE), None, "no room yet");
    let hour_later = t0() + HOUR;
    assert_eq!(cap.summary(hour_later), Some(30));
    assert_eq!(cap.summary(hour_later), None, "one summary");
    // The summary took a slot: the first mail's has passed, so does the second's.
    assert!(cap.admit(hour_later + Duration::from_secs(1)));
}

#[test]
fn a_held_back_count_waits_for_its_summary_before_more_mail() {
    let mut cap = HourlyCap::default();
    for _ in 0..HourlyCap::LIMIT {
        cap.admit(t0());
    }
    assert!(!cap.admit(t0()));
    // Room again, but the summary has not gone out: the mail is counted, not sent.
    assert!(!cap.admit(t0() + HOUR));
    assert_eq!(cap.summary(t0() + HOUR), Some(2));
}

#[test]
fn retries_back_off_honour_retry_after_and_stop_after_the_window() {
    let retry = Retry::default();
    let s = Duration::from_secs;
    assert_eq!(retry.wait(1, None, s(0)), Some(s(5)));
    assert_eq!(retry.wait(2, None, s(5)), Some(s(10)));
    assert_eq!(retry.wait(5, None, s(100)), Some(s(60)), "at most a minute");
    assert_eq!(retry.wait(1, Some(s(30)), s(0)), Some(s(30)));
    assert_eq!(
        retry.wait(6, None, s(240)),
        Some(s(60)),
        "ends at the window"
    );
    assert_eq!(retry.wait(7, None, s(241)), None, "past the window");
    assert_eq!(
        retry.wait(1, Some(s(600)), s(0)),
        None,
        "asked past the window"
    );
}

#[test]
fn a_failing_provider_writes_one_failure_event_an_hour() {
    let mut failures = FailureEvents::default();
    assert!(failures.due(t0()));
    assert!(!failures.due(t0() + MINUTE));
    assert!(!failures.due(t0() + HOUR - MINUTE));
    assert!(failures.due(t0() + HOUR));
}
