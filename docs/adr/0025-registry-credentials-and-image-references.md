# ADR-0025: Registry credentials (classic PAT for ghcr) and image references (third-party tags resolved at apply)

- Status: Accepted (resolves OQ-06, OQ-25)
- Date: 2026-11-07

## Context

Private ghcr images need credentials, both for podman pulls and for the digest poller. Stacks
also contain third-party images, such as rustfs from Docker Hub, that are not built by our CI
and are not deployed through the webhook.

## Decision

**Credentials (OQ-06)**

- For ghcr, use a **classic personal access token with only `read:packages`**. Preferably it
  belongs to a dedicated machine account that has read access to the relevant packages.
- Credentials are stored in hoster's secret store (ADR-0029), entered in the web UI under
  **Settings → Registries**, and keyed by **registry host**: `ghcr.io`, `docker.io` and so on.
- The same credential serves podman pulls (bollard `X-Registry-Auth`) and the registry client's
  token requests.
- Whether ghcr accepts fine-grained PATs is tested during implementation. If it does, they are
  documented as the preferred option.
- ~~GitHub App installation tokens replace the PAT once the GitHub App lands.~~ See the correction
  below.

**Image references (OQ-25)**

- Any OCI registry may appear in stack files.
- **CI-built ghcr images** have no tag in the stack file. Their digest comes from the
  webhook or the poller.
- **Third-party images** may be given as `repo:tag`, `repo:tag@sha256:…` or `repo@sha256:…`.
  - A **tag without a digest is resolved to a digest when the stack is applied**.
  - The digest is stored in that stack revision and shown in the diff before the apply is
    confirmed.
  - Deploys, restarts and rollbacks always use the stored digest.
  - A "re-resolve tags" action in the UI creates a new revision with fresh digests and deploys it
    if they changed.
- The webhook and poller only deal with CI-built ghcr services.

## Alternatives considered

- **Public packages only**: rules out private apps.
- **`podman login` / `auth.json` managed over SSH**: no UI, and the poller would have to read it
  anyway.
- **Third-party images must be pinned by digest** (the earlier recommendation): fully explicit,
  but tedious for routine updates. It is still allowed, and resolve-at-apply keeps runtime
  behaviour equally deterministic.
- **ghcr-only (mirror third-party images)**: needs a mirroring workflow.

## Consequences

- The registry client must be a **generic OCI distribution client**, not ghcr-specific. It
  discovers the token endpoint from the `WWW-Authenticate` challenge. That covers ghcr (verified)
  and Docker Hub, whose token server is separate (`auth.docker.io`).
- Docker Hub's anonymous pull rate limits apply. A `docker.io` credential can be added.
- A classic PAT can read every package its account can read, so use a dedicated machine account
  for least privilege.
- Running images never change silently: a tag moving upstream has no effect until you apply or
  re-resolve.

## Correction (2026-11-07)

The original text assumed GitHub App installation tokens would replace the PAT once the deferred
GitHub App exists. **That is unverified.** No official GitHub statement was found that ghcr accepts
App installation tokens. A user-filed docs issue (github/docs#9547) reports that they are rejected
by the npm and Maven registries and suggests ghcr is unsupported too. Until a live test shows
otherwise, assume the **classic PAT is the long-term pull credential**. The registry-client task
includes that test.

Forwarding the CI job's `GITHUB_TOKEN` through the webhook to authorize pulls was also considered
and rejected:

- It expires when the job ends, possibly before the pull happens.
- It only covers the first pull: not rollbacks, re-pulls or the repo-ownership check.
- It is a write-scoped token handed to another system.

CI identity is better used to authenticate the webhook itself (OQ-29).

## Verification status (2026-11-08)

The registry client (`src/registry/client.rs`) resolves public images anonymously on both
ghcr.io and Docker Hub (token realm discovered from `WWW-Authenticate`; Docker Hub's realm is
`auth.docker.io`), checked by the gated live tests (`HOSTER_REGISTRY_LIVE_TESTS=1 cargo test live`).

The two token-kind checks this ADR asks for need private packages and real tokens, so they were
**not run during implementation** and remain open:

- **Fine-grained PAT for ghcr pulls:** untested. The classic PAT stays the documented credential.
- **GitHub App installation token (`packages: read`):** untested; still assumed unsupported, as in
  the correction above.

To run either check, set `HOSTER_REGISTRY_LIVE_TESTS=1`,
`HOSTER_REGISTRY_LIVE_IMAGE=ghcr.io/<owner>/<private-image>:<tag>` and
`HOSTER_REGISTRY_LIVE_CREDENTIAL=<username>:<token>`, then run
`cargo test live::private_image_with_credential -- --nocapture`. Record the outcome here.
