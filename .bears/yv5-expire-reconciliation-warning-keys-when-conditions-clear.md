---
id: yv5
title: Expire reconciliation warning keys when conditions clear
status: open
priority: P3
created: "2026-11-17T23:46:06.067113116Z"
updated: "2026-11-17T23:46:06.067113116Z"
---

## Summary
Missing-container and foreign-object warnings are suppressed for the lifetime of the process, hiding recurrences and retaining stale keys.

## Acceptance criteria
Verify a persistent condition warns once, a resolved condition releases its key, and a recurrence warns again. Skipped or failed observations must not clear active warnings.

## References
- src/orchestrator/reconcile.rs (first_warning and pass)
- ADR-0028