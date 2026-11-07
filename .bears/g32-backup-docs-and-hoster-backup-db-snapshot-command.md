---
id: g32
title: Backup docs and `hoster backup db` snapshot command
status: in_progress
priority: P3
created: "2026-11-07T15:15:31.710389846Z"
updated: "2026-11-07T22:46:37.288762274Z"
tags:
  - ops
  - docs
depends_on:
  - h3a
parent: ame
attempts: 1
---

## Summary
Implement the v1 backup story from ADR-0030: a consistent SQLite snapshot command and docs/backup.md.

## Acceptance Criteria
- [ ] `hoster backup db <file>` writes a consistent snapshot via `VACUUM INTO` while hoster is running
- [ ] docs/backup.md lists: DB snapshot, secret-store key (stored separately), acme/ and certs/, volumes via `podman volume export`, and restore steps
- [ ] Restore procedure tested once on a scratch instance (noted in PR)

## Testing
- Integration test: snapshot while writes are happening opens cleanly and passes `PRAGMA integrity_check`
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0030, ADR-0029