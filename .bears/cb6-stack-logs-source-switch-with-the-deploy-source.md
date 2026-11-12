---
id: cb6
title: "Stack logs: source switch with the deploy source"
status: open
priority: P3
created: "2026-11-12T13:26:42.608608506Z"
updated: "2026-11-12T13:26:42.608608506Z"
tags:
  - web
  - observability
depends_on:
  - yn7
parent: psq
---

## Summary
Add the **log source** switch of `docs/design/StackLogs.dc.html` (containers, access log, deploy) to the stack logs tab and implement the **deploy** source: the stack's deployment transitions and output over the chosen time range, read from what `hoster.db` already holds. The access-log source is its own task because it waits for the edge request log epic.

## Acceptance Criteria
- [ ] A source switch above the toolbar, `?source=containers` (the default) or `deploy`; the access-log entry is absent until its task lands
- [ ] `source=deploy` lists the stack's deployment events (`deployment_events`) in the time range, in time order across deployments: time, deployment (linked to its page), the transition or step, and its message or failure reason
- [ ] The time range and paging work as for containers. The service chips, level filter and field filters are hidden for this source; grep stays and matches the message, if the grep task has landed
- [ ] Deployment history older than the log store's 7 days is still listed: this source is not bound by `logs.db`'s retention, and the page does not show the "data starts at" notice for it
- [ ] Follow appends new deployment events while a deployment runs, if the follow task has landed (the deployment page's live progress shows how they are broadcast today)
- [ ] Switching source keeps the time range and drops parameters the new source does not have

## Implementation Notes
- Model the source as an enum with `FromStr`/`Display` for the parameter, and one view type per source behind a common row shape; do not fold deployment events into the log store
- Deployment events are read through the existing store methods used by `src/web/stacks/deployments.rs`; add a "by stack over a time span" query on the store if only "by deployment" exists (index check: `deployment_events_by_deployment` is `(deployment_id, id)`, so go through the stack's deployments in the range rather than scanning the table)
- Reuse the wording of the deployments tab for transitions, so the two pages read the same

## Edge Cases
- A deployment that started before the range and is still running shows its events inside the range only
- A stack with no deployments in the range: empty state linking the Deployments tab
- An unknown `source` value falls back to containers

## Testing
- Handler tests with seeded deployments: order across two deployments, range limits, hidden filters, links
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0044 (deployment event logs), ADR-0067, ADR-0054
- `docs/design/StackLogs.dc.html`, `src/web/stacks/logs.rs`, `src/web/stacks/deployments.rs`, `migrations/0001_initial.up.sql`