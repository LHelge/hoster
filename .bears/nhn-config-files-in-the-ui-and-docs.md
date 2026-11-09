---
id: nhn
title: Config files in the UI and docs
status: open
priority: P2
created: "2026-11-09T17:23:25.591456230Z"
updated: "2026-11-09T17:23:25.591456230Z"
tags:
  - web
  - docs
depends_on:
  - rp5
parent: xgf
---

## Summary
Show config files where operators look for them, and finish the documentation.

## Acceptance Criteria
- [ ] The stack overview service rows list files: container path, size, source (inline / secret name), mode. Secret content is never rendered.
- [ ] The apply preview's plan names services redeployed *because* a file changed, e.g. "app: file /etc/app/config.yml changed".
- [ ] The environment tab lists secrets used by files alongside env secrets, so a missing one is obvious before deploying.
- [ ] stack-format.md has a worked example (e.g. a generic reverse-proxy or metrics image configured by a file, nothing app-specific in hoster itself) and the UID / mode guidance. `aphid build` passes.

## Testing
- Web tests in `src/web/stacks/tests.rs` for overview, preview and environment rendering, including that secret content never appears.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M12 ADR (task 5jg); ADR-0029, ADR-0054
- `src/web/stacks/overview.rs`, `src/web/stacks/apply.rs`, `src/web/stacks/environment.rs`, `templates/`
- `site/content/wiki/stack-format.md`