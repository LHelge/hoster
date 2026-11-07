---
id: "8eh"
title: Stack file model, TOML parsing and validation
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.197524343Z"
updated: "2026-11-07T22:11:50.656523930Z"
tags:
  - stack
depends_on:
  - mnu
  - hft
parent: ame
attempts: 1
---

## Summary
Implement the stack definition types and validation from docs/stack-format.md as a pure module.

## Acceptance Criteria
- [ ] serde types for version, name, drain_timeout, deploy (repository_id required; refs, environment, job_workflow_ref — ADR-0047), services (image, track, command, entrypoint, env, secrets, volumes, health, update, stop_timeout, user, read_only, routes) with `deny_unknown_fields`
- [ ] Validation per stack-format §2: name regexes, host FQDN rules, prefix normalization into segments, strip_prefix on '/' warning, port range, duration parsing, image rules per ADR-0025 (CI ghcr images without tag; third-party `repo:tag`, `repo:tag@digest` or `repo@digest`)
- [ ] Errors carry TOML line/column spans and a field path; warnings returned separately
- [ ] The worked example in docs/stack-format.md §3 is a test fixture and validates cleanly
- [ ] `track` only allowed on CI-built ghcr images (ADR-0043); `health` not allowed on non-routed services (ADR-0038); `resources` is not in v1 (ADR-0038) and is rejected as unknown

## Implementation Notes
- Use `toml` + `serde_spanned` for spans
- Expose `NormalizedPrefix` type reused by routing

## Edge Cases & Considerations
- Uppercase hosts, trailing dots, ports in host, `//`, `..`, `?` in prefix — all rejected or normalized consistently

## Testing
- Table-driven tests for every validation rule; snapshot test of error messages (insta)
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/stack-format.md
- ADR-0009, ADR-0010, ADR-0013