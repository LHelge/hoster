---
id: rja
title: "ADR: email notifications through a provider trait (Resend first)"
status: done
priority: P3
created: "2026-11-09T23:22:29.903098581Z"
updated: "2026-11-09T23:31:14.963882040Z"
tags:
  - decision
  - docs
  - adr
parent: c5s
attempts: 1
---

## Summary
Write the ADR (next free number) for notifications, amending ADR-0045 ("no outbound alerting") and ADR-0024 (some settings now live in SQLite and are edited in the UI). It resolves OQ-37. Send it to review for owner sign-off.

Owner decisions (2026-11-10, OQ-37): email; Resend as the first provider behind a small provider trait; the next providers are other HTTP APIs (Mailjet, Twilio and similar) before SMTP; configuration in the database, with `hoster.toml` kept for what is essential to bring the server up; one recipient address; the rest as recommended.

## Outcome
Written as `docs/adr/0058-email-notifications.md`, accepted by the owner on 2026-11-10. Changes from the proposals below, after the owner's follow-up on 2026-11-10: mail only, so no short SMS form in the message; an instance name as a run-time setting (ADR-0058 §2, task `mxq`) replaces the admin host in subjects.

## Points the ADR must settle (proposed answers)
1. **Provider trait.** One trait with a single "deliver this message" method and a typed error that separates retryable (network, 429, 5xx) from permanent (401/403, 422 unverified sender) failures. The message carries a subject, a short plain-text form, a long plain-text form and an HTML form, so a later SMS-style provider (Twilio) can take the short form and a mail provider the rest. The provider is chosen by a typed enum stored with the settings, not by a string lookup.
2. **Resend.** `POST https://api.resend.com/emails` with a bearer key and `from`, `to`, `subject`, `html`, `text`, through the existing `reqwest` client. The from address must be on a domain verified at Resend. Verify the request and error shapes against Resend's current API docs when writing; don't rely on memory.
3. **Configuration in SQLite.** One settings row: provider, from address, recipient, enabled, "also successes". The API key goes in the secret store (ADR-0029), write-only in the UI like registry credentials (ADR-0025). State the rule this sets for ADR-0024: `hoster.toml` holds what the server needs to start; operational settings edited at run time live in SQLite. Notifications are off until configured.
4. **How the notifier sees events.** A background task that reads the `events` table past a persisted cursor, woken by the `UiEvent` broadcast and a slow timer. It never depends on the broadcast keeping up (ADR-0055 §3). The cursor starts at the newest event when notifications are first enabled. Events older than a cut-off (propose 1 h) are skipped, so a long outage doesn't send stale alerts.
5. **Which events.**
   - Problems: `DeployFailed`, `DeployRolledBack`, `CrashLoopEntered`, `CertificateFailed`, `DnsMismatch`, `HosterRolledBack`.
   - Recoveries: `CrashLoopLeft`; `CertificateIssued`/`CertificateRenewed` only for a host whose failure was notified.
   - Informational, always sent: `HosterUpdated`.
   - With "also successes": `DeploySucceeded`, `CertificateIssued`, `CertificateRenewed`.
   - Decide whether `HealthChanged` to unhealthy (and back) of a live service belongs to the problem set. Propose yes.
   - Never: `ContainerRestarted`, `DeployStarted`, stack and secret operations, `HosterStarted`.
6. **De-duplication and rate limit.** One mail per state change, keyed by (kind, stack, subject). A repeated failure for the same key is silent until it recovers or a reminder interval passes (propose 24 h); certificate retries (1 h to 12 h back-off) are the case this exists for. A global cap (propose 20 mails per hour); beyond it, one "N more events" mail linking to the Events page.
7. **Failed deliveries.** Retry retryable errors with back-off for a few minutes, then drop. Record a new event kind for a failed delivery, which itself never notifies. Keep the last delivery's time and result for the Settings page.
8. **Mail content.** Subject `[<admin host>] <readable line>`; the body is the event row (tone, message, mono meta line) plus a link to the stack or the Events page on the admin host. HTML with inline styles and table layout, tokens copied from `assets/hoster.css`, system font fall-backs, no SVG, no remote images; a plain-text part always. There is no instance-name setting today, so the admin host identifies the instance.
9. **Test send.** A button on the Settings page that sends a fixed test message through the saved configuration and shows the provider's answer.
10. **Out of scope.** SMTP, several recipients, per-kind toggles, a certificate "N days left" event, a 5xx-spike alert (needs epic `t97`), alerts when hoster or the host is down.

## Acceptance Criteria
- [ ] ADR written with alternatives (JSON webhook, SMTP, config in `hoster.toml`, a broadcast subscriber) and consequences. Status lines of ADR-0045 and ADR-0024 updated. Index row added.
- [ ] OQ-37 in `docs/open-questions.md` replaced by its "Resolved → ADR" line.
- [ ] `docs/architecture.md` component overview (§2) and threads (§3) mention the notifier.
- [ ] The sibling tasks in epic `c5s` reference the ADR by number, and their bodies are corrected where the ADR differs from the proposals above.
- [ ] Task sent to review for owner sign-off.

## References
- OQ-37 in `docs/open-questions.md`
- ADR-0024, ADR-0025, ADR-0029, ADR-0045, ADR-0054, ADR-0055
- `docs/architecture.md` §2, §3
- `src/store/events.rs`, `src/store/types.rs` (`EventKind`, `Severity`), `src/web/settings.rs`