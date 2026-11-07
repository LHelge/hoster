---
id: f9t
title: "Spike: systemd socket activation for :80/:443 across hoster restarts"
status: done
priority: P3
created: "2026-11-07T15:57:16.041553238Z"
updated: "2026-11-07T23:20:53.112541524Z"
tags:
  - proxy
  - ops
  - spike
depends_on:
  - vhg
parent: up4
attempts: 1
---

## Summary
Time-boxed investigation per ADR-0034: can a `hoster.socket` user unit (ListenStream=80/443, works with the ADR-0023 sysctl) hand listening fds to Pingora so that connections queue in the kernel backlog during a hoster restart instead of being refused?

## Acceptance Criteria
- [ ] Findings note in docs/ (or an ADR draft): whether Pingora 0.9.x can adopt inherited listener fds (its upgrade path uses fd transfer internally), what code would be needed, and measured client-visible behaviour during a restart with vs without socket activation
- [ ] Recommendation: adopt, defer, or drop

## Implementation Notes
- Look at pingora-core `server/transfer_fd` and `listeners` for how inherited fds are bound
- Prototype only; no production code merged unless the owner approves

## References
- ADR-0034, ADR-0023