# ADR-0030: Named podman volumes kept on stack removal; backups documented, not built in (v1)

- Status: Accepted (resolves OQ-03)
- Date: 2026-11-07

## Context

Some services need persistent data, for example rustfs. The stack format must not expose host
paths (ADR-0019). Operators need to know what to back up.

## Decision

- **Volume model.** Volumes are **named podman volumes** `hoster-<stack>-<volume>`, labelled
  `hoster.managed=true` and `hoster.stack`, and declared in the stack file as `volumes = { <name>
  = "<mount path>" }`. Host bind mounts cannot be expressed.
- **Lifecycle.** Volumes are created on demand and **kept** when a stack is removed. Deleting
  them is a separate action in the UI that asks you to type the stack name to confirm.
- **Backups in v1** are documented rather than automated:
  - `hoster backup db <file>`: a consistent SQLite snapshot via `VACUUM INTO`, safe while hoster
    runs.
  - The secret-store key file (ADR-0029), stored separately from the DB backup.
  - `acme/` and `certs/`: optional, since certs can be re-issued, but they save rate limits.
  - Volumes: `podman volume export hoster-<stack>-<vol>`. For databases, consistency is the app's
    concern; stop the service or use the app's own dump tool.
- **Later.** Built-in scheduled backups (local plus restic/S3) are a later milestone.

## Alternatives considered

- **Directories under the state dir, bind-mounted**: easier to inspect, but rootless UID mapping
  makes ownership awkward and it would open a bind-mount path in the format.
- **Built-in scheduled backups in v1**: more work, and naive exports of live databases are
  inconsistent anyway.

## Consequences

- Validation warns when a service has volumes but uses `start-first`. During a deploy, two
  instances would mount the same volume (see ADR-0039).
- `docs/backup.md` is part of the v1 docs.
