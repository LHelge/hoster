# ADR-0058: Email notifications for problems and recoveries, sent through a provider trait (Resend first), configured at run time in SQLite

- Status: Accepted (amends ADR-0045 and ADR-0024; resolves OQ-37)
- Date: 2026-11-10

## Context

ADR-0045 keeps observability in the web UI and has no outbound alerting. Its consequences name
that "the main gap to revisit after v1": the operator learns about a failed deploy, a crash loop
or a failing certificate only by opening the dashboard or the journal. The event log (ADR-0055)
now records all of these in SQLite and was built as "the basis for notifications".

The design draws no notifications screen. `docs/design/Settings.dc.html` has a nav item, and
says of the instance name that it is "shown in the sidebar and in notifications".

The owner's answers to OQ-37 (2026-11-10): email, not a chat webhook; Resend first, behind a
small provider trait, with other HTTP mail APIs (Mailjet and similar) expected before SMTP;
configuration in the database, with `hoster.toml` kept for what is essential to bring the server
up; one recipient; an instance name as a run-time setting; mail only, no SMS.

Until now every setting came from `hoster.toml` and `HOSTER_*` (ADR-0024), and Settings shows
them read-only. The one exception is registry credentials, which are edited in the UI and kept in
the secret store (ADR-0025, ADR-0029).

## Decision

**1. Run-time settings live in SQLite.** This amends ADR-0024:

- `hoster.toml` and `HOSTER_*` hold what hoster needs to start and be reached: the state
  directory, the admin host and identity, listeners, ACME, ports and credentials files.
- Settings the operator changes while hoster runs live in SQLite, are edited in Settings and
  take effect without a restart. They have no `hoster.toml` key and no environment override.
- Each area gets its own typed single-row table, not a key/value table, so columns are typed
  and checked at compile time like the rest of the store.
- Credentials among them go in the secret store (ADR-0029) and are write-only in the UI, as
  registry credentials are.

The first two such areas are the instance name (§2) and notifications (§3 onwards).

**2. Instance name.** A short display name for this hoster, editable in Settings → General. It
defaults to the machine's hostname, which is what the sidebar shows today. It is shown in the
sidebar and used in every mail's subject and footer. It is a label only: nothing is derived from
it, and links in mails use the admin host.

**3. Channel: email through an HTTP mail API.** A `Provider` trait with one method that delivers
a `Message` (sender, recipient, subject, plain text, HTML, an idempotency key) and returns a
typed error. The error separates **retryable** (network error, timeout, 5xx, a per-second rate
limit) from **permanent** (rejected key, unverified sender, validation error, an exhausted daily
or monthly quota) and carries the provider's message for the UI. The provider is a typed enum
stored with the settings; adding one means a new variant and a new type, with no change to
callers. The trait is for mail. SMS and chat are not designed for.

**4. Resend is the first provider.** One request per mail with the existing `reqwest` client:

- `POST https://api.resend.com/emails`, `Authorization: Bearer <key>`, JSON body with `from`,
  `to`, `subject`, `html` and `text`.
- A `User-Agent: hoster/<version>` header. Resend rejects requests without one (403).
- An `Idempotency-Key` header derived from the event, so a retry after a timeout cannot send
  the same mail twice. Resend keeps keys for 24 hours.
- 401 and 403 are permanent: an invalid or suspended key, or a from address on a domain that is
  not verified at Resend. 429 `rate_limit_exceeded` and 5xx are retryable, honouring
  `retry-after` when present. 429 `daily_quota_exceeded` and `monthly_quota_exceeded` are
  permanent for the retry window.

(Checked against Resend's API reference on 2026-11-10.)

**5. Configuration.** One `notification_settings` row: provider, from address, recipient,
enabled, "also notify on successes". The API key is in the secret store. One recipient.
Notifications are off until the row exists and is enabled. Settings → Notifications edits it,
has a **Send test** button that delivers a fixed test message through the saved configuration
and shows the provider's answer, and shows the last delivery's time and result.

**6. The notifier reads the event log past a cursor.** A background task on the orchestrator
runtime reads `events` rows with an id above a persisted cursor. The `ui_events` broadcast wakes
it and a one-minute timer is the fallback, so it never depends on the broadcast keeping up
(ADR-0055 §3). The cursor moves to the newest event when notifications are enabled, and events
older than one hour are skipped, so neither enabling nor a long outage sends stale mail. The
cursor and the de-duplication record advance in one transaction.

**7. Which events send mail.**

| Set | Kinds |
|---|---|
| Problems | `DeployFailed`, `DeployRolledBack`, `CrashLoopEntered`, `CertificateFailed`, `DnsMismatch`, `HealthChanged` to unhealthy, `HosterRolledBack` |
| Recoveries | `CrashLoopLeft`; `HealthChanged` back to healthy; `CertificateIssued` / `CertificateRenewed` for a host whose problem was mailed; `DeploySucceeded` for a stack whose last mailed deploy event was a problem |
| Always | `HosterUpdated` |
| Only with "also successes" | every `DeploySucceeded`, `CertificateIssued`, `CertificateRenewed` |
| Never | `DeployStarted`, `ContainerRestarted`, `StackApplied`, `StackRemoved`, `SecretSet`, `SecretDeleted`, `HosterStarted`, `NotificationFailed` |

The set is fixed. There are no per-kind toggles.

**8. One mail per state change.** Problems are keyed by (kind, stack, subject). While a key's
problem is open, repeats are silent; a reminder is sent if it is still failing 24 hours after the
last mail. A recovery closes the key and is mailed only if the problem was. Deploy problems are
not held back this way: each failed or rolled-back deployment is its own mail. On top of this
there is a cap of 20 mails in any hour. Beyond it one summary mail says how many more events
there were and links to the Events page.

**9. Failed deliveries.** A retryable error is retried with back-off for up to five minutes,
then dropped. A permanent error is dropped at once. Either way the notifier records a
`NotificationFailed` event (severity `warning`) with the provider's reason, at most one per hour
while the provider keeps failing, and updates the last-delivery state. `NotificationFailed`
never sends mail.

**10. The mail.** Subject `[<instance name>] <the event's readable line>`. The body is the event
as the UI shows it: tone, message, the mono meta line, the time, and one button on the admin
host linking where the event's feed row does: the page it names, else its stack, else the Events
page. The summary links to the Events page and the test message to Settings → Notifications. It
is rendered with Askama from its own templates:

- HTML with table layout and inline styles only, colours copied from the design tokens of
  ADR-0054 (Latte, light), system sans and monospace font stacks, the tone as a coloured cell.
- No `<style>` block, web font, SVG or remote image, since mail clients drop or block them.
- A plain-text part with the same content, always.

**11. Not in this decision.** SMTP, SMS and chat webhooks; several recipients; per-kind toggles;
a certificate "N days left" event (a renewal failure is mailed and reminded daily instead); a
5xx-spike alert, which needs persisted traffic history (epic `t97`); any alert when hoster or
the host itself is down.

## Alternatives considered

- **One outbound JSON webhook (ntfy, Slack, Discord)**, as ADR-0045 suggested: the simplest to
  build, but the owner reads mail, not a chat tool, and each tool wants its own payload shape.
- **SMTP:** works with any mail host, but adds an SMTP client dependency, more configuration
  (host, port, TLS mode, credentials) and port-25/587 egress that some VPS providers block. It
  can become a provider later.
- **Resend hard-wired, no trait:** less code now, but the owner expects more HTTP mail APIs, and
  the trait is also what the tests fake.
- **Configuration in `hoster.toml` with a credential file** (ADR-0024 as it stands): no new
  pattern, but a test button next to settings that need a file edit and a restart is awkward,
  and the owner wants `hoster.toml` kept to the essentials.
- **A key/value settings table:** one migration for every future setting, but untyped values
  and no compile-time checked queries.
- **A notifier that subscribes to `ui_events` only:** no cursor to persist, but it loses events
  when it lags or while hoster restarts, which is when alerts matter.
- **Queue undelivered mail until the provider works again:** nothing is lost, but an hours-old
  "deploy failed" arriving after the fix is noise. The event log keeps the history.
- **Per-kind toggles:** flexible, but more UI and state than one operator needs. The fixed set
  can grow a toggle later.

## Consequences

- hoster makes outbound HTTPS requests to a third party, and event text (stack and service
  names, hosts, image references, error messages) leaves the server in mail. Secret events are
  never mailed.
- The sender needs a domain verified at Resend. Until then only Resend's test sender works, and
  only to the account owner's address. The Settings page and the guide say so.
- Mail is best effort. A provider outage longer than five minutes loses those mails; the
  `NotificationFailed` event and the Settings status show it afterwards.
- Nothing is sent when hoster or the host is down. An external uptime check is still needed for
  that.
- Settings gains editable sections whose values are not in `hoster.toml`. The read-only sections
  keep their "set in `hoster.toml`" footer; the editable ones don't have it.
- The settings and the API key are in `hoster.db`, so backups (ADR-0030) include them, and
  restoring a backup restores them. Down-migrating (ADR-0052 rollback) drops the tables; the
  operator configures notifications again after a rollback past this release.
- `EventKind` gains `NotificationFailed`, which appears on the Events page under the operator
  and system group.
- The mail template holds a third copy of the design tokens, after `assets/hoster.css` and
  `site/theme/`. A token change has to update all three.
- A health-flapping service sends a problem and a recovery mail per flap until the hourly cap
  holds it back.
