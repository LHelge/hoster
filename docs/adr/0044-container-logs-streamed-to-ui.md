# ADR-0044: Container logs stay in podman and are streamed into the web UI

- Status: Accepted (resolves OQ-04; amended by ADR-0067, which persists parsed logs in a separate database)
- Date: 2026-11-07

## Decision

- **Where logs live.** Containers log through podman's `k8s-file` driver, with a maximum log size
  per container set at create.
- **Viewing them.** The web UI shows logs per container with *tail N*, *since* and *follow*,
  read through bollard `logs` and streamed live over SSE using the HTMX SSE extension.
  stdout and stderr are distinguished. The stream closes when the browser disconnects.
- **hoster's own logs** go to stdout, which ends up in the user journal (ADR-0023).
- **Deployment event logs** are stored in SQLite and shown on each deployment's page.

## Alternatives considered

- **journald log driver**: integrates with host logging, but access to the user journal and
  querying it from the UI are more fiddly.
- **Shell only** (`podman logs`): no UI.

## Consequences

- Logs of removed containers are gone once a generation is cleaned up. Deployment event logs
  survive.
- No log shipping. Out of scope for v1.
