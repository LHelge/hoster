---
id: zyd
title: "Decide: dot segments and non-normal paths at the edge (OQ-40)"
status: open
priority: P2
created: "2026-11-11T14:29:35.099064549Z"
updated: "2026-11-11T14:29:35.099064549Z"
tags:
  - decision
  - proxy
  - security
---

## Summary
The proxy matches and forwards paths exactly as written (ADR-0009): no dot-segment resolution, slash merging or percent-decoding. The adversarial tests of task `fq5` show that no spelling reaches another service than its literal segments name, but that a prefix is not a boundary inside one service: `/api/../internal` matches `/api` and an app that resolves dot segments serves its own `/internal`. Options and recommendation in docs/open-questions.md OQ-40: A document only, B refuse `.`/`..` segments (plain or `%2e`) with 400, C normalise before matching, D also refuse `//`, `%2f` and backslashes. Recommended: B plus the documentation paragraph.

## Acceptance Criteria
- [ ] Owner decides; ADR written (amends ADR-0009/ADR-0010 as needed)
- [ ] Follow-up implementation task filed: the check in `src/proxy/router.rs` or `src/routing/table.rs`, the cases in `paths_are_matched_and_forwarded_literally` (`src/proxy/tests/smuggling.rs`), architecture §6.2, the stack-format guide

## References
- docs/open-questions.md OQ-40
- docs/architecture.md §6.2
- `src/proxy/tests/smuggling.rs` (`paths_are_matched_and_forwarded_literally`)