---
id: "7ga"
title: "Docs: notifications guide and install/backup notes"
status: open
priority: P3
created: "2026-11-09T23:23:38.041736865Z"
updated: "2026-11-09T23:23:38.041736865Z"
tags:
  - docs
  - notify
depends_on:
  - pym
  - "5sp"
parent: c5s
---

## Summary
Tell operators how to turn notifications on and what to expect from them, on the docs site.

## Acceptance Criteria
- [ ] A wiki page in `site/content/wiki/` covering: creating a Resend API key and verifying a sender domain, filling in Settings → Notifications, the test mail, which events send mail and which don't, how repeats and the hourly cap behave, and what a failed delivery looks like in the Events page.
- [ ] It states the limits plainly: nothing is sent when hoster or the host is down, one recipient, no SMTP yet.
- [ ] The install guide links to it as a suggested step after first login; the backup guide notes that the notification settings and key are part of the SQLite backup. Links use `[[slug]]`.
- [ ] `spec.md` and the README feature list mention notifications where they describe observability.
- [ ] `aphid build` in `site/` passes.

## References
- ADR-0058
- `site/content/wiki/` (install and backup pages), `spec.md`, `README.md`