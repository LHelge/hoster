---
id: x38
title: "Log store: hoster rollback undoes logs.db migrations too"
status: open
priority: P2
created: "2026-11-12T19:53:26.724667671Z"
updated: "2026-11-12T19:53:26.724667671Z"
tags:
  - store
  - update
depends_on:
  - "7ne"
parent: f8z
---

## Summary
`logs.db` has its own reversible migrations (ADR-0067 §2, ADR-0052), and `LogStore::open` refuses a file migrated by a newer hoster. So `hoster rollback`, which the newer binary runs, has to undo `logs.db` to the previous binary's log schema as well. Otherwise the older binary comes back up with its log store off. Found while building `7ne`: rollback only knows `hoster.db`'s schema, via `BuildInfo.schema`.

## Acceptance Criteria
- [ ] `BuildInfo` (and `hoster version --json`) carries the log schema as well (`log_schema`), read as absent from binaries that predate the log store
- [ ] `Updater::rollback` undoes `logs.db` to the previous binary's `log_schema` after `hoster.db`, with `LogStore::undo_migrations`; when the previous binary has no log store (`log_schema` absent), `logs.db` is left as it is (that binary never opens it)
- [ ] A failed undo of `logs.db` does not fail the rollback or restore `hoster.db`'s snapshot: logs are expendable (ADR-0067 §9). The file is moved aside to `logs.db.rollback-<version>` instead, and the outcome says so
- [ ] The rollback plan shown before confirming says that log records stored by the newer schema may be lost
- [ ] Tests in `src/update/tests.rs` / `cli_tests.rs` beside the existing rollback tests

## References
- ADR-0052, ADR-0067 (§2, §9)
- `src/update/mod.rs` (`RollbackPlan`, `rollback`), `src/installation/mod.rs` (`BuildInfo`), `src/store/logs/`