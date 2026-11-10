---
id: xgf
title: "M12: Config files in containers"
type: epic
status: open
priority: P2
created: "2026-11-09T17:20:07.807354984Z"
updated: "2026-11-09T23:58:38.823017608Z"
tags:
  - stack
  - runtime
  - third-party
---

## Scope

Many third-party images (Prometheus, Grafana provisioning, Caddy, nginx, Mosquitto, …) are configured by a file, not by env vars. Bind mounts are excluded on purpose (ADR-0019; owner 2026-11-09: users should not care about the host filesystem). This epic lets a stack file carry file contents itself, as ADR-0057 decides. Branch: `epic/xgf-config-files`.

```toml
[services.prometheus.files]
"/etc/prometheus/prometheus.yml" = { content = """…""" }
"/etc/app/credentials.json"      = { secret = "monitoring/creds", mode = "0440", owner = "0:65534" }
```

hoster uploads each file into the container's own layer between create and start (`PUT …/archive`), so there is no host path, podman object or label to manage. Files are not writable by a non-root container user, and read-only with `read_only = true`. Their content (or secret version) is part of the service's config hash, so a change redeploys the service and rolls back with the deployment. Stacks with `files` need podman 5.4 or later.

Tasks: nnq (format and validation), rp5 (upload and secrets), nhn (UI and docs).

## Acceptance Criteria

- [x] An ADR records the format, the size caps, the mechanism and how it relates to ADR-0019 (ADR-0057, task 5jg).
- [ ] Inline and secret-backed files reach the container at the declared path with the documented owner and mode (`content`: `0:0 0444`; `secret`: the container's user, `0400`), readable by the container's user and not writable by a non-root container user (read-only with `read_only = true`).
- [ ] Changing a file's content or its secret redeploys only that service, and stacks without `files` keep their config hash. A rollback restores the old inline content; secret files get the current value (ADR-0041 Correction).
- [ ] Missing secrets reject the deployment by name and path, never showing a value. Secret file content is redacted from exec output, each line too.
- [ ] File material goes away with its container: container removal is the GC, and nothing else needs cleaning up.
- [ ] A stack with `files` is rejected on podman older than 5.4, naming both versions. Other stacks are unaffected, and `hoster doctor` reports the podman version.
- [ ] The stack overview lists files (path, mode, owner, size or secret name) without ever showing secret content. The stack-format reference documents `files`.

## References

- ADR-0057; ADR-0019, ADR-0029, ADR-0041, ADR-0049
- `docs/architecture.md` §1.7 (archive upload spike), §5.1 (Pulling), §5.5 (redaction), §8 (Config files bullet), §9 (secrets at rest)
- `site/content/wiki/stack-format.md` §2.2
