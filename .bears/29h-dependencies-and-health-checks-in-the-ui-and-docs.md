---
id: "29h"
title: Dependencies and health checks in the UI and docs
status: done
priority: P2
created: "2026-11-09T17:23:50.060462255Z"
updated: "2026-11-09T22:27:24.607712944Z"
tags:
  - web
  - docs
depends_on:
  - kbh
parent: qq9
attempts: 1
---

## Summary
Show what M11 added, and document it.

## Acceptance Criteria
- [ ] Stack overview: each service shows its `depends_on`, which health check is in effect (declared exec / http / tcp, or the default) with the command or path, and "waiting for <dep>" when the reconciler defers it.
- [ ] Apply preview: the start order (waves) of the services being (re)deployed.
- [ ] Deployment events render wave and exec-probe events readably, with the output tail collapsed.
- [ ] stack-format.md: `depends_on` and `health.exec` in §2.2 / §2.4, the precedence rules, a new worked example (§3.x) of a generic app + postgres + redis stack using `depends_on`, exec checks and `update = "stop-first"` where it matters. architecture.md §5 is consistent with the implementation. `aphid build` passes.

## Testing
- Web tests in `src/web/stacks/tests.rs` for the overview and preview rendering.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M11 ADR (task 4qn); ADR-0054
- `src/web/stacks/overview.rs`, `src/web/stacks/apply.rs`, `src/web/stacks/deployments.rs`, `templates/`
- `site/content/wiki/stack-format.md`, `docs/architecture.md` §5