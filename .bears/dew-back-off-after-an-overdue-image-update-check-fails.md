---
id: dew
title: Back off after an overdue image update check fails
status: open
priority: P2
created: "2026-11-17T23:46:05.480121912Z"
updated: "2026-11-17T23:46:05.480121912Z"
---

## Summary
An overdue last-pass timestamp yields zero delay if a pass fails before saving a new timestamp.

## Acceptance criteria
With a persistent discovery or store failure, verify retries are spaced apart and successful checks resume normal scheduling.

## References
- src/registry/updates.rs
- ADR-0061