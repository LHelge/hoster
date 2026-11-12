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

- **CPU column:** the current container's CPU % (basis as ADR-0066 defines), right-aligned mono, with a small `Sparkline` of the last hour; shown against `limits.cpus` where the service sets one (ADR-0059).
- **Memory column:** usage, and `/ <limit>` with a `Meter` filled to usage/limit where `limits.memory` is set (`ServiceRow.limits`); without a limit, usage alone and no meter (or a sparkline only). Tone thresholds per ADR-0066.
- The muted rows of other containers (draining old generation, starting new one) show their own figures; a service without a running container shows `—`.
- **Live:** the stack page's SSE task (`src/web/stacks/overview.rs`, subscribed to `ui_events`) also wakes on the sampler's change signal and re-renders the same fragment it already sends, so the columns move every 10 s without a new connection or endpoint. Make sure this does not re-run expensive queries needlessly: if the fragment re-reads traffic history or lists containers from podman on every render, restrict what a sampler tick re-renders or reuse what is cached, and say what you chose.
- The stack cards on `/stacks` and the dashboard are out of scope.
- The stack-format wiki page's limits section (`site/content/wiki/stack-format.md`) gets one sentence saying usage against the limits is shown on the stack overview; `aphid build` in `site/` must still pass if the tool is installed.

## Acceptance Criteria

- [ ] Web tests: a row with limits (meter, `/ limit`), one without, the unknown state, a draining container's row
- [ ] The SSE stream sends a fresh fragment after a sampler tick (test with the sampler's tick driven by hand)
- [ ] No inline styles; CSS in the stylesheet with existing tokens
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0066, ADR-0054, ADR-0059; `docs/design/StackDetail.dc.html` (services table only), `src/web/stacks/overview.rs`, `src/web/stacks/overview/services.rs`, `templates/stack.html`, `src/web/ui.rs`, `src/web/charts.rs` (`Sparkline`).