---
id: sng
title: "CI: scheduled cargo audit for RustSec advisories"
status: open
priority: P1
created: "2026-11-10T12:55:41.461499Z"
updated: "2026-11-10T12:55:41.461499Z"
tags:
  - ci
  - security
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
  `rustsec/audit-check`. Pin the action by commit SHA like the other workflows.
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

## Acceptance

- [ ] A scheduled run on `main` is green with the current lockfile, or its findings are filed
- [ ] A lockfile with a known-vulnerable crate version fails the PR job (try it on a scratch branch)
- [ ] A scheduled finding opens an issue

## References

- `.github/workflows/ci.yml` (conventions: pinned actions, job naming)
- `docs/adr/0008-embedded-pingora-proxy.md` (Consequences: the version is pinned and upgraded deliberately)
- README "Cutting a release"