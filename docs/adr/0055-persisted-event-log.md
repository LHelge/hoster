# ADR-0055: A persisted event log of deploys, containers, certificates and operator actions, kept 30 days and written explicitly by each producer

- Status: Accepted (amends ADR-0045; makes ADR-0028's "restarts are recorded as events" concrete)
- Date: 2026-11-09

## Context

The design's dashboard has a **Recent events** feed (`docs/design/Main.dc.html`). hoster has no
queryable history except deployment transitions (`deployment_events`, the crash-recovery log of
one deployment). Container restarts and crash loops live only in the reconciler's memory,
certificate outcomes only in the journal, and operator actions nowhere. ADR-0028 already says
restarts are "recorded as events", but nothing records them. The notifications follow-up (ADR-0045's
"main gap") needs the same kind of history to filter.

`UiEvent` is a broadcast channel with room for 256 messages. A slow reader lags and loses
messages, which is fine for refreshing pages but not for a durable record.

## Decision

**1. One `events` table in SQLite.** Columns: `id`, `at`, `kind` (a typed enum, stored as text),
`severity` (`info` / `success` / `warning` / `error`, the UI's tone dot), `stack` (nullable, **no
foreign key**, so a stack's history survives its removal), `subject` (service, host or
deployment the event links to, nullable), `message` (the readable line) and `meta` (the short
mono line: sha, digest, exit code, version). Indexed by `at` and by `(stack, at)`.

**2. Event kinds in v1:**

- **Deploys:** started, succeeded, failed, rolled back. Each carries its source (UI, webhook,
  poller) and, if known, the commit sha. `deployment_events` stays as the per-deployment
  transition log. The event log holds one line per outcome, not every transition.
- **Container lifecycle:** a reconciler restart (exit code, OOM if podman reports it), a service
  entering or leaving crash loop, a health change of a live service.
- **Certificates:** issued, renewed, failed (with the reason and next retry), DNS mismatch.
- **Operator and system:** a stack applied or removed, a secret set or deleted (name only, never
  the value), hoster started, hoster updated or rolled back.

A "5xx spike" event depends on persisted traffic history (epic `t97`) and is not part of v1.

**3. Explicit writes.** The producer (orchestrator, reconciler, ACME task, web handlers, update
flow) writes the event through the store, in the same transaction as the state change it
describes when there is one. After the write it sends a `UiEvent` so live pages append it. The
event log never depends on a broadcast receiver keeping up.

**4. Retention: 30 days.** A daily task deletes older rows. This is not configurable in v1.

**5. Three views:** a **Recent events** card on the dashboard (newest first, appended live over the
existing SSE stream), the same feed filtered to one stack on the stack overview, and an **Events**
page with every event, paginated, filterable by kind, severity and stack.

**6. Notifications build on it.** The notifications epic (`c5s`) will be a filter over this
table. Its channels, rate limiting and configuration are decided there in their own ADR.

## Alternatives considered

- **A recorder that subscribes to `UiEvent`:** touches less code, but drops events when it lags,
  and restarts and operator actions are not on the broadcast.
- **Deriving the feed from `deployment_events` plus the journal:** no new table, but the journal is
  not queryable from the UI without a reader, and it rotates on its own schedule.
- **A count cap or configurable retention:** a fixed 30 days matches the planned traffic-history
  range and is enough on one VPS. A setting can come later if someone needs it.

## Consequences

- Every producer gains a store call. The reconciler and ACME task, which are in-memory today,
  write to SQLite on each restart or issuance. Both are rare enough that the I/O doesn't matter.
- The table is append-only apart from the retention sweep. Backups (ADR-0030) include it.
- Down-migrating (ADR-0052 rollback) drops the table and with it the history. That is acceptable
  for a 30-day feed.
- A crash-looping container writes at most one event per restart, which with ADR-0028's backoff
  is at most one every 5 minutes in steady state.
