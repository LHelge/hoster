---
id: qq9
title: "M11: Service dependencies and readiness"
type: epic
status: done
priority: P1
created: "2026-11-09T17:20:07.797247925Z"
updated: "2026-11-09T22:27:24.607934695Z"
tags:
  - stack
  - orchestrator
  - health
  - third-party
---

## Scope

Make multi-container stacks of images we didn't write (an app plus its database, cache and workers) start reliably. Branch: `epic/<id>-service-dependencies`.

- `depends_on = ["db", "cache"]` per service: a service is started only after the services it depends on are healthy. Deploys and the reconciler's restarts follow dependency order.
- `health.exec = ["pg_isready", "-U", "postgres"]`: a command run inside the container through the podman exec API. Exit code 0 means healthy. Allowed on routed and non-routed services.
- Every container is created with `Healthcheck {Test: ["NONE"]}`, so podman never runs image checks on its own timers. The defaults stay as they are (`GET /healthz` for routed services, "running for 5 s" for the rest).

Owner decisions (2026-11-09): exec checks only. The image HEALTHCHECK default was dropped after the ADR spike: podman drops HEALTHCHECK from OCI images, so it would need a registry config fetch per digest (see ADR-0056, Alternatives). A start-phase dependent of a changing stop-first service is held back until that service passes. The reconciler's wait is bounded by the dependency's `start_period`. Compose support is deferred.

## Acceptance Criteria

- [ ] An ADR records the semantics and amends ADR-0038 / ADR-0048 / ADR-0051 where they change.
- [ ] A stack of app + postgres + redis with no ordering luck in service names deploys first time, every time, using exec health checks.
- [ ] Cycles, self-dependencies and unknown services are rejected at apply with spans.
- [ ] After a host reboot, the reconciler brings services up in dependency order.
- [ ] The stack overview and deployment events show dependencies and exec probe results.
- [ ] The stack-format reference documents every new field.

## References

- ADR-0038, ADR-0048, ADR-0051, ADR-0028
- `docs/architecture.md` §5 (deploy state machine)
- `site/content/wiki/stack-format.md` §2.2, §2.4