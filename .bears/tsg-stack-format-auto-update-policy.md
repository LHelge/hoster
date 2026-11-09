---
id: tsg
title: "Stack format: `auto_update` policy"
status: open
priority: P2
created: "2026-11-09T17:22:16.239960436Z"
updated: "2026-11-09T17:22:16.239960436Z"
tags:
  - stack
depends_on:
  - "7ah"
parent: "6z3"
---

## Summary
Parse and validate the per-service `auto_update` policy from the M14 ADR.

## Acceptance Criteria
- [ ] `Service.auto_update: AutoUpdate` (an enum with `Default` = `Off`, kebab-case serde).
- [ ] Errors: `auto_update` other than `off` on an untagged (CI-built) image, together with `track`, or on a digest-only reference (`repo@sha256:…`, which has no tag to follow). `patch` / `minor` on a tag that doesn't parse as a `TagVersion` is an error naming the tag. Use the same parse rules as the registry task: share the type, which lives in `src/stack/image.rs`.
- [ ] Not part of the config hash, since changing the policy shouldn't redeploy. Confirm against the ADR.
- [ ] stack-format.md §2.2 documents the field, its risks, and the grouping rule. The schema is regenerated if zcu has landed.

## Testing
- Stack snapshots for each rule; a test that changing only `auto_update` produces a revision with no service changes in the plan.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M14 ADR (task 7ah)
- `site/content/wiki/stack-format.md` §2.2
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/stack/image.rs`, `src/orchestrator/plan.rs`