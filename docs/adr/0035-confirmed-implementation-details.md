# ADR-0035: Confirmed implementation details from architecture.md

- Status: Accepted (resolves OQ-28); "orchestrator actor" in #1 reads "orchestrator" since ADR-0050
- Date: 2026-11-07

## Decision

The owner confirmed these details from architecture.md as decided:

1. The **orchestrator actor** performs the RouteTable swap as the single writer. Deploy tasks
   don't.
2. A **1 s settle delay** after a swap before drain checks start.
3. **308** (not 301) for HTTP → HTTPS redirects.
4. **One certificate per hostname**: a single SAN, with no bundling per stack.
5. **ACME backoff** from 5 min to 24 h per host. Renew at **1/3 of the lifetime remaining**, or in
   the ARI window when the CA provides one.
6. Client-supplied `X-Forwarded-Proto`, `X-Forwarded-Host` and `X-Forwarded-Prefix` are
   **overwritten**. `X-Forwarded-For` is **appended** to.
7. **No upstream retries** once request body streaming has started.
8. **Naming:** network `hoster-<stack>`, containers `hoster-<stack>-<svc>-g<gen>`, plus the
   `hoster.*` label set in architecture §8.
9. **No request body size limit** by default. An optional per-route `max_body` may come later,
   enforced by counting bytes while streaming, not by buffering.
10. A **single binary crate** with modules (architecture §10). It is split into a workspace only
    if compile times demand it.

## Consequences

These items are no longer marked provisional anywhere in the docs.
