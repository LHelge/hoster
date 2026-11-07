# ADR-0043: The digest poller deploys only when a tracked tag's digest changes

- Status: Accepted (resolves OQ-13); first sighting of a never-run service deploys since ADR-0051
- Date: 2026-11-07

## Decision

- **What is polled.** Only services whose stack file sets `track = "<tag>"`, and which are
  CI-built ghcr images.
- **How.** Every 5 minutes with jitter, the poller resolves each tracked tag to a digest
  (registry client, ADR-0025). On `429` or errors it backs off per image.
- **Edge-triggered.** The **last observed digest** for each (repo, tag) is stored in SQLite. A
  deploy is issued (`source = poller`) only when the observed digest **changes** and differs from
  the running one.
- **First sighting.** When a tag is seen for the first time, its digest is recorded without
  deploying.

## Alternatives considered

- **Deploy whenever the digest differs from the running one**: undoes rollbacks made in the UI
  (ADR-0041) and fights webhook deploys of other tags.
- **Alert only**: defeats the purpose of having a fallback.

## Consequences

- A missed webhook is caught within about 5 minutes, when the tag moves.
- A rollback holds until the tracked tag moves again, i.e. until the next release.
- `track` is now a settled part of the stack format.
