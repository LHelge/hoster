---
id: sng
title: "CI: scheduled cargo audit for RustSec advisories"
status: done
priority: P1
created: "2026-11-10T12:55:41.461499Z"
updated: "2026-11-11T21:30:29.321598202Z"
tags:
  - ci
  - security
attempts: 1
---

## Problem

hoster embeds its edge (Pingora, rustls, h2, instant-acme). A CVE in one of them is fixed only by
a hoster release, and nothing tells us when one is published: CI has no advisory check, and
Dependabot is not enabled for the repository.

## Do

- Add a workflow (`.github/workflows/audit.yml`) that runs `cargo audit` against `Cargo.lock`:
  - on a daily `schedule`, so an advisory published against an unchanged lockfile is seen;
  - on pushes and pull requests that touch `Cargo.toml` or `Cargo.lock`;
  - on `workflow_dispatch`.
- The scheduled run must reach the owner. A failed scheduled run only mails whoever last edited
  the workflow, so have it open (or update) a GitHub issue per advisory instead, e.g. with
  `rustsec/audit-check`. Pin that action by commit SHA, since it gets `issues: write` (the other
  workflows use tags for first-party and well-known actions).
- Fail on vulnerabilities. Report `unmaintained`, `unsound` and `yanked` as warnings without
  failing the run.
- If an advisory has to be ignored, do it in `.cargo/audit.toml` with a comment saying why and
  when to look again. Do not ignore anything up front.
- README "Cutting a release": add one line saying an advisory against a shipped dependency means
  a patch release.

## Owner step (not code)

- Turn on Dependabot **alerts** under Settings → Code security. It needs no file in the repo and
  covers the same RustSec data through the GitHub Advisory Database, in the Security tab.
  Dependabot version updates (`.github/dependabot.yml`) are not part of this task.

## Outcome (2026-11-11)

- `rustsec/audit-check` pinned at `858dc40f52ca…` (default branch, node24; v2.0.0 runs node20). It fails a check run only on non-informational vulnerabilities and opens one issue per advisory on `schedule`, skipping advisory IDs already in an issue title.
- The first audit found **RUSTSEC-2026-0459** (libsqlite3-sys 0.37 via sqlx 0.9.0; no sqlx release allows the fixed 0.39). hoster is not exposed (only its own hoster.db in a 0700 dir; the restore guide deletes stale `-wal`), so it is ignored in `.cargo/audit.toml` with that reason; t24 removes the ignore with the sqlx bump.
- Four unmaintained warnings, all transitive (basic-toml via askama; derivative, rustls-pemfile, unsafe-libyaml via pingora-core). The first scheduled run will open an issue for each.

## Acceptance

- [ ] A scheduled run on `main` is green with the current lockfile, or its findings are filed (after merge; PR run green, finding filed as t24)
- [ ] A lockfile with a known-vulnerable crate version fails the PR job (try it on a scratch branch)
- [ ] A scheduled finding opens an issue (after merge: the four unmaintained warnings)

## References

- `.github/workflows/ci.yml` (conventions: job naming, tags for actions)
- `docs/adr/0008-embedded-pingora-proxy.md` (Consequences: the version is pinned and upgraded deliberately)
- README "Cutting a release"