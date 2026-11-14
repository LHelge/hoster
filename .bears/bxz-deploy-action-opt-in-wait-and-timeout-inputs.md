---
id: bxz
title: "Deploy action: opt-in wait and timeout inputs"
status: open
priority: P2
created: "2026-11-14T21:53:58.299637565Z"
updated: "2026-11-14T21:53:58.299637565Z"
tags:
  - ci
  - docs
depends_on:
  - mqu
parent: aax
---

## Summary
`wait` (default `false`) and `timeout` (default `10m`) inputs on `actions/deploy`. With `wait: true` and an `accepted` result, poll the status endpoint (`mqu`) until terminal.

## Acceptance Criteria
- [ ] `Live`/`LiveWithWarnings` → exit 0 (warnings shown); `Failed`/`Rejected`/`Superseded` → non-zero with state and reason; timeout → non-zero naming the last state; `noop` → exit 0 without waiting
- [ ] Deployment URL printed and the outcome added to the job summary in every case; a new `state` output
- [ ] Transport errors and 502/503 while polling retried; token re-minted as the ADR requires
- [ ] Default (`wait` unset) behaves exactly as today
- [ ] `actions/deploy/stub.py` serves the status endpoint; `test.sh` covers live, failed, superseded, timeout and the default
- [ ] `site/content/wiki/deploying-from-ci.md` documents the inputs; examples updated where useful; `aphid build` passes

## References
- `actions/deploy/` (action.yml, deploy.sh, stub.py, test.sh), `site/content/wiki/deploying-from-ci.md`, `examples/`, ADR from `msm`