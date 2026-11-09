---
id: xgf
title: "M12: Config files mounted into containers"
type: epic
status: open
priority: P2
created: "2026-11-09T17:20:07.807354984Z"
updated: "2026-11-09T17:20:07.807354984Z"
tags:
  - stack
  - runtime
  - third-party
---

## Scope

Many third-party images (Prometheus, Grafana provisioning, Caddy, nginx, Mosquitto, …) are configured by a file, not by env vars. Bind mounts are excluded on purpose (ADR-0019; owner 2026-11-09: users should not care about the host filesystem). This epic lets a stack file carry file contents itself. Branch: `epic/<id>-config-files`.

```toml
[services.prometheus.files]
"/etc/prometheus/prometheus.yml" = { content = """…""" }
"/etc/app/credentials.json"      = { secret = "monitoring/creds" }
```

Files are mounted read-only. Their content (or secret version) is part of the service's config hash, so a change redeploys the service and rolls back with the deployment.

## Acceptance Criteria

- [ ] An ADR records the format, the size cap, the mount mechanism and how it relates to ADR-0019.
- [ ] Inline and secret-backed files reach the container at the declared path, read-only, readable by the container's user.
- [ ] Changing a file's content or its secret redeploys only that service. A rollback restores the old content.
- [ ] Missing secrets reject the deployment by name, never showing a value.
- [ ] File material of retired generations is garbage-collected.
- [ ] The stack overview lists files (path, size, source) without ever showing secret content. The stack-format reference documents `files`.

## References

- ADR-0019, ADR-0029, ADR-0041, ADR-0049
- `docs/architecture.md` §8 (podman conventions), §9 (security boundaries)
- `site/content/wiki/stack-format.md` §2.2