# ADR-0067: Container logs and the proxy's requests are parsed and persisted in a separate SQLite file for 7 days

- Status: Accepted (resolves OQ-41; amends ADR-0044 and ADR-0045; amended by ADR-0068, which records hoster's own events at info and above); spans, OTLP log records and the request row's span ids are added by ADR-0069
- Date: 2026-11-12

## Context

Container logs stay in podman and the UI streams one container at a time (ADR-0044). They are
gone once a generation is cleaned up, they cannot be searched or filtered by level, and the
proxy's per-request log goes to the journal, where the UI cannot read it per stack. The stack
logs design (`docs/design/StackLogs.dc.html`, epic `psq`) wants a merged view of a stack's
services, grep, a level filter, download and an access-log source.

The owner's apps log through `tracing`. Its JSON formatter writes level, message, fields and
spans as one JSON object per line on stdout, with no SDK and no network path, and
`aphid-server` does so with `APHID_LOG_FORMAT=json`. Third-party images write plain text.

The owner decided the points marked *(owner)* below on 2026-11-12 (task `xuq`). The rest is the
design that follows from them.

## Decision

**1. hoster persists logs** *(owner)*. A follower per running managed container reads its
stdout and stderr through bollard `logs` with follow and timestamps, from the moment hoster
starts the container until it is removed. Each line becomes a record in the log store. podman's
size-capped `k8s-file` (ADR-0044) stays as the capture buffer: the store keeps a cursor per
container (the time of the last stored line), and after a hoster restart a follower resumes
from it, so lines written while hoster was down are read once it is back, up to what the file
still holds.

**2. Storage** *(owner)*. One SQLite file, `logs.db`, in the state directory beside `hoster.db`,
mode 0600, WAL, with its own pool and its own reversible migrations (ADR-0052). It is created
with `auto_vacuum = INCREMENTAL` so that retention can give pages back to the file system.
Nothing in `hoster.db` refers to it, and it has no foreign keys to stacks: a removed stack's
logs stay until retention removes them, as its events do (ADR-0055).

**3. Parsing** *(owner)*. A line that parses as a JSON object is mapped by the common keys:

| Record field | Keys read, first match wins |
|---|---|
| level | `level`, `severity`, `lvl` (names in any case and the usual numeric scales), normalised to `trace`, `debug`, `info`, `warn`, `error` |
| message | `message`, `msg`, and `fields.message` as `tracing` writes it |
| fields | every other key, kept as JSON (`tracing`'s `fields`, `target`, `span` and `spans` included) |

Any other line is stored whole as the message with no level. There is no guessing of levels in
text formats. A record's time is the time podman stamped on the line, not a time the line
claims. A line longer than 16 KiB is cut at that length and marked as cut.

**4. Record shape.** One `log_records` table: an ascending id (insertion order, the tiebreak
for equal times), time in ms since the epoch, stack, service, deployment generation, stream
(`stdout` or `stderr`), level (nullable), message, fields (JSON text, nullable) and nullable
trace and span ids, which stay empty until OQ-42. An index on `(stack, time)` serves a stack
over a span; the id serves paging and following.

**5. The proxy's requests** *(owner)*. The proxy stores one row per finished request in a
`requests` table in the same file: time, host, prefix, stack, service, method, path, status,
time to the response header and in total, response bytes, client address, whether its upstream
failed, and a request id. Rows go from the proxy thread through a bounded channel to the
recorder; when the channel is full the row is dropped and counted. The proxy never waits for
the store. The journal line per request is kept at `debug` only. Traffic history (ADR-0062)
keeps its own buckets and is not derived from these rows.

**6. Writing.** One recorder task owns all writes: it collects records and rows and commits
them in one transaction about once a second and on graceful shutdown, as the traffic recorder
does (ADR-0062). Pages read committed data only.

**7. Retention and caps** *(owner)*. Constants, not settings:

- Records and rows older than **7 days** are deleted hourly.
- If `logs.db` is larger than **1 GiB** after that, the oldest hour is deleted, across both
  tables, until it is not; each pass ends with an incremental vacuum.
- A container that writes more than a fixed number of lines per second (a constant, set when
  the follower is built) has the excess dropped. One record with the number of dropped lines
  marks each gap.

**8. Search** *(owner)*. A query narrows by time, stack, service, level and stream with the
index, then matches a substring or regular expression against the message and fields of those
rows in Rust, newest first, in pages, with a cap on the rows scanned per request. No full-text
index.

**9. Backups** *(owner)*. `hoster backup db` (ADR-0030) does not include `logs.db`. A restored
server starts with empty logs.

**10. Live view.** The container log page of ADR-0044, which streams straight from podman, is
kept: it shows a line before the recorder has committed it and works when the store is
unavailable.

## Alternatives considered

- **Logs stay in podman only** (ADR-0044 as it was): the stack logs page would merge N live
  streams per request; no history, no search across removed containers.
- **An external log server** (Loki and the like): a second service to run, against ADR-0001 and
  ADR-0045.
- **One SQLite file per UTC day**: the agent's recommendation. Retention is deleting a file and
  never needs a vacuum, but every query has to read several files and merge them. The owner
  chose one file for the simpler queries and migrations.
- **The main `hoster.db`**: log bursts would share the single writer with deploy state, and
  backups would grow with log volume.
- **DuckDB**: about ten times smaller on disk and faster at aggregates; a heavy C++ build for
  the static musl binaries and weaker at many small appends.
- **Guessing levels in text lines** (`ERROR`, `[warn]`, logfmt), as the first `psq` stub
  planned: gives third-party images levels, with wrong guesses.
- **An FTS5 index**: fast word search, but roughly twice the disk, no arbitrary substrings or
  regular expressions, and FTS5 would have to be in the sqlx build.
- **30 days and 2 GiB**, **3 days and 500 MB**.
- **Including logs in backups**, or a flag for it.
- **An in-memory ring per route** or **reading the journal** for the access log: no history to
  search, or a dependency on journal access that ADR-0044 already found fiddly.

## Consequences

- Logs outlive their container for 7 days and can be searched; the stack logs page (`psq`)
  becomes queries on the store.
- Whatever an app prints is now on disk for 7 days in a second file, secrets it logs included.
  The file has the state database's permissions, and podman already kept the same text.
- hoster holds one open log stream per running container, and writes about once a second while
  anything logs.
- A busy server reaches the 1 GiB cap before 7 days: about 350 MB a day at one request a second
  with ten records each. Then the cap, not the age, decides how far back logs go, and the page
  has to say from when its data starts.
- Deleting rows from one file costs writes and a vacuum that per-day files would not need.
- Lines are lost when hoster is down for longer than podman's log file covers, when the rate
  cap is hit, and when the proxy outruns the recorder. Each loss is counted or marked.
- Text lines have no level, so the level filter hides or shows them as a group.
- Client addresses and request paths are stored for 7 days; the guide on logging has to say so.
- This file is where OTLP log records and spans would be stored (OQ-42).
