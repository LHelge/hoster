# ADR-0062: Traffic history is persisted in SQLite as 1-minute buckets for 24 h and 1-hour buckets for 60 days, per route, flushed once a minute

- Status: Accepted (amends ADR-0045; fills the traffic history ADR-0054 left out)
- Date: 2026-11-11

## Context

The design's dashboard has a time-range picker, responses-by-status bars, p50/p95 lines,
"vs previous period" deltas and per-stack req/min with sparklines; the stack cards and the stack
overview show the same figures for one stack (`docs/design/Main.dc.html`, `Stacks.dc.html`,
`StackDetail.dc.html`). ADR-0054 adopted the look but not these views, because hoster had no
history to draw them from.

Today `src/stats/` keeps per-route counters by status class, upstream errors and an in-flight
gauge since hoster started, plus p50/p95 over a rolling window of five minutes (ADR-0045). All of
it resets on restart, so a self-update (ADR-0052) wipes the dashboard. The event log (ADR-0055)
is the closest existing pattern: SQLite, no stack foreign key, fixed retention.

The owner decided the points marked *(owner)* below on 2026-11-11 (epic `t97`). The rest is the
design that follows from them.

## Decision

**1. Storage** *(owner)*. Two new tables in the main SQLite database (`hoster.db`, ADR-0022),
created by one reversible migration (ADR-0052): `traffic_minutes` and `traffic_hours`. Backups
of the database (ADR-0030) include them.

**2. Resolution and retention** *(owner)*. 1-minute buckets kept 24 h; 1-hour buckets kept 60
days, so every range, 30 d included, has a previous period. Both are constants, not settings.

**3. Key** *(owner)*. Per route (host, prefix) with its owning stack and service. Stack and
dashboard figures add routes up; latency histograms merge.

**4. Schema.** Both tables have the same shape:

| Column | Meaning |
|---|---|
| `start` | Bucket start, ms since the epoch (`Timestamp`), aligned to a UTC minute or hour |
| `host`, `prefix` | The route as the route table spells it |
| `stack`, `service` | The route's owner |
| `none`, `s1xx`, `s2xx`, `s3xx`, `s4xx`, `s5xx` | One count per `StatusClass` |
| `upstream_errors` | Requests that failed because of their upstream |
| `latency` | The latency histogram (§6) |

- Primary key `(start, host, prefix, stack, service)`, so writing a bucket again adds to it
  (an upsert). It also serves "everything over a span". An index on `(stack, start)` serves
  "this stack over a span".
- **No foreign key to stacks**: a removed stack's history stays until retention removes it, as
  its events do (ADR-0055).
- The key columns are `NOT NULL`, with `''` for "none", because SQLite treats `NULL`s in a key as
  distinct, so an upsert on them would never match. **Hoster's own routes** (the admin UI,
  `Owner::Reserved`) are stored with their host and prefix, `stack = ''` and `service` set to the
  reserved purpose (`admin UI`). **Unrouted requests** are one series with `host`, `prefix`,
  `stack` and `service` all `''`.
- A (host, prefix) that changes hands within a bucket gets one row per owner, as the in-memory
  counters start over today.

**5. Only buckets with requests are written.** Buckets are aligned to UTC minutes and hours. A
request counts in the bucket it finished in, to within the recorder's tick, which runs just after
each minute ends (§7).

**6. Latency per bucket** is the existing log-linear histogram (`src/stats/latency.rs`: exact
below 4 µs, then four buckets per power of two of microseconds, 108 buckets, within 25 %),
stored as a **compact versioned blob**: one version byte (`1`), then for each non-empty bucket
its index (one byte) and its count (unsigned LEB128). An empty histogram is the version byte
alone. The bucket layout is now a persisted format: changing it needs a new version that still
decodes the old one. Decoding an unknown version or malformed bytes is an error, never a panic.
Percentiles for a bar or a total come from summing histograms.

**7. Writing** *(owner: one write per minute; a crash loses at most a minute)*. A recorder task
in `src/stats/` ticks just after every wall-clock minute. Each tick drains the in-memory counters
(the per-route delta since the previous drain; the proxy's hot path stays lock-free) and writes,
in **one transaction**, the minute rows of the minute that ended and adds them to their hour rows
(counts added; histograms read, merged in Rust and written back, since a blob cannot be merged
in SQL). A late or skipped tick still writes one bucket and never a duplicate. A failed write is
logged at `warn` and kept for the next tick, under its own bucket start, for up to 5 minutes;
after that it is dropped, so nothing grows without bound and the proxy never waits. On
**graceful shutdown** the recorder flushes once more after the proxy has stopped (the partial
minute is upserted into its bucket, so a restart within the same minute adds to it). A
**retention sweep** deletes minute rows older than 24 h and hour rows older than 60 d, at
startup and then hourly.

**8. Reading** *(owner: pages read flushed data only, up to a minute behind)*. The live minute
is never included.

- **Windows** *(owner)*. The dashboard follows its range picker (1h, 6h, 24h, 7d, 30d; default
  24h). Pages without a picker (the stack cards on `/stacks`, the stack overview) show a **fixed
  last hour**, one constant shared by both.
- **Bars per range**: 1h = 60 x 1 min, 6h = 72 x 5 min (both from `traffic_minutes`), 24h = 24
  x 1 h, 7d = 56 x 3 h, 30d = 30 x 1 d (from `traffic_hours`). They are rolling windows counted
  back from the newest flushed minute (or the current, partial, hour), not calendar days, so no
  time zone is involved; axis labels are relative (`-24h … now`), as in the mockups. A bucket
  without rows is an empty bar, and a range reaching past retention or across downtime is not an
  error.
- **req/min** *(owner)* is the rate over the last 5 flushed minutes, whatever the range. The
  stack overview's tile adds the **peak minute** of its hour and how long ago it was.
- **Deltas.** A "vs previous" delta compares a range's totals with the period of equal length
  just before it. It is left out when the previous period holds no rows (fresh install, history
  shorter than twice the range, or no traffic then), since a change from nothing has no
  meaning. The tone stays neutral.
- **What the dashboard totals cover**: as today, every stack's routes plus unrouted, without
  hoster's own routes. Hoster's own routes, unrouted and removed stacks stay in the note under
  the dashboard's stacks table.
- **Live values that stay in memory**: the in-flight gauge only. The rolling latency window and
  the since-start counts are removed once the pages read history.

**9. UI under the CSP** (ADR-0054 unchanged). The picker is a group of `?range=` links; charts
are server-drawn SVG whose geometry is computed in Rust and whose colours come from classes in
`assets/hoster.css`; pages work without JavaScript. The dashboard's live overview stream carries
the range in its URL, so the partials it sends stay on the chosen range; its periodic refresh
(once a minute) picks up each flush, so no new `UiEvent` is needed.

**10. Out of scope** *(owner)*. A slowest-routes panel, a range picker on the stack pages and
egress bytes are proposals, not part of this decision. SLO lines and uptime % are not adopted.

## Size and write cost

A row is about 150 bytes (key strings of about 50, nine small integers, a histogram blob of
20–60 bytes for the few non-empty buckets a route has, record overhead) plus about 70 bytes in
the primary-key index and 20 in the stack index: about 250 bytes. A route with requests every
minute has at most 1,440 minute rows and 60 × 24 = 1,440 hour rows, so **about 0.7 MB per
always-busy route**; a dozen routes stay under 10 MB, and idle routes cost nothing. Each flush
is one transaction of at most one minute row and one hour-row read and write per active route:
tens of statements and one WAL commit (`synchronous = NORMAL`) a minute.

## Alternatives considered

- **A separate SQLite file:** isolates the write load, but needs its own migrations, backup step
  and rollback story; the load is one small transaction a minute.
- **A ring file (fixed-size binary archive, RRD style):** constant size, but hand-written
  format, no queries by stack and nothing the backup command already covers.
- **1 min for 24 h plus 1 h for 30 d:** the 30d range would have no previous period.
- **One 5-minute table:** simpler, but too coarse for the 1h range and its peak minute, and
  8,640 rows per route for 30 d without a previous period.
- **Per-stack or per-service keys:** fewer rows, but loses which route a figure belongs to and
  cannot attribute hoster's own routes and unrouted requests the way the dashboard does.
- **A fixed 24 h window everywhere:** no range picker, as the mockups have one.
- **Mean req/min over the range:** hides what is happening now; a 30 d mean says little.
- **Including the live minute:** fresher figures, but every page would merge the in-memory
  counters with the rows, and the last bar would be partial.
- **A metrics endpoint with an external TSDB** (Prometheus): ADR-0045 rejected it; it needs
  software beyond the single binary.

## Consequences

- Traffic figures survive restarts and self-updates. ADR-0045's "statistics reset when hoster
  restarts" no longer holds for traffic; the in-flight gauge alone is live.
- Pages lag by up to a minute, and a crash loses at most the last minute.
- The histogram's bucket layout is a persisted format with a version byte.
- Down-migrating (ADR-0052 rollback) drops both tables and the history, which is acceptable
  for a dashboard.
- The "5xx spike" event ADR-0055 deferred now has the data it needs; it is not part of this
  decision.
