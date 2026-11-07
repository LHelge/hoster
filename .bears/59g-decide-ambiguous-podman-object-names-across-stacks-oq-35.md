---
id: "59g"
title: "Decide: ambiguous podman object names across stacks (OQ-35)"
status: open
priority: P1
created: "2026-11-07T23:10:21.409833637Z"
updated: "2026-11-07T23:10:21.409833637Z"
tags:
  - decision
---

## Summary
Stack, service and volume names may contain `-`, so `hoster-<stack>-<volume>` and `hoster-<stack>-<service>-g<n>` can collide across stacks (stack `a-b` volume `c` vs stack `a` volume `b-c`). For volumes, the result is that one stack's data is shared with another. See docs/open-questions.md OQ-35 for the options (reject at apply, unambiguous `_` separator, forbid `-` in stack names, ownership label check) and the recommendation (separator plus label check).

Found in the review of 3v6.

## Acceptance Criteria
- [ ] Owner decides; ADR written; architecture §8, ADR-0030 and stack-format updated
- [ ] Follow-up implementation task filed (naming lives in `ServiceGeneration`/`VolumeMount::named` in src/runtime/mod.rs)

## References
- docs/open-questions.md OQ-35
- docs/architecture.md §8
- ADR-0006, ADR-0030, ADR-0046