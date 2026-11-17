---
id: sd2
title: Preserve no-overwrite backup publication without hard links
status: open
priority: P2
created: "2026-11-17T23:46:05.732173066Z"
updated: "2026-11-17T23:46:05.732173066Z"
---

## Summary
When hard-link publication fails, the destination check and replacing rename are separate operations.

## Acceptance criteria
Exercise concurrent publication to one name on the fallback path. An existing or winning snapshot must remain intact, including when atomic no-replace publication is unavailable.

## References
- src/store/mod.rs (publish_snapshot)
- site/content/wiki/backup.md