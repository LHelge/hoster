# ADR-0049: Applying a stack revision or changing a secret redeploys the affected services immediately

- Status: Accepted (resolves OQ-32; refines the config-change part of ADR-0029)
- Date: 2026-11-07

## Context

ADR-0029 made the idempotency key (image digest, config hash) and offered "apply & redeploy" as
an option next to a plain "apply", promising that "nothing redeploys silently in the background".
Those two statements conflict: after an apply without redeploy, a webhook carrying the digest that
is already running has a different config hash, is therefore not a no-op, and deploys the pending
revision. Either the promise or the staged apply had to go.

## Decision

- **There is no "apply without redeploy".** Applying a stack revision validates it, shows the diff
  and the list of services whose config hash changes, and on confirmation applies the revision
  **and** redeploys exactly those services, in one deployment with `source = apply`.
- **Saving, replacing or deleting a secret** works the same way: the UI lists the services that
  reference it and, on confirmation, redeploys them.
- **"Re-resolve tags"** (ADR-0025) is an apply like any other.
- **Invariant:** outside a running or failed deployment, the config every service runs with is the
  active revision. The idempotency key (digest, config hash) stays as in ADR-0029 and is now always
  consistent with it: a webhook with an unchanged digest is a true no-op.
- **If the redeploy fails,** the revision is still the active one (it is the desired state). The
  stack page shows "revision N: deploy failed" with the reason, and the next deploy of the stack,
  from any source, uses the revision. This is the only path by which a later webhook can carry a
  config change, and it is visible rather than silent.

## Alternatives considered

- **Apply makes the revision active; the next deploy of any kind picks it up.** Consistent with the
  key, but a webhook that only meant "new image" would also ship a staged config change, which is
  the surprise ADR-0029 tried to avoid.
- **Two revision pointers (applied vs. deployed), webhooks pinned to the deployed one.** Keeps the
  promise literally, but webhooks would start containers from a revision the admin had already
  replaced, and every deploy would carry a revision id.

## Consequences

- Staging a config change for a later release is not possible. To change config and image
  together, apply the revision first (short redeploy), then release.
- One fewer UI state and no "pending redeploy" indicator; the stack page shows the active revision
  hash and the last deployment outcome.
- The confirmation step remains: nothing redeploys without the admin seeing which services move.
- `spec.md` §6 reads: "Config changes redeploy the affected services when you confirm the apply."
