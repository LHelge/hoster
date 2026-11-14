---
id: "82j"
title: Descriptor-count proxy tests fail under the full parallel test suite
status: proposed
priority: P2
created: "2026-11-12T20:07:01.324862752Z"
updated: "2026-11-12T20:07:04.388553247Z"
tags:
  - test
  - proxy
---

## Summary
On `main` (915fc53, 2026-11-12) a full `cargo test` fails `proxy::tests::limits::new_clients_are_refused_at_the_descriptor_cap` (limits.rs:458, `assert_eq!(h.admission.refused(), 1)`) and `proxy::admission::tests::proc_counts_this_process_without_the_reading_descriptor`. Both pass when run on their own. On the log store branch, `h2_control_frame_floods_do_not_grow_memory` (process memory growth) and `a_silent_client_is_dropped_after_sixty_seconds` also failed once each, including when only `proxy::tests::limits` ran: the same kind of process-wide measurement under parallel load. Both read the whole process's descriptor count (`/proc/self/fd`), which other tests running in parallel change.

## Acceptance Criteria
- [ ] Both tests pass reliably in a full parallel `cargo test` (10 runs in a row)
- [ ] The fix keeps what they test (the cap counts this process's descriptors). Options: serialise them against fd-heavy tests with a shared lock, allow for concurrent descriptor churn in the assertions, or move them into their own test binary (`tests/`) so no other test shares the process

## References
- ADR-0065, `src/proxy/tests/limits.rs`, `src/proxy/admission.rs`; commits 81086ce, dbf052a (earlier flake fixes in these tests)