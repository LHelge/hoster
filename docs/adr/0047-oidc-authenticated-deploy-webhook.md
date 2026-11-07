# ADR-0047: The deploy webhook is authenticated with GitHub Actions OIDC tokens only

- Status: Accepted (resolves OQ-29; supersedes the authentication parts of ADR-0004 and ADR-0042)
- Date: 2026-11-07

## Context

ADR-0004 and ADR-0042 authenticated deploy webhooks with HMAC-SHA256 and a per-stack shared
secret. That design has three weaknesses:

- The secret is long-lived and has to be stored in CI.
- A captured request can be replayed within ±5 min.
- It cannot express *which* workflow or ref may deploy.

GitHub Actions can mint a short-lived OIDC ID token for the running job, signed by GitHub, with
claims identifying the repository, ref and workflow. The discovery document was verified live on
2026-11-07:

- Issuer `https://token.actions.githubusercontent.com`
- Keys published at `/.well-known/jwks`
- RS256 signatures

## Decision

- **Request.** `POST https://<admin-host>/hooks/deploy` with
  `Authorization: Bearer <GitHub Actions OIDC ID token>`. There is **no HMAC path and no
  per-stack webhook secret**.
- **Token verification** happens before the body is parsed:
  - **Signature:** RS256, checked against GitHub's JWKS. The key set is fetched over HTTPS,
    cached, refreshed when an unknown `kid` appears, and refreshed at most once a minute.
  - **Issuer:** `iss` must equal `https://token.actions.githubusercontent.com`.
  - **Audience:** `aud` must equal the configured audience, by default `https://<admin-host>`
    (ADR-0024).
  - **Lifetime:** `exp`, `nbf` and `iat` are checked with 60 s of clock-skew allowance.
  - **Single use:** a `jti` that has been seen before is rejected. Seen `jti`s are stored in
    SQLite until their `exp`.
- **Authorization** comes from the stack file's `[deploy]` section (stack-format.md §2.5):
  - `repository_id` is **required**. It is stable across repository renames.
  - `refs` (glob patterns on `ref`), `environment` and `job_workflow_ref` are optional
    restrictions.
  - A stack without `[deploy]` cannot be deployed through the webhook. UI deploys and rollbacks
    still work.
- **Payload** (body, at most 64 KiB):

  ```json
  {"stack":"notes","images":{"api":"sha256:…"},"tag":"v1.4.2"}
  ```

  - `timestamp` is no longer needed.
  - Provenance in the deploy history (repo, sha, run id, actor) is taken from the **verified token
    claims**, not from the body.
- **Unchanged from ADR-0042:**
  - several services per payload deploy as one deployment
  - unknown services are rejected
  - each digest must belong to its service's configured repo
  - `stack_file` is reserved
  - the response codes
- **Help when setting up.** Rejected attempts are listed on the stack page with their claims
  (`repository`, `repository_id`, `ref`, `environment`, `workflow_ref`), so the admin can copy the
  right `repository_id` into the stack file.

## Alternatives considered

- **Keep HMAC (ADR-0042)**: it works from anywhere, but needs long-lived shared secrets and leaves
  a replay window.
- **OIDC plus an opt-in HMAC fallback**: two auth paths to build and secure, and a stack with the
  fallback is only as strong as HMAC.
- **Forwarding `GITHUB_TOKEN` for pulls**: rejected in the ADR-0025 correction. OIDC tokens don't
  authorize ghcr pulls either, so pulls stay on the PAT.

## Consequences

- No deploy secret exists anywhere: nothing to generate, store in CI, rotate or leak. The
  webhook-secret UI is dropped.
- **Replays are impossible**, because `jti` is single-use and tokens expire within minutes.
- **Deploys can only come from GitHub Actions.** Manual deploys go through the UI (deploy by
  digest, or rollback).
- **Who can deploy is configurable.** GitHub environments with protection rules (required
  reviewers, tag-only deployments) can gate production deploys, and hoster enforces that through
  `environment`.
- **The release job needs `permissions: id-token: write`.**
- **hoster depends on GitHub's JWKS endpoint** for verification. A cached key set covers short
  outages, and a JWKS fetch failure rejects the deploy, never accepts it.
- **The token authenticates the caller, not the body.** A stolen token could be sent with a
  different body, but only once and only before it expires, and only within the stack's
  `[deploy]` rule and repo-ownership checks.
- **The clock must still be NTP-synced** for `exp`/`nbf`, so `hoster doctor` keeps that check.
