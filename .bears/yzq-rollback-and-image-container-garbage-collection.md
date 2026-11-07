---
id: yzq
title: Rollback and image/container garbage collection
status: open
priority: P2
created: "2026-11-07T14:45:17.310490069Z"
updated: "2026-11-07T21:12:34.750596925Z"
tags:
  - orchestrator
depends_on:
  - shg
parent: mdd
---

## Summary
Rollback = redeploy a previous successful deployment's digest set and config revision (ADR-0041); prune images while retaining rollback candidates: current + last 3 successful per service.

**Correction to ADR-0041's wording:** hoster cannot label images — labels on a pulled image come from its build, so there are no `hoster.managed` images. GC must identify hoster's images itself: keep an `images` table (digest, repo, pulled_at) written by `p7a` on every successful pull, and prune only digests in that table that are outside every service's retention set. Images hoster did not pull are never touched.

## Acceptance Criteria
- [ ] Rollback{stack, deployment_id} creates a normal deployment with source=rollback; it uses that deployment's digests and stack revision but the **current** secret values (old secret values are not retained)
- [ ] `images` table maintained on pull; GC computes per-service retention (current + last k successful digests) and removes other recorded images daily via the runtime
- [ ] Images not in the table are left alone; a removal failure (image in use) is logged and retried next run
- [ ] Poller does not undo a rollback (verified once M7 lands; note in `68p`)

## Testing
- FakeRuntime tests for retention set computation, "foreign image untouched", and rollback flow
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0041, ADR-0030