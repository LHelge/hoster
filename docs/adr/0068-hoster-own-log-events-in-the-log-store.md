# ADR-0068: hoster's own log events at info and above are recorded in the log store

- Status: Accepted (amends ADR-0067)
- Date: 2026-11-12

## Context

ADR-0067 persists what containers print and one row per proxied request in `logs.db`. hoster's
own `tracing` events (deploy steps, health checks, certificate orders, upstream failures, the
reconciler's restarts) still go only to stderr and the user journal (ADR-0044, ADR-0045), which
the admin UI cannot read. The Telemetry mockup (`docs/design/Telemetry.dc.html`) shows such lines
in the same record list as the apps' lines, which is where they explain an app's behaviour ("the
health check failed" beside the app's own error).

The owner decided on 2026-11-12, while the telemetry epics were planned, that hoster records its
own events at `info` and above. The rest is the design that follows from it.

## Decision

1. **A second layer.** `Logging` installs a layer beside the stderr formatter that hands every
   event at `info` or above to the log store's recorder (ADR-0067 §6). Its filter is fixed, not
   `RUST_LOG`, so what the UI shows does not depend on how the journal is tuned. The stderr
   output and the journal are unchanged.
2. **Excluded targets.** The log store's own modules (follower, recorder, housekeeping), so that
   recording cannot feed itself, and the per-request access target, which the request rows
   already cover (ADR-0067 §5).
3. **Record shape.** Service `hoster`; stack and deployment generation from the `stack` and
   `generation` fields of the event or its enclosing spans (`LogSpan` sets them for deploy code),
   and empty otherwise; stream `hoster`; level from the event; message from the event's message;
   fields as the JSON formatter writes them (the event's other fields, `target`, `span`,
   `spans`), so the parser's mapping of ADR-0067 §3 applies unchanged. Trace and span ids stay
   empty until hoster has trace context (OQ-42, question 6).
4. **Never blocking.** The layer offers each record to the recorder's bounded channel and drops
   it, counted, when the channel is full. Events emitted before the recorder has started or after
   it has stopped are in the journal only. The per-container line-rate cap of ADR-0067 §7 applies
   to `hoster` as to one container.
5. **Where they show.** A record with a stack belongs to that stack, under the service `hoster`,
   on the stack's logs tab and on the Telemetry page. Records without a stack appear on the
   Telemetry page only, under `hoster` in its stack selector.

This amends ADR-0067 §4: `stack` is nullable and `stream` takes `hoster` besides `stdout` and
`stderr`. Retention, the size cap and search are those of ADR-0067.

## Alternatives considered

- **Warnings and errors only**: trivially small, but drops the lines that explain a deploy or a
  restart, which are logged at `info`.
- **The journal only** (unchanged): the UI keeps a blind spot for exactly the events that hoster
  itself causes.
- **Following `RUST_LOG`**: debugging the journal at `debug` would flood the store, and
  `RUST_LOG=warn` would empty the UI.
- **Reading the journal back into the store**: the dependency on journal access that ADR-0044
  avoided.

## Consequences

- hoster's own lines are searchable for 7 days in the UI, beside the apps' lines and filtered
  by stack.
- `logs.db` holds what hoster logs at `info`: stack names, image references, hostnames,
  certificate orders. The journal already held the same text.
- A burst of hoster's own events can be dropped under load; the drop is counted as other losses
  of ADR-0067 are.
- `stack` being nullable in `log_records` is fixed in the log store's first migration, before
  any data exists.
