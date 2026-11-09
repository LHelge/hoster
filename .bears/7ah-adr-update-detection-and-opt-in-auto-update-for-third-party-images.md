---
id: "7ah"
title: "ADR: update detection and opt-in auto-update for third-party images"
status: open
priority: P2
created: "2026-11-09T17:21:18.094672133Z"
updated: "2026-11-09T17:21:18.094672133Z"
tags:
  - decision
  - docs
  - adr
parent: "6z3"
---

## Summary
Write the ADR (next free number) for M14, amending ADR-0025 ("the webhook and poller only deal with CI-built ghcr services") and ADR-0043. Send it to review for owner sign-off. Owner decision: notify + opt-in auto-update.

## Points the ADR must settle (proposed answers)
1. **What is checked.** Every service whose image has a tag and is not CI-built: not untagged ghcr and no `track`. This includes `repo:tag@sha256:…`, where the digest is the pin and the tag names the line.
2. **Two signals.**
   - **Moved tag:** the tag now resolves to a digest other than the pinned one. This is the signal for floating tags (`16`, `8-alpine`, `latest`).
   - **Newer version:** the registry's tag list holds a higher version in the same line.
3. **Version lines.**
   - Parse a tag as optional `v` + 1–3 numeric components + optional `-suffix`.
   - Same line means same prefix style, same component count and same suffix. So `16.4-alpine` → `16.6-alpine`, but never → `17.0` or `16.6-bookworm`.
   - Skip pre-releases (`-rc`, `-beta`, `-alpha`, `-dev`, `-nightly`).
   - Tags that don't parse only get the moved-tag signal.
   - Define what "patch" and "minor" mean for 1- and 2-component tags.
4. **Policy field.** `auto_update = "off" | "digest" | "patch" | "minor"`, default `off`. The name avoids clashing with the existing `update` (strategy) field.
   - `digest`: follow the moved-tag signal.
   - `patch` / `minor`: bump the tag within those bounds.
   - `auto_update` together with `track` is an error.
5. **Grouping.** All available updates of a stack found in one check pass are applied as **one revision / one deployment**.
   - Services on the same repository family and tag (e.g. `immich-server:v2.3.1` and `immich-machine-learning:v2.3.1`) must move together. Recommend an implicit group by (registry, owner, tag). If any member's target is missing, hold the whole group.
6. **Applying.**
   - An auto-update writes a new stack revision: the tag is rewritten in the TOML text, which keeps comments, and the digest is pinned. It deploys through the normal path with a new `DeploymentSource::AutoUpdate`.
   - Edge-triggered as in ADR-0043: a target that failed is not retried until the candidate changes.
   - Rollback (ADR-0041) holds until a newer candidate appears.
7. **Rate limits and cadence.**
   - Default every 6 h with jitter, configurable as `updates.interval` in `hoster.toml`.
   - Use HEAD manifest requests. List tags only for version-tagged services, with pagination (`Link` header).
   - Back off per registry on 429. Use credentials when configured (ADR-0025).
   - Document Docker Hub's anonymous limits.
8. **Storage.** Candidates in SQLite (stack, service, current tag/digest, candidate tag/digest, kind, first seen, last checked, last error), in a reversible migration.
9. **Interaction with self-update** (refused while deploying, ADR-0052) and the deploy queue (ADR-0040).

## Acceptance Criteria
- [ ] ADR written with alternatives (notify only, auto by default, image-specific feeds) and consequences. Amended ADRs' status lines updated. Index row added.
- [ ] `docs/architecture.md` component overview (§2) and threads (§3) mention the update checker.
- [ ] Task sent to review for owner sign-off.

## References
- ADR-0025, ADR-0040, ADR-0041, ADR-0043, ADR-0049, ADR-0052
- `docs/architecture.md` §2, §3, §5.2
- `src/registry/poller.rs`, `src/registry/client.rs`, `src/orchestrator/mod.rs` (`reresolve_preview`, `reresolve`)