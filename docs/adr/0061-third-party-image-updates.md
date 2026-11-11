# ADR-0061: Third-party image updates are detected in the background, shown in the UI and applied automatically only on a per-service `auto_update` opt-in

- Status: Accepted (amends ADR-0025 and ADR-0043)
- Date: 2026-11-11

## Context

A third-party image's tag (`postgres:16`, `valkey:8`, `ghcr.io/immich-app/immich-server:v2.3.1`)
is resolved to a digest at apply and never changes until the operator edits the stack file or
presses "Re-resolve tags" (ADR-0025). The webhook and the digest poller only deal with CI-built
ghcr services (ADR-0025, ADR-0043). Nothing tells the operator that `postgres:16` has moved to a
patched build or that immich has shipped `v2.3.4`, so third-party services drift out of date.

The owner's direction (2026-11-09) is **notify, plus an opt-in auto-update**: by default nothing
deploys by itself. The owner's answers on the details below are from 2026-11-11.

## Decision

**1. What is checked.** Every service whose image carries a tag and is not CI-built. That
excludes untagged ghcr images (their digest comes from the webhook or poller) and services with
`track`. It includes `repo:tag@sha256:…`, where the digest is the pin and the tag names the line.
`repo@sha256:…` with no tag is not checked: there is nothing to compare against.

**2. Two signals.**

- **Moved tag:** the tag now resolves to a digest other than the one the active revision runs.
  This is how floating tags (`16`, `8-alpine`, `latest`) get their patch releases.
- **Newer version:** the registry's tag list holds a higher version in the same line (§3).

**3. Version lines.**

- A tag is a version if it is an optional `v`, then 1–3 dot-separated numeric components, then
  an optional `-suffix`. Tags that don't parse (`latest`, `bookworm`, `RELEASE.2025-…`) get only
  the moved-tag signal.
- Two tags are in the **same line** when they share the `v` prefix style, the component count
  and the suffix exactly. `16.4-alpine` → `16.6-alpine` is in the line; `17.0`, `16.6-bookworm`
  and `16.4.1-alpine` are not.
- Pre-releases are skipped. A suffix containing `rc`, `beta`, `alpha`, `dev`, `nightly`,
  `preview` or `snapshot` (case-insensitive) is never a candidate.
- **Bump classes are positional**: the first component is major, the second minor and the third
  patch.
  - `16.4.2` → `16.4.3` is a patch, `16.5.0` is a minor and `17.0.0` is a major.
  - `16.4` → `16.5` is a minor. Patches to `16.4` arrive as moved-tag signals.
  - `16` → `17` is a major. Everything below it arrives as moved-tag signals.
- For each service the checker keeps at most two version candidates: the highest within its
  major (patch or minor), and the highest newer major. **Majors are shown, marked as major, and
  are never applied automatically.**

**4. Policy.** A per-service field `auto_update = "off" | "digest" | "patch" | "minor"`, default
`off`. The name avoids a clash with the existing `update` (strategy) field (ADR-0039).

- `digest` follows the moved-tag signal.
- `patch` adds version bumps within the same major.minor. On a 2-component tag that is the same
  as `digest`, and on a 1-component tag too.
- `minor` adds bumps within the same major.
- It is set per service only; there is no stack-level default.
- It is not part of the config hash (ADR-0029): changing the policy doesn't redeploy.
- It is an error together with `track`, on a CI-built ghcr image, or on a digest-only image.
  `patch` or `minor` on a tag that doesn't parse as a version is also an error. Those tags can
  only use `digest`.

**5. Groups move together.** Within a stack, services whose images share registry, namespace and
tag form an implicit **update group**, e.g. `immich-app/immich-server:v2.3.1` and
`immich-app/immich-machine-learning:v2.3.1`. There is no field to declare or break groups.

- A version bump of a group is a candidate only when **every** member's repository has the target
  tag. Otherwise the whole group waits. A false grouping costs at most a delay.
- A group auto-updates under the **strictest** policy among its members. If one member is `off`,
  the group is shown as an available update but not applied.
- Moved-tag signals of group members found in the same pass are applied together. Members whose
  tag did not move stay as they are.

**6. Detection runs in the background.** An **update checker** task on the orchestrator runtime,
separate from the digest poller:

- **Cadence:** every 6 hours by default, with ±10 % jitter. The time of the last pass is
  persisted, so a restart doesn't trigger a full pass. Settings → Updates has a **Check now**
  button.
- **Requests:** a `HEAD` manifest request per checked tag through the existing registry client
  (ADR-0025), with configured credentials. Tag lists (`/v2/<name>/tags/list`, following the
  `Link` header for pagination) only for services whose tag parses as a version, once per
  repository per pass.
- **Backoff:** on 429 or errors, the checker backs off per registry host, honouring
  `Retry-After`, and records the error on the affected candidates. The rest of the pass
  continues.
- **Docker Hub:** its anonymous rate limits are documented in the guide, with the current numbers
  checked at implementation, together with the advice to add a `docker.io` credential.

**7. Settings in SQLite.** The interval lives in a typed single-row `update_settings` table
edited in Settings → Updates (ADR-0058 §1), not in `hoster.toml`. The interval is bounded to
between 1 hour and 7 days. A minimum keeps the checker polite to registries.

**8. Storage.** Two tables (reversible migration, ADR-0052). `image_checks` has a row per (stack,
service): the image, tag and digest last checked, when, and the last error, so a failed check is
recorded even before anything was found. `image_updates` has a row per (stack, service, kind)
under it: the candidate tag (and digest, for a moved tag), kind (`digest`, `patch`, `minor`,
`major`) and first seen. The record of the candidate last acted on (§10) belongs to auto-update.
Rows go when the service or its stack goes, and a check made for another tag or digest replaces
the candidates found for the old one. Newly found candidates are recorded in the event log
(ADR-0055) at info level.

**9. Showing and applying an update.**

- "Update available" appears on the stack overview, the stacks list and the dashboard warnings.
  Majors appear separately, marked as major.
- **One click** (one service, or all non-major candidates of a stack) opens the normal apply
  preview with the rewritten stack file and its diff (ADR-0049). Nothing deploys before the
  operator confirms.
- **Rewriting** keeps the operator's style. `repo:tag` becomes `repo:newtag`, and its digest is
  resolved and stored in the revision as with any apply. `repo:tag@sha256:…` gets both rewritten.
  The edit is made with `toml_edit`, so comments and formatting are kept. A moved-tag update of a
  plain `repo:tag` changes no text, only the stored digests, as "Re-resolve tags" does.

**10. Auto-update.**

- **One deployment per stack:** all candidates that a stack's policies allow, found in one pass,
  become **one** new revision and **one** deployment with the new source
  `DeploymentSource::AutoUpdate`.
- The revision is built on the active revision and stored with the same expected-revision check
  as "Re-resolve tags", so an apply that lands meanwhile is never reverted.
- **Busy stacks are skipped:** if the stack has an active or a queued deployment (ADR-0040), the
  checker leaves it for the next pass. An auto-update never takes the pending slot from an
  operator's or a webhook's target.
- **Edge-triggered, as in ADR-0043:** each candidate is acted on at most once. A failed
  auto-update (validation, pull, health check) is not retried for the same target until a newer
  candidate appears. The same rule keeps a rollback (ADR-0041) or a manual downgrade in place.
- **Mail:** a failed auto-update mails as any failed deployment does (ADR-0058). Available
  updates and successful auto-updates are not mailed, beyond the existing "also notify on
  successes" setting.
- **Self-update** (ADR-0052) is refused while an auto-update deploys, as for any deployment.

## Alternatives considered

- **Notify only**: safe, but floating tags such as `postgres:16` never pick up security patches
  unless someone clicks.
- **Auto-update by default**: a third-party release would roll out on every hoster without the
  operator asking for it, which breaks the rule that running images never change silently
  (ADR-0025).
- **Image-specific feeds** (GitHub releases, Docker Hub webhooks, vendor APIs): more precise for
  some images, but app-specific (ADR-0018). The registry API covers every image.
- **External tooling (Renovate-style PRs)**: stack files are applied through the UI and often
  don't live in a repository (ADR-0027).
- **Extending the digest poller**: the poller runs every 5 minutes for CI images and deploys by
  digest without a revision. Third-party updates rewrite the stack file and run far less often.
- **Hiding majors**: quieter, but a stack can sit on an end-of-life major with no hint.
- **The last component as patch**: makes `16` → `17` a "patch" and would auto-apply postgres
  majors.
- **Full semver parsing**: most image tags aren't semver (2 components, `-alpine` suffixes, `v`
  prefixes).
- **An explicit `update_group` field**, alone or as an override: precise, but forgetting it splits
  a release, and the override is extra surface for a rare case.
- **A stack-level `auto_update` default**: less typing, but it adds a defaults mechanism that the
  stack format doesn't have anywhere else.
- **The interval in `hoster.toml`**: needs a restart to change, against ADR-0058's split of
  run-time settings.
- **Mailing every new candidate, or a daily digest**: noise for routine patches, and the digest
  needs a new scheduled kind of mail.
- **Queueing auto-updates like any deploy**: newest-wins (ADR-0040) could supersede an operator's
  pending apply, or supersede the auto-update itself and count it as acted on.
- **Always writing the digest into the text**: the file becomes self-describing, but the first
  auto-update changes the operator's chosen style.

## Consequences

- ADR-0025's rule that "the webhook and poller only deal with CI-built ghcr services" now has a
  third path: the update checker, which handles third-party images only. "Running images never
  change silently" holds for every service with `auto_update = "off"`, which is the default.
- ADR-0043's edge-triggered rule is reused for third-party candidates, including the way a
  rollback holds.
- The stack format gains `auto_update`, and the registry client gains tag listing with
  pagination.
- An auto-updated revision is an ordinary revision. Its diff and source are in the history, and
  rolling it back works as for any deployment.
- A wrong implicit group delays an update but never splits a release.
- Third-party updates arrive within about one interval (6 hours by default). Registries see one
  `HEAD` per checked tag and one tag list per versioned repository per pass.
- The checker sees only what the registry shows. A moved tag can be a rebuilt image with the same
  version, and a version bump can break the app. The guide documents both risks, and that `minor`
  is a choice to trust the image's versioning.
