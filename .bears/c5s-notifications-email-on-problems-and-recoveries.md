---
id: c5s
title: "Notifications: email on problems and recoveries"
type: epic
status: done
priority: P3
created: "2026-11-08T18:22:30.061186156Z"
updated: "2026-11-10T01:35:18.477088104Z"
tags:
  - design-followup
  - observability
  - notify
  - web
---

From the design pass (epic 8bf). The design shows notifications only as a Settings nav item (`docs/design/Settings.dc.html`); the scope below was agreed with the owner on 2026-11-10 (OQ-37).

## Scope

hoster mails the operator when something needs them, and again when it is over. ADR-0045 names this "the main gap to revisit after v1"; the event log (ADR-0055) is what it filters.

- Email through an HTTP mail API. Resend is the first provider, behind a small provider trait; Mailjet and similar HTTP mail APIs are expected next, before SMTP. Mail only, no SMS.
- Configuration in SQLite and the secret store, edited in Settings → Notifications. `hoster.toml` stays for what is essential to bring the server up.
- One recipient address.
- An instance name as a run-time setting, shown in the sidebar and in mail subjects.
- A fixed "problems and recoveries" set of events, with one "also successes" toggle.
- One mail per state change, an hourly cap, a short retry and then a recorded delivery failure.
- Mail rendered with Askama in the admin UI's style (inline styles, plain-text part).

## Out of scope

SMTP, SMS and chat webhooks, several recipients, per-kind toggles, a certificate "N days left" event, a 5xx-spike alert (needs epic `t97`), alerts when hoster or the host itself is down.

## Acceptance Criteria

- [ ] The operator configures Resend in Settings, sends a test mail and receives it.
- [ ] A failed deploy, a crash loop and a failed certificate each produce one mail, and their recovery produces one more.
- [ ] A crash-looping service or a retrying certificate does not produce a mail per attempt.
- [ ] A broken key or sender shows up in Settings and in the event log.
- [ ] The docs site explains the setup and the limits.

## Order

`rja` (ADR-0058, owner sign-off) → `mxq`, `pzp`, `6qz` in any order, `q55` after `mxq` → `pym` and `5sp` → `7ga` (docs).

References: ADR-0058, OQ-37, ADR-0024, ADR-0029, ADR-0045, ADR-0055.