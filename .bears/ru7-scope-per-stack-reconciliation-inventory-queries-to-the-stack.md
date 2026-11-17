---
id: ru7
title: Scope per-stack reconciliation inventory queries to the stack
status: open
priority: P2
created: "2026-11-17T23:46:05.899850465Z"
updated: "2026-11-17T23:46:05.899850465Z"
---

## Summary
After initial discovery, each claimed stack fetches the entire host container list again.

## Acceptance criteria
Keep the fresh inventory read under the stack claim. Verify per-stack runtime queries filter by stack and preserve ownership checks and obsolete-generation cleanup.

## References
- src/orchestrator/reconcile.rs
- src/runtime/podman.rs
- ADR-0028