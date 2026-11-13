---
title: Logging from an app
category: Guides
---

hoster keeps what your containers print. Every line a managed container writes to stdout or
stderr becomes a record in the log store, `logs.db`, and stays there for 7 days. You do not add
an SDK or point the app at a log server: print one JSON object per line and hoster reads the
level, the message and the other fields from it. Plain text works too; it just has no level.

This page covers what hoster reads from a line, what it keeps and for how long, and how to get
JSON lines out of a few common loggers. An app with an OpenTelemetry SDK can send its logs, and
its traces, over OTLP instead: see [[tracing|Traces and logs over OTLP]]. Use one or the other
for logs, not both, or every event is stored twice.

# How a line is read

hoster follows each managed container from the moment it starts it until the container is
removed. Each line is one record, with the stack, service, deployment generation and stream
(`stdout` or `stderr`) taken from the container, never from the line. A record's time is the
time podman stamped on the line, not a time the line claims.

## JSON lines

A line that parses as a JSON object is mapped by the keys loggers commonly use. For each part,
the first key present wins:

| Part | Keys |
|---|---|
| level | `level`, `severity`, `lvl` |
| message | `message`, `msg`, then `fields.message` (where `tracing-subscriber` puts it) |
| fields | every other key, kept as JSON |

Level names are read in any case and folded onto `trace`, `debug`, `info`, `warn` and `error`:
`warning` is `warn`, `notice` is `info`, `fatal`, `critical` and `panic` are `error`, and so on.
Two numeric scales are read, as numbers or as strings like `"30"`:

| Scale | trace | debug | info | warn | error |
|---|---|---|---|---|---|
| pino, bunyan | 10 | 20 | 30 | 40 | 50, 60 |
| syslog | | 7 | 5, 6 | 4 | 0–3 |

Any other number, and any name hoster does not know, gives no level, and the key stays in the
fields. OpenTelemetry's severity numbers (1–24) are not read from a line: they overlap syslog's.

So this line from a Rust app:

```json
{"timestamp":"2026-11-12T10:00:00.000Z","level":"WARN","fields":{"message":"slow query","ms":812},"target":"blog::db"}
```

becomes a `warn` record with the message `slow query` and the fields
`{"timestamp": "…", "fields": {"ms": 812}, "target": "blog::db"}`.

## Text lines

Any other line, including a JSON value that is not an object, is kept whole as the message with
no level. hoster does not guess levels in text: `ERROR: disk full` has none. A level filter
shows or hides text lines as a group. Third-party images that print text (nginx, postgres) are
stored this way.

## Long lines

A line longer than 16 KiB is cut at 16 KiB and marked as cut. A cut JSON line no longer parses,
so it is kept as text, with no level.

# What is kept, and for how long

**Retention.** Records older than 7 days are deleted every hour. If `logs.db` is still larger
than 1 GiB after that, the oldest hour is deleted until it is not. Whichever limit comes first
decides how far back the logs go: a busy server reaches 1 GiB before 7 days. These are
constants, not settings.

**The line-rate cap.** A container that prints more than 1000 lines in one second has the rest
of that second dropped. Each run of dropped lines leaves one `warn` record in its place, such as
`6 lines dropped: more than 1000 lines a second`.

**hoster restarts.** podman keeps capturing a container's output while hoster is stopped. When
hoster starts again it reads on from the last line it stored, so nothing is stored twice and
lines written in between are picked up. podman's log file is size-capped, though. If it no
longer holds everything hoster missed, a `warn` record says that the lines between two times
*were lost*, or *may have been lost* when hoster cannot tell for sure.

**hoster's own events.** hoster records its own log events at `info` and above (deploy steps,
health checks, certificate orders, restarts) in the same store, as the service `hoster` with the
stream `hoster`. An event about a stack belongs to that stack, so a failed health check sits
beside the app's own error. This level is fixed and does not follow `RUST_LOG`; the journal is
unchanged ([[install#operating-hoster|Operating hoster]]).

**Where to see it.** The admin UI's System page has a **Log store** card: the file's size
against the 1 GiB cap, how far back it reaches, and what was lost since hoster started (to the
rate cap, a full queue or a failed write). `hoster doctor` checks that `logs.db` exists with
mode `0600` and opens. A stack's **Logs** tab reads the store: every service's records merged
oldest to newest, hoster's own records about the stack under `hoster`, over a range of 5
minutes to 7 days, with older and newer pages. The newest page follows new records as they
are stored, about once a second, until you switch **follow** off or page back. A record with
fields opens on them, nested keys flattened to dotted ones (`span.request_id`,
`spans.0.name`), and a click on a value lists only the records with that value, such as every
line of one request. It links each container's live log, which still
streams straight from podman, so it shows a line before it is stored and works when the store
is unavailable. The **Telemetry** page's **Logs** tab has the same filters across every stack,
with a stack column and a stack selector (every stack, one, or `hoster` for hoster's own
records that name no stack), over the last 15 minutes unless you pick another range. It does
not follow new records; reload it, or follow on the stack's tab.

**Downloading.** The stack's **Logs** tab has a **download** button for the containers'
records: every record the current filters select over the whole range, oldest first, not only
the page shown, as a file named after the stack, the start of the range and its length
(`blog-2026-11-12T10-00Z-1h.log`). **text** writes one line per record,
`<time> <service> <level or -> <message>`, with line breaks in a message written as `\n` and
`\r` so that one record stays one line for `grep`. **JSON lines** writes one object per record
with its time, stack, service, generation, stream, level, message and fields. A download holds
at most 100,000 records; one that reaches that ends with a line saying from which time on the
rest was left out, so narrow the range or the filters and download again from there.

**Backups and rollback.** `hoster backup db` does not include `logs.db`, and a restored server
starts with empty logs ([[backup]]). `hoster rollback` undoes the log store's schema too
([[install#updates-and-rollback|Updates and rollback]]).

# The proxy's requests

hoster's proxy stores one row per request it routed to a stack in the same file, under the same
retention: when the request arrived, the host and the matched route's prefix, the stack and
service, the method, the path, the status sent to the client, the time until the app's response
header and in total, the response body bytes, the client's address and whether the app could
not be reached or timed out. The path is stored without its query string, which can hold
tokens. A WebSocket or another upgraded connection is one row with status 101, written when it
closes, so its total is how long it was open.

Each request gets a request id, 32 hex digits, made when it arrives.

Requests that never reach an app are not stored: an unknown host or path (404), a host that is
not the TLS connection's name (421), everything on port 80 (redirects and ACME challenges) and
the admin UI's own host. That is what scanners send, and it has no stack to belong to.

The proxy never waits for the store. If the store falls behind, rows are dropped, and the
System page's **Log store** card counts them.

# Privacy

Whatever an app prints is on disk for 7 days in `logs.db`, secrets included. The file sits in
the state directory beside `hoster.db` with the same permissions (`0600`, the `hoster` user),
and podman already kept the same text. Do not log tokens, passwords or request bodies you would
not want on disk. The proxy's requests are kept there for 7 days too, with client addresses and
request paths (without the query string).

# Examples

## Rust with `tracing`

`tracing-subscriber`'s JSON formatter writes one object per line on stdout, with the level, the
event's fields (message included) under `fields`, the `target`, and the current span and span
list. Add the crate with the `json` and `env-filter` features:

```sh
cargo add tracing
cargo add tracing-subscriber --features json,env-filter
```

and install it at the start of `main`:

```rust
tracing_subscriber::fmt()
    .json()
    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
    .init();
```

Set the level in the stack file with `RUST_LOG` in the service's `env`
([[stack-format#2-2-service|Stack file format, §2.2]]). `.flatten_event(true)` moves the
event's fields to the top level; hoster reads both shapes.

## aphid-server

`aphid-server` writes JSON lines when `APHID_LOG_FORMAT=json` is set:

```toml
[services.docs.env]
APHID_LOG_FORMAT = "json"
```

From version 0.4.0 it also exports over OTLP, and so stores each line twice unless one of the
two is turned off: the lines on stdout with `APHID_LOG_FORMAT = "none"` (0.5.1 or later), or
the export ([[tracing#aphid-server|Traces and logs over OTLP]]).

## Node (pino)

pino writes JSON to stdout by default, with the numeric level and `msg`:
`const log = require("pino")(); log.info("ready");`

## Go (`slog`)

The standard library's JSON handler writes `level` and `msg`:
`slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stdout, nil)))`

# Why it works this way

The reasons, and the alternatives that were turned down (a log server, guessing levels in
text, longer retention, logs in backups), are in
[ADR-0067](https://github.com/LHelge/hoster/blob/main/docs/adr/0067-persisted-structured-logs.md)
and, for hoster's own events,
[ADR-0068](https://github.com/LHelge/hoster/blob/main/docs/adr/0068-hoster-own-log-events-in-the-log-store.md).
