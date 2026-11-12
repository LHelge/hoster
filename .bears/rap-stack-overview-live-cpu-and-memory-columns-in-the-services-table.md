---
id: rap
title: "Stack overview: live CPU and memory columns in the services table"
status: open
priority: P2
created: "2026-11-12T00:08:17.756976088Z"
updated: "2026-11-12T00:08:17.756976088Z"
tags:
  - web
  - observability
depends_on:
  - bqt
parent: qmw
---

## Summary

CPU and Memory columns in the services table of the stack overview (`docs/design/StackDetail.dc.html` lines 186–194), fed by the resource sampler (task bqt, ADR-0066) and updated live over the page's existing SSE stream.

## What

- **CPU column:** the container's CPU % of one core (ADR-0066 §4), right-aligned mono, with a small `Sparkline` of the last hour; where the service sets `limits.cpus` (ADR-0059) also `/ <cpus × 100>%` and a `Meter` filled to usage/limit; without one, no meter.
- **Memory column:** usage, and `/ <limit>` with a `Meter` filled to usage/limit where `limits.memory` is set (`ServiceRow.limits`); without a limit, usage and a sparkline, no meter. Limits come from the revision, never from stats. Tones per ADR-0066 §10 (`Ok` < 80 %, `Warn` ≥ 80 %, `Fail` ≥ 95 % of a limit; `Neutral` without one).
- The muted rows of other containers (draining old generation, starting new one) show their own figures; a service without a running container shows `—`.
- **Live (ADR-0066 §9):** the stack page's one SSE stream today carries only the Recent events card (`StackFeed`, `src/web/events/live.rs`, `/stacks/{name}/events`, swapped into `#stack-events`). It also wakes on the sampler's change signal and then sends `<hx-partial>`s that replace the stack's containers' CPU and memory cells (ids keyed by container id), as the dashboard stream does for its events card (`src/web/events.rs`, `RecentEvents::partial`). The partials are rendered from the sampler's cache alone: a tick never lists containers from podman, reads the store or re-sends the events card. Check that a message carrying only partials leaves `#stack-events` as it is. Rows that appear or go away still change on reload only.
- The stack cards on `/stacks` and the dashboard are out of scope.
- The stack-format wiki page's limits section (`site/content/wiki/stack-format.md`) gets one sentence saying usage against the limits is shown on the stack overview; `aphid build` in `site/` must still pass if the tool is installed.

## Acceptance Criteria

- [ ] Web tests: a row with limits (meter, `/ limit`), one without, the unknown state, a draining container's row
- [ ] The SSE stream sends a fresh fragment after a sampler tick (test with the sampler's tick driven by hand)
- [ ] No inline styles; CSS in the stylesheet with existing tokens
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0066, ADR-0054, ADR-0059; `docs/design/StackDetail.dc.html` (services table only), `src/web/stacks/overview.rs`, `src/web/events/live.rs`, `src/web/events.rs` (`RecentEvents::partial`), `src/web/stacks/overview/services.rs`, `templates/stack.html`, `src/web/ui.rs`, `src/web/charts.rs` (`Sparkline`).