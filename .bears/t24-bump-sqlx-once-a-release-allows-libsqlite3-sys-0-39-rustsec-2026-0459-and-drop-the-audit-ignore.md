---
id: t24
title: Bump sqlx once a release allows libsqlite3-sys >= 0.39 (RUSTSEC-2026-0459), and drop the audit ignore
status: open
priority: P2
created: "2026-11-11T21:29:12.665846303Z"
updated: "2026-11-11T21:29:12.665846303Z"
tags:
  - security
  - store
  - deps
---

## Problem
RUSTSEC-2026-0459 (CVE-2026-119375, medium): libsqlite3-sys 0.37 bundles SQLite < 3.54, which can return uninitialised page-cache memory when it opens a database with a crafted sibling `-wal`. The fix is libsqlite3-sys >= 0.39 (SQLite 3.54), but sqlx-sqlite 0.9.0, the newest sqlx (2026-05-21), requires `>=0.30.1, <0.38.0`.

hoster is not exposed (it opens only its own hoster.db in its 0700 data dir; the restore guide deletes stale `-wal`/`-shm`), so sng ignores the advisory in `.cargo/audit.toml` with that reason. This task removes the ignore with the bump.

## Do
- Watch sqlx releases for one whose `sqlx-sqlite` allows libsqlite3-sys 0.39. When it lands: `cargo update -p sqlx` (or `cargo add sqlx@<ver>` if it is a breaking bump), run the store tests (migrations up/down), and remove `RUSTSEC-2026-0459` from `.cargo/audit.toml`.
- Ship it in the next patch release (README "Cutting a release").

## Acceptance
- [ ] `cargo audit` passes with `RUSTSEC-2026-0459` no longer in `.cargo/audit.toml`
- [ ] `cargo tree -i libsqlite3-sys` shows >= 0.39

## References
- `.cargo/audit.toml`, `Cargo.toml` (`sqlx`), `src/store/`
- https://rustsec.org/advisories/RUSTSEC-2026-0459