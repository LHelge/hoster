---
id: ckc
title: "Telemetry page: nav item and the Logs tab across stacks"
status: open
priority: P2
created: "2026-11-12T19:35:00.245280479Z"
updated: "2026-11-12T19:35:00.245280479Z"
tags:
  - web
  - observability
depends_on:
  - yn7
parent: psq
---

## Summary
Add the Telemetry page of `docs/design/Telemetry.dc.html` with its **Logs** tab: the shared log view that task `yn7` builds, mounted with the all-stacks scope. One view, two scopes (owner, 2026-11-12): what the stack tab can filter, this page can filter too, plus a stack selector. The Traces tab comes with epic `qnh`.

## Acceptance Criteria
- [ ] Sidebar order Dashboard, Stacks, Telemetry, Certificates, Settings, System (`docs/design/README.md`), with an `Icon` for Telemetry from Lucide (ADR-0053); other existing items keep their place relative to these
- [ ] `/telemetry` redirects to `/telemetry/logs` until the Traces tab exists. The tab bar shows Logs with its record count for the range
- [ ] A stack selector: all stacks (the default), each stack with records in the range or in the current state, and `hoster` for hoster's own records without a stack (ADR-0068). `?stack=` is the same parameter the stack tab fixes
- [ ] The record list shows a stack column besides service in the all-stacks scope; service colours use the categorical palette of `docs/design/README.md` (Telemetry notes), as CSS classes, never inline styles
- [ ] Range presets 5m, 15m, 1h, 6h, 24h, 7d (shared with the stack tab), default 15m as in the mockup
- [ ] Every filter the shared view has landed (level chips, grep, field chips) works here, with the same parameters
- [ ] The query box is grep, labelled as such (OQ-43: no query language yet)
- [ ] Header figures of the mockup that hoster has: records per second over the last minute, the retention (7 days) and `logs.db`'s size; not the receiver line until the OTLP epic lands
- [ ] The breadcrumb is `~/telemetry/logs`; the page works at phone width per the shell rules of ADR-0054

## Implementation Notes
- Mount the shared view module with `LogScope::All`; do not copy handlers or templates from the stack tab
- Nav items are built in `src/web/` (see how `/events` is defined in `src/web/dashboard.rs`); the route table is in `src/web/mod.rs`

## Testing
- Handler tests: redirect, scope, stack selector including `hoster`, parameters shared with the stack tab
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (logs tab), `docs/design/README.md`, ADR-0067, ADR-0068, ADR-0054, ADR-0053, OQ-43
- `src/web/mod.rs`, `src/web/dashboard.rs`, the shared log view from `yn7`