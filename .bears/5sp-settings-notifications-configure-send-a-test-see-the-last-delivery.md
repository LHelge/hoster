---
id: "5sp"
title: "Settings → Notifications: configure, send a test, see the last delivery"
status: open
priority: P3
created: "2026-11-09T23:23:24.838900008Z"
updated: "2026-11-09T23:29:11.834719784Z"
tags:
  - web
  - ui
  - notify
depends_on:
  - q55
  - pzp
  - "6qz"
  - mxq
parent: c5s
---

## Summary
The Settings section the design shows only as a nav item (`docs/design/Settings.dc.html`): the operator picks the provider, enters the API key, the from address and the recipient, switches notifications on, sends a test mail and sees whether the last delivery worked. It is the second editable Settings section after Registries, and the first whose settings live in SQLite (ADR-0058).

## Acceptance Criteria
- [ ] `SettingsPart::Notifications` at `/settings/notifications`, in the sub-nav where the design places it.
- [ ] A form with the provider (Resend is the only choice, but the control is driven by the provider enum), API key, from address, recipient, "enabled" and "also notify on successes".
- [ ] The API key is write-only: the page shows whether one is stored and when it was last changed, never the value. Leaving the field empty on save keeps the stored key.
- [ ] Saving validates the addresses with the typed address and shows field errors in the page; it stores the key in the secret store and the rest in the settings row.
- [ ] **Send test** delivers the test message through the saved configuration and shows the result inline: sent, or the provider's error in plain words (invalid key, sender domain not verified, quota used up, rate limited, network).
- [ ] A status block: the last delivery's time and result, and a link to the Events page filtered to delivery failures.
- [ ] The test mail's subject uses the instance name (task `mxq`).
- [ ] A short note under the from address that it must be on a domain verified at the provider.
- [ ] A way to remove the configuration (key and settings), with the confirm step the secret pages use.
- [ ] Changes take effect without restarting hoster.
- [ ] The module doc comment of `src/web/settings.rs` is updated: Registries is no longer the only editable section.

## Implementation Notes
- Model the handlers, the write-only credential and the "Test" action on Settings → Registries in `src/web/settings.rs`; reuse the design system's form and status components (ADR-0054). No new JavaScript; htmx for the test result.
- CSRF and session handling as on the other POST forms.

## Edge Cases
- Test pressed with nothing saved: a plain message, no request sent.
- Enabled without a key or an address is refused on save.
- The test send obeys a small cooldown so the button can't be used to burn the provider's quota.

## Testing
- Handler tests in `src/web/settings/tests.rs`: render when unconfigured and configured, save and validation errors, key kept on empty field, test send success and failure with a fake provider, removal.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass.

## References
- ADR-0058, ADR-0025, ADR-0029, ADR-0054
- `docs/design/Settings.dc.html`
- `src/web/settings.rs`, `src/web/settings/tests.rs`, `templates/settings.html`, `templates/settings_base.html`, `templates/registries.html`