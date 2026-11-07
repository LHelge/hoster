# ADR-0004: Deploys are triggered by an HMAC-signed webhook carrying the image digest

- Status: Accepted; authentication (HMAC) superseded by ADR-0047 (GitHub Actions OIDC)
- Date: 2026-11-07

## Context

We want deploys to start within seconds of a release, initiated by the release workflow that just
pushed the image, without giving CI shell access to the server.

## Decision

The release workflow POSTs a JSON payload to hoster, signed with HMAC-SHA256 over the raw body using
a shared secret. The payload names the stack/service(s) and carries the **image digest**
(`sha256:…`), not only a tag. hoster verifies the signature in constant time before parsing.

## Alternatives considered

- **Tag-only payload** ("deploy `:latest`"): racy (tag can move between notify and pull), not
  idempotent, and ambiguous for multi-arch. Rejected.
- **SSH from CI** to run a deploy command: grants CI a shell on production. Rejected.
- **GitHub App / repository webhooks** (`registry_package`/`release` events): richer and no shared
  secret in CI, but more setup (app registration, installation tokens, event filtering).
  **Deferred**, not rejected.
- **Pull-only (poller as primary)**: adds latency and registry load; kept as fallback (ADR-0005).

## Consequences

- One secret per trust scope must be distributed to CI (per-stack secrets, ADR-0042).
- An authentic but old payload could be replayed; because the digest is explicit, a replay could
  roll a stack back. ADR-0042 limits replays to a ±5 min timestamp window.
- The webhook endpoint must be reachable from GitHub's runners, i.e. publicly (ADR-0042).
