# ADR-0038: hoster-driven health probes; no resource limits in v1

- Status: Accepted (resolves OQ-07); default probe path is `/healthz` since ADR-0051
- Date: 2026-11-07

## Decision

**Health checks**

- hoster probes containers **from the host** on their allocated `127.0.0.1` port. Two probe types:
  - `http`: path, port, expected status class
  - `tcp`: connect
- Parameters: `interval`, `timeout`, `healthy_after` (consecutive successes), `start_period`
  (abort the deploy after this).
- A deploy aborts early if the container exits during checking.
- **Defaults when not declared:**
  - Routed services: HTTP `GET /` on the first route's port, expecting 2xx–3xx.
  - Non-routed services: "running and not restarted for 5 s".
- **After a deploy**, probes run periodically (every 10–30 s). They feed UI status and in-process
  statistics only (ADR-0045).
  Restarting exited containers is the reconciler's job (ADR-0028), and failing probes alone don't
  restart anything in v1.

**Resource limits**

- **None in v1.** The stack format has no `resources` field. It can be added later, mapped to
  `Memory`, `NanoCpus` and `PidsLimit`, once cgroup delegation is checked on the target host.

## Alternatives considered

- **Image `HEALTHCHECK`**: rootless podman runs these through user-session systemd timers, and the
  image needs a binary such as curl.
- **Both, with fallback**: more behaviour to explain, for little gain.
- **Optional limits with a default pids cap / required limits**: deferred.

## Consequences

- Distroless images work without changes.
- With no limits, one app can starve the others of memory or CPU. Acceptable for a
  single-operator VPS in v1.
- Stack files that contain `resources` are rejected (unknown field) until it is added.
