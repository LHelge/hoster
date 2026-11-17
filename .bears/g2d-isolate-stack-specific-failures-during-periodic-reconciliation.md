---
id: g2d
title: Isolate stack-specific failures during periodic reconciliation
status: open
priority: P2
created: "2026-11-17T23:46:05.983625904Z"
updated: "2026-11-17T23:46:05.983625904Z"
---

## Summary
A stack-local read or definition error exits the pass before later stacks are reconciled.

## Acceptance criteria
With one failing stack, verify a later healthy stack is reconciled in the same periodic pass and the failed stack is reported and retried. Preserve host-wide error handling.

## References
- src/orchestrator/reconcile.rs (pass and pass_stack)
- ADR-0028