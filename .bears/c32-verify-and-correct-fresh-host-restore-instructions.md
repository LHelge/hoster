---
id: c32
title: Verify and correct fresh-host restore instructions
status: done
priority: P1
created: "2026-11-17T23:46:05.311280184Z"
updated: "2026-11-17T23:55:49.546165469Z"
attempts: 1
---

## Summary
The restore guide promises startup recreates services from committed digests. Recovery only warns about absent containers.

## Acceptance criteria
Document a verified route from a restored database and volume on an empty runtime to a serving app. Verify a request reads the restored data and record any missing recovery step.

## References
- site/content/wiki/backup.md
- src/orchestrator/reconcile.rs
- ADR-0028, ADR-0030