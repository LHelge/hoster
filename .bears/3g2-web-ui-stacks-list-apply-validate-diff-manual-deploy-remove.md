---
id: "3g2"
title: "Web UI: stacks list, apply (validate + diff), manual deploy, remove"
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.225800435Z"
updated: "2026-11-08T00:31:25.316896475Z"
tags:
  - web
depends_on:
  - tuv
  - "8w4"
  - pap
parent: ame
attempts: 1
---

## Summary
First useful admin pages: manage stacks and trigger deploys through the orchestrator.

## Acceptance Criteria
- [ ] Stacks list with services, current digests, active revision hash and last deploy state
- [ ] Apply stack (ADR-0027, ADR-0049): upload or paste TOML → validation errors with line numbers, or a diff vs the current revision incl. resolved third-party digests (ADR-0025) **and the list of services that will be redeployed** → confirm → ApplyStack, which applies and redeploys in one step; the page then follows the resulting deployment. There is no "apply without redeploy"
- [ ] Manual deploy form: per-service digest input → Deploy command; shows Noop/Accepted result
- [ ] Remove stack with typed confirmation (stack name) and optional volume deletion
- [ ] HTMX partial updates for forms; full pages work without JS for read-only views

## Implementation Notes
- Askama templates per page + partials; handlers talk only to the orchestrator (shape per OQ-33)

## Testing
- Handler tests with a fake orchestrator; template render tests
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- spec.md §5.4
- ADR-0021, ADR-0049