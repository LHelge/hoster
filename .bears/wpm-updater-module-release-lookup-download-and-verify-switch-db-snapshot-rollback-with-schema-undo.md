---
id: wpm
title: "Updater module: release lookup, download and verify, switch, DB snapshot, rollback with schema undo"
status: open
priority: P1
created: "2026-11-08T08:25:49.982318440Z"
updated: "2026-11-08T08:25:49.982318440Z"
tags:
  - core
  - ops
depends_on:
  - y4b
  - vug
parent: tbf
---

## Summary
A deep `update` module that the CLI and the web UI both call: find a release on GitHub, download the binary for this architecture, verify it against SHA256SUMS, install and switch it, and roll back by undoing migrations from the newer binary.

## Acceptance Criteria
- [ ] Release source trait with a GitHub implementation (`api.github.com/repos/LHelge/hoster/releases/latest` and `/releases/tags/vX.Y.Z`, unauthenticated, hoster's own User-Agent with no personal data) and a fake for tests; returns version, release notes (body) and asset URLs. `latest` never returns a pre-release
- [ ] `check()` → current vs latest version + notes (SemVer comparison; "up to date" when equal or newer)
- [ ] `update(version?)`:
  - refuses while any deployment is in progress (query the orchestrator / store)
  - downloads `hoster-<target>` for the running target and `SHA256SUMS`, verifies the hash (mismatch or missing entry → error, nothing installed)
  - installs into the layout, snapshots the DB (reuse the `hoster backup db` code) to a named pre-update snapshot in the state dir
  - switches `current`, prunes to current + previous
  - returns; the caller triggers graceful shutdown (UI) or `systemctl --user restart hoster` (CLI)
- [ ] `rollback()` (run by the current binary; see ADR-0052 steps): reads the previous version's schema via `versions/<prev>/hoster version --json`; stops the service; snapshots the DB; `Migrator::undo(target)`; verifies the DB is at `target`, else restores the snapshot and errors; switches `current`; starts the service. No previous version → clear error
- [ ] Errors are typed (thiserror) with messages naming what to do; no partial state is left on any failure path (temp files removed, `current` unchanged)
- [ ] Downloads are size-bounded and time-limited; only https URLs on github.com / objects.githubusercontent.com are followed
- [ ] docs/architecture.md §10 lists the new module

## Implementation Notes
- Reuse the existing HTTP client stack (reqwest/hyper with ring, ADR-0037). No new crypto provider.
- Service control behind a small trait (systemctl --user) so rollback is testable with a fake.

## Testing
- Fake release source + local files: happy path, checksum mismatch, missing asset for target, refused while deploying, rollback across 2 migrations (real SQLite, undo verified), rollback failure restores the snapshot
- One `#[ignore]`/env-gated (`TEST_HOSTER_GITHUB_LIVE`) test against the real API once a release exists

## References
- ADR-0052 (Self-update; Reversible migrations and rollback)
- ADR-0034, ADR-0037, ADR-0022
- docs/backup.md