---
id: thv
title: "Decide: connection caps and slow-client policy at the edge (OQ-39)"
status: done
priority: P1
created: "2026-11-11T14:13:10.559548706Z"
updated: "2026-11-11T21:55:35.865144955Z"
tags:
  - decision
  - proxy
  - security
---

## Summary
The limits survey (task `95p`, architecture §9.1) left four cases with no bound, each a policy choice: (1) client connections are not capped, per address or in total, so about a thousand idle connections exhaust the default 1024 file descriptors and hoster then accepts nothing on `:80`, `:443` or the admin socket until they close; (2) a slow-but-moving request header or body is never cut; (3) an `h2` connection that never sends the preface is never closed (no Pingora setting reaches it); (4) no limit on requests per kept-alive connection. Options and recommendation in docs/open-questions.md OQ-39: A leave it, B total connection cap below a raised `LimitNOFILE=`, C per-address cap, D request rate limiting, E minimum transfer rate, F report the preface timeout upstream. Recommended: B + C + F.

## Acceptance Criteria
- [ ] Owner decides; ADR written (amends ADR-0008/ADR-0032 as needed)
- [ ] Follow-up implementation task(s) filed: the cap(s) in `src/proxy`, `LimitNOFILE=` in the unit, architecture §9.1 rows and their tests in `src/proxy/tests/limits.rs` and `tests/supervisor.rs`, the upstream report

## References
- docs/open-questions.md OQ-39
- docs/architecture.md §9.1
- `src/proxy/mod.rs` (`DownstreamLimits`), `deploy/systemd/hoster.service`