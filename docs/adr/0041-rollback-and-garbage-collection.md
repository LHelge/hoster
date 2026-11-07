# ADR-0041: Rollback = redeploy a previous deployment; keep the last 3 successful images

- Status: Accepted (resolves OQ-08, OQ-24)
- Date: 2026-11-07

## Decision

**Rollback**

- **Starting one.** Every successful deployment in the UI history offers **"redeploy this one"**.
- **What it does.** It creates a normal zero-downtime deployment (`source = rollback`) with that
  deployment's digest set and config revision.
- **The poller doesn't undo it,** because it only reacts to tag digest changes (ADR-0043).
- **Automatic rollback** on post-deploy health degradation is a possible later addition.

**Garbage collection**

- **Containers.** Containers of old generations are removed after each deploy. The reconciler
  removes any leftovers.
- **Images.** Each service keeps its current digest and the **last 3 successful** ones, so a
  rollback works even when the registry is unreachable. Other `hoster.managed` images are pruned
  daily.
- **Volumes are never pruned automatically** (ADR-0030).

## Alternatives considered

- **Keep the old container stopped for rollback**: holds ports and containers, and gets messy
  with stop-first services.
- **Redeploy plus automatic rollback in v1**: needs continuous health checks earlier than planned.
- **Keep the current image only / no pruning**: rollback would need a re-pull, or the disk
  slowly fills.

## Consequences

- Rollback has the same cost and safety as a normal deploy.
- Disk usage is bounded at about 4 images per service.

## Correction (2026-11-07)

"Other `hoster.managed` images are pruned daily" assumed hoster could label images. It cannot:
labels on an image come from its build, and a pull adds none. hoster therefore keeps its own
record of what it pulled (an `images` table: digest, repository, pulled at) and prunes only
recorded digests that fall outside every service's retention set. Images hoster did not pull are
never removed. The decision (keep current + last 3 successful, prune daily) is unchanged.

Rollback uses the previous deployment's digests and stack revision but the **current** secret
values; old secret values are not retained (ADR-0029).
