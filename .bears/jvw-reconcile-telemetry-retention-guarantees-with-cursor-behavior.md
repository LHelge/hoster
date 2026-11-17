---
id: jvw
title: Reconcile telemetry retention guarantees with cursor behavior
status: open
priority: P3
created: "2026-11-17T23:46:06.150578287Z"
updated: "2026-11-17T23:46:06.150578287Z"
---

## Summary
Retention preserves each table’s highest-ID row indefinitely on an idle host to prevent cursor ID reuse.

## Acceptance criteria
Cover quiet tables beyond their retention period. Make the documented retention behavior accurate and verify existing pagination and follow cursors remain valid when ingestion resumes.

## References
- src/store/logs/retention.rs
- site/content/wiki/logging.md
- site/content/wiki/backup.md
- ADR-0067, ADR-0069