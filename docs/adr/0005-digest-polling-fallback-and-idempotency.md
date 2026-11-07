# ADR-0005: ghcr digest polling as fallback; both paths idempotent by digest

- Status: Accepted
- Date: 2026-11-07

## Context

Webhooks can be lost (CI step failure, hoster down during the release, network). We need eventual
convergence without manual action, and we must not double-deploy when both mechanisms fire.

## Decision

- A background poller periodically resolves tracked ghcr tags to digests using the registry API
  (`HEAD /v2/<repo>/manifests/<tag>`, `Docker-Content-Digest`; verified live) and issues a deploy
  when the digest changes.
- Every deploy request—webhook, poller, manual—is compared against the digest set currently
  running for the stack. If identical, it is a no-op.
- A GitHub App integration is deferred.

## Alternatives considered

- **Webhook only**: lost notifications leave stale deploys until noticed. Rejected.
- **Poller only**: slower, and registry polling of many repos at short intervals risks rate
  limits. Rejected as primary.
- **Idempotency by delivery ID** (dedupe webhook IDs): doesn't cover poller/webhook overlap, and
  requires storing delivery IDs. Digest equality is the natural key.

## Consequences

- The running digest per service must be durable and authoritative (state store, ADR-0022; container
  label `hoster.digest`).
- Multi-arch: compare the *requested* (index) digest, not podman's per-platform image ID.
- Semantics of "which tag does the poller track, and what if the webhook deployed a digest the tag
  doesn't point at" are defined in ADR-0043.
