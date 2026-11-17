---
id: mu3
title: Preserve stop-first exclusivity when recovery cleanup fails
status: open
priority: P1
created: "2026-11-17T23:46:05.396069518Z"
updated: "2026-11-17T23:46:05.396069518Z"
---

## Summary
Recovery continues to the committed generation even if the obsolete generation could neither be stopped nor removed.

## Acceptance criteria
Simulate both runtime calls failing. Hold the committed stop-first service while the conflicting container may still run, and resume once retirement is confirmed.

## References
- src/orchestrator/reconcile.rs
- ADR-0039, ADR-0048