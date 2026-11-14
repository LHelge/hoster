---
id: bxz
title: "Deploy action: opt-in wait and timeout inputs"
status: done
priority: P2
created: "2026-11-14T21:53:58.299637565Z"
updated: "2026-11-14T22:25:37.840421671Z"
tags:
  - ci
  - docs
depends_on:
  - mqu
parent: aax
attempts: 1
---

## Summary
`wait` (default `false`) and `timeout` (default `10m`) inputs on `actions/deploy`, as ADR-0070 decides. With `wait: true` and an `accepted` result, poll `GET /hooks/deploy/{stack}/deployments/{id}` (`mqu`) every 5 s until terminal, reusing the deploy's OIDC token.

## Acceptance Criteria
- [x] `Live` → exit 0; `LiveWithWarnings` → exit 0 with a `::warning::`; `Failed`/`Rejected`/`Superseded` → non-zero with state and reason; timeout → non-zero naming the last state (the deployment keeps running); `noop` → exit 0 without waiting
- [x] Deployment URL printed and the outcome added to the job summary in every case; a new `state` output
- [x] A `401` while polling mints a fresh token; transport errors, `502` and `503` are retried
- [x] Default (`wait` unset) behaves exactly as today
- [x] `actions/deploy/stub.py` serves the status endpoint; `test.sh` covers live, live with warnings, failed, superseded, timeout, a 401 re-mint and the default
- [x] `site/content/wiki/deploying-from-ci.md` documents the inputs and that `wait` needs a server with ADR-0070; `aphid build` passes. The examples and the docs deploy in release.yml are left without `wait`: they pin or run against servers that may predate ADR-0070

## References
- ADR-0070, `actions/deploy/` (action.yml, deploy.sh, stub.py, test.sh), `site/content/wiki/deploying-from-ci.md`, `examples/`