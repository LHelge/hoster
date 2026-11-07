# ADR-0045: Observability lives in the web UI only (no metrics endpoint, no alerting) in v1

- Status: Accepted (resolves OQ-26)
- Date: 2026-11-07

## Decision

- **No metrics export.** hoster keeps **in-process statistics** and shows them in the web UI. It
  does not expose a Prometheus/metrics endpoint. The statistics are:
  - per-route request counts by status class
  - recent latency (p50/p95 over a rolling window)
  - upstream errors and current in-flight requests
  - deploy outcomes and durations (from SQLite)
  - certificate expiry
  - reconcile restarts and CrashLooping state
- **No outbound alerting.** Problems are logged at `warn`/`error` and highlighted on the
  dashboard.

## Alternatives considered

- **Prometheus endpoint plus an optional notification webhook**: needs an external scraper or
  alerting setup to be useful.
- **Prometheus endpoint without alerts**: same dependency, with less value for a single operator.

## Consequences

- Nothing notifies the operator when a certificate is about to expire or a deploy fails. They
  have to look at the dashboard or the logs. This is the main gap to revisit after v1, e.g.
  with a single notification webhook.
- Statistics reset when hoster restarts, except what is persisted in SQLite (deploy history,
  restarts).
