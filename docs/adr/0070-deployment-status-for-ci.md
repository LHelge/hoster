# ADR-0070: CI reads a deployment's outcome with its OIDC token, which a read does not consume

- Status: Accepted (resolves OQ-45; amends ADR-0042 and ADR-0047)
- Date: 2026-11-14

## Context

The deploy action (`actions/deploy/`) posts digests to `/hooks/deploy` and succeeds on hoster's
`202 {"result":"accepted","deployment_id":…,"state":"Received"}`. A green CI run therefore means
only that hoster accepted the request. A rollout that fails its health checks, and so keeps the
old containers, still looks green in GitHub (user feedback, 2026-11-14).

A deployment's state is shown only in the admin UI (`/stacks/{name}/deployments/{id}/status` and
its SSE stream), behind the admin session. CI has no session. It authenticates only with a GitHub
Actions OIDC token against the stack's `[deploy]` rule (ADR-0047), and that rule makes each
token's `jti` single use, so a polling client cannot simply resend the token it deployed with.

## Decision

- **Endpoint.** `GET https://<admin-host>/hooks/deploy/{stack}/deployments/{id}` with
  `Authorization: Bearer <GitHub Actions OIDC ID token>`, next to the webhook and, like it,
  outside the admin session guard.
- **Token verification** is ADR-0047's: RS256 against GitHub's JWKS, `iss`, `aud`, and `exp`,
  `nbf` and `iat` with 60 s of skew. The **single-use rule does not apply to reads**: a read
  neither checks nor records the `jti`. A read changes nothing, and the rule exists to stop a
  replayed *deploy*. The action may reuse the token it deployed with until it expires.
- **Authorisation.**
  - The token must pass `{stack}`'s `[deploy]` rule exactly as a deploy would (`repository_id`,
    and `refs`, `environment` and `job_workflow_ref` where set).
  - The deployment must be one of `{stack}`'s and must have been started by the webhook from
    the **same workflow run**: its recorded provenance (ADR-0047) has the token's `repository`
    and `run_id`. A job can read the deployments its own run started, and nothing else: not UI
    deploys, poller or auto-update deployments, nor another run's.
- **Answers.**
  - `200` with JSON:

    ```json
    {"deployment_id":"30","stack":"notes","state":"Failed","terminal":true,
     "reason":"api: health check failed after 30s: GET /healthz answered 503",
     "created_at":"2026-11-14T09:12:03Z","finished_at":"2026-11-14T09:13:41Z",
     "url":"https://hoster.example.com/stacks/notes/deployments/30"}
    ```

    `state` is spelled as in the webhook's `202`; `terminal` is `DeploymentState::is_terminal`;
    `reason` is the failure or rejection reason hoster recorded, or `null`; `finished_at` is
    `null` until the deployment is terminal.
  - `401` without detail for a bad or expired token, as the webhook answers.
  - `403` for a token that fails the stack's rule, a stack without `[deploy]` and an unknown
    stack, with the webhook's body, so stacks cannot be probed. Unlike a refused deploy, a
    refused read is **not** listed on the stack page: the deploy from the same run already was.
  - `404` with one body (`{"error":"no such deployment"}`) for an unknown or malformed id,
    another stack's deployment and a deployment the run did not start, so ids cannot be
    probed.
- **The deploy action** gains `wait` (default `false`) and `timeout` (default `10m`). With
  `wait: true` and an `accepted` result it polls every 5 s until the deployment is terminal:
  - `Live` → exit 0; `LiveWithWarnings` → exit 0 with a warning annotation;
  - `Failed`, `Rejected` or `Superseded` → non-zero, printing the state and the reason;
  - the timeout → non-zero, naming the last state seen. The deployment keeps running.

  It prints the deployment URL and writes the outcome to the job summary either way, and exposes
  a `state` output. A `401` while polling mints a new token; transport errors, `502` and `503`
  are retried. `noop` needs no wait. Without `wait`, the action behaves as before.

## Alternatives considered

- **A fresh OIDC token per poll, each `jti` consumed.** Nothing new to reason about, but one
  token request and one seen-`jti` row every 5 s for nothing the single-use rule protects.
- **A read capability in the `202`** (a random, short-lived token or URL for that deployment).
  No OIDC on the read path, but a new kind of bearer credential to mint, store hashed and
  expire, against ADR-0047's "no deploy secret anywhere".
- **Scoping reads by the `[deploy]` rule alone**, so any run allowed to deploy the stack could
  read all of its deployments. Harmless for most stacks, but wider than the action needs.
  Scoping by `repository_id` instead would need it stored in the provenance; `run_id` is
  already there and is the tighter scope.
- **The SSE stream for CI.** Fewer requests, but a streaming client in bash is fragile, and a
  poll every 5 s costs hoster nothing.
- **Waiting by default.** It would change every existing workflow's run time and failure
  behaviour on upgrade.

## Consequences

- A CI run can fail when its rollout fails, without opening the admin UI.
- A token leaked from a job log could read that run's deployment states until it expires, and
  nothing else. It still cannot deploy twice.
- The status endpoint is a second unauthenticated-by-session route on the admin host, so
  architecture §9 lists it with the webhook.
- The endpoint is part of the webhook contract that the action's release tag names (epic `8kz`):
  an action release with `wait` needs a server at least that new, and the deploying-from-CI guide
  says so.
