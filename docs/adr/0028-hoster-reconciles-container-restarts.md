# ADR-0028: hoster restarts containers itself (reconciliation); podman restart policy is off

- Status: Accepted (resolves OQ-17)
- Date: 2026-11-07

## Context

Rootless podman containers don't come back after a reboot unless something starts them, and a
crashed container stays exited.

## Decision

- Containers are created with podman restart policy **`no`**.
- **At startup** the reconciler starts every container of each stack's current generation
  (architecture §4 step 5).
- **While running**, a reconcile loop (about every 15 s) restarts exited containers of the
  current generation with exponential backoff from 1 s to 5 min. A service is marked
  **CrashLooping** after 5 restarts within 10 minutes and keeps retrying at the maximum backoff.
- Restarts are recorded as events and shown in the UI (in-process statistics, ADR-0045; there is
  no metrics endpoint).

## Alternatives considered

- **Podman `restart=always` + `podman-restart.service`**: keeps working while hoster is down, but
  races hoster's deploys and cleanup, e.g. by restarting a generation that is being removed.
- **Quadlet units per container**: strong systemd supervision, but it would make two
  orchestrators and churn unit files on every deploy.

## Consequences

- One source of truth for what should run.
- Nothing restarts containers while hoster is down. This is acceptable because systemd restarts
  hoster (`Restart=always`, ADR-0023) and hoster starts at boot through linger.
- The reconcile loop must skip stacks with an active deployment, which owns those containers
  until it finishes.
