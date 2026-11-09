---
id: mxq
title: Instance name as a run-time setting, shown in the sidebar and Settings → General
status: open
priority: P3
created: "2026-11-09T23:28:49.821515863Z"
updated: "2026-11-09T23:28:49.821515863Z"
tags:
  - store
  - web
  - ui
depends_on:
  - rja
parent: c5s
---

## Summary
Give this hoster a display name the operator can change while it runs (ADR-0058 §2). It is the first run-time setting stored in SQLite under ADR-0058 §1, so it also sets the pattern the notification settings follow: a typed single-row table, edited in Settings, effective without a restart. Mail subjects use it (`[<instance name>] …`).

## Acceptance Criteria
- [ ] A reversible migration (`sqlx migrate add -r`) adds a single-row table for instance settings with the name; the `.down.sql` restores the previous schema exactly.
- [ ] A typed `InstanceName` (validated with `FromStr`, shown with `Display`): trimmed, non-empty, a sensible length limit, no control characters or line breaks (it goes into a mail subject).
- [ ] Without a stored name the instance name is the machine's hostname, which is what the sidebar shows today. Nothing changes for an existing install until the operator sets one.
- [ ] Store methods to read and set the name, and to clear it back to the default.
- [ ] The sidebar shows the instance name where it shows the hostname now, on every page, without a store read per request beyond what the shell already does (cache it and refresh on change).
- [ ] Settings → General has an editable "Instance name" field with the help text from the design ("Shown in the sidebar and in notifications"), a save action and a reset to the default. The rest of General stays read-only, and the "set in `hoster.toml`" footer no longer claims to cover the editable field.
- [ ] One accessor that the notifier and the mail templates use to get the current name.

## Implementation Notes
- `src/web/pages.rs` reads the hostname once (`gethostname`) for the `Shell`; that value becomes the default.
- `src/web/settings.rs` and `src/web/settings/effective.rs` render General from `EffectiveConfig`; the name is not part of `EffectiveConfig`, since it is not configuration from `hoster.toml`.
- Form handling, CSRF and field errors as in Settings → Registries.

## Edge Cases
- A name with `<`, `&` or quotes is escaped in the sidebar and later in mail.
- Saving an empty field resets to the default instead of storing an empty name.

## Testing
- `InstanceName` parsing tests; store round trip and up/down migration; handler tests for save, validation error and reset; the sidebar shows a changed name on the next page load.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass.

## References
- ADR-0058 §1, §2; ADR-0024, ADR-0054
- `docs/design/Settings.dc.html` (General → Instance name)
- `src/web/pages.rs`, `src/web/settings.rs`, `src/web/settings/effective.rs`, `templates/settings.html`, `src/store/`, `migrations/`