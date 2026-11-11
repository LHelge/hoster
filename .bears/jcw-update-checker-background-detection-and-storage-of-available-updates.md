---
id: jcw
title: "Update checker: background detection and storage of available updates"
status: open
priority: P2
created: "2026-11-09T17:22:55.324406067Z"
updated: "2026-11-09T17:22:55.324406067Z"
tags:
  - registry
  - store
  - orchestrator
depends_on:
  - ggf
  - tsg
parent: "6z3"
---

## Summary
A background task that finds available updates for every checkable third-party service, per ADR-0061, and stores them. It only detects; it never deploys (that's the auto-update task).

## Acceptance Criteria
- [ ] An `UpdateChecker` (in `src/registry/`, next to the poller, sharing its client and backoff style) runs at the interval from a typed single-row `update_settings` table (ADR-0061 §7, ADR-0058 §1: default 6 h, bounded 1 h–7 d, ±10 % jitter, last pass persisted so a restart doesn't trigger a full pass), edited in Settings → Updates. It also offers a "Check now" entry point for the UI.
- [ ] For each service of each active revision whose image is checkable:
  - a moved-tag check (HEAD digest vs. the revision's pinned digest);
  - for parseable tags, the newest same-line candidate across patch / minor / major. Store the highest of each kind, so the UI can show "patch available, major available".
- [ ] A reversible migration (`sqlx migrate add -r`) adds an `image_updates` table per the ADR, with the store API on the store type. The down migration restores the schema exactly; the store tests enforce this.
- [ ] Per-registry backoff on 429 / errors, honouring `Retry-After`. Errors are recorded per row (last error, last checked) without failing the pass.
- [ ] Rows are cleared or updated when a revision changes the service's image or tag, or the stack is removed.
- [ ] Results feed `UiEvent`s so the UI can refresh live.

## Testing
- Checker tests with a fake registry: moved tag, newer patch / minor / major, unparseable tag, a 429 backoff, credentials used, a row lifecycle on re-apply and on stack removal.
- Store migration up / down test.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0061; ADR-0022, ADR-0025, ADR-0058, ADR-0043, ADR-0052 (reversible migrations)
- `docs/architecture.md` §2, §3
- `src/registry/poller.rs`, `src/registry/client.rs`, `src/config.rs`, `src/store/`, `migrations/`