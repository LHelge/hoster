---
id: xsf
title: "Web UI: System page with version, check for updates, release notes and Update"
status: open
priority: P1
created: "2026-11-08T08:25:50.008118293Z"
updated: "2026-11-08T08:25:55.887046420Z"
tags:
  - web
depends_on:
  - wpm
parent: tbf
---

## Summary
A System page in the admin UI for seeing the running version and applying updates.

## Acceptance Criteria
- [ ] Nav entry "System"; page shows running version, target, schema version, and installed versions (current, previous)
- [ ] "Check for updates" (POST, CSRF-protected, HTMX) calls `update::check()` only when clicked; shows "up to date" or the new version with its release notes rendered safely (markdown → HTML with escaping/sanitising, or as preformatted text; no raw HTML from the release body)
- [ ] "Update to vX.Y.Z" button: disabled with an explanation while any deployment is in progress (re-checked server-side; a POST during a deploy gets a 409); on confirm runs `update::update()`, shows "restarting…" and polls until the new version answers, then reloads
- [ ] After a successful update the server performs a graceful shutdown (same path as SIGTERM in the supervisor) so systemd restarts the new binary
- [ ] Rollback is not offered in the UI (CLI only, ADR-0052); the page says how to roll back
- [ ] Errors (network, checksum mismatch, refused) are shown inline; nothing is applied on error

## Testing
- Handler tests with the updater's fake release source: up to date, update available, refused while deploying (409), checksum failure, CSRF rejection
- The shutdown trigger tested via the supervisor's cancellation token

## References
- ADR-0052 (Self-update)
- ADR-0021, ADR-0031
- src/web/