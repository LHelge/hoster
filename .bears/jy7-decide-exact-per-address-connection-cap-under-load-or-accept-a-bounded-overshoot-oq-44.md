---
id: jy7
title: "Decide: exact per-address connection cap under load, or accept a bounded overshoot (OQ-44)"
status: open
priority: P3
created: "2026-11-13T14:23:39.493687181Z"
updated: "2026-11-13T14:23:39.493687181Z"
tags:
  - decision
  - proxy
  - security
---

## Decision
Should the per-address cap of ADR-0065 be exact under load (reserve in the connection filter, with expiry), or does ADR-0065 accept that a burst from one address overshoots it by however far the accept loop runs ahead (about 3 idle, 80 seen on a loaded CI runner)?

Options, recommendation and background: `docs/open-questions.md` OQ-44. Recommendation: A, amend ADR-0065 to accept a bounded overshoot, no code change.

## Done when
- The owner has chosen; ADR-0065 is amended (A) or a task is filed to implement reservations (B).
- OQ-44 records the answer.

## References
OQ-44, ADR-0065, `src/proxy/admission.rs`, `src/proxy/tests/limits.rs` (`a_burst_from_one_address_overshoots_its_cap_by_little`), `.config/nextest.toml`.