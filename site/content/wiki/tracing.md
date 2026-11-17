---
title: Traces and logs over OTLP
category: Guides
---

hoster receives OpenTelemetry traces and logs from the apps it runs. An app with an
OpenTelemetry SDK sends spans and log records over OTLP, and hoster keeps them in the log store
beside the containers' printed lines ([[logging]]). Every service is already pointed at hoster
through the standard `OTEL_*` environment variables, so an SDK that reads its environment needs
no endpoint, token or other setting in the stack file.

hoster also starts a trace for every request its proxy routes and sends it to the app as a
`traceparent` header. An app that continues that trace has its spans nested under the proxy's.

This page covers what hoster gives a service, what it accepts and keeps, how the proxy's trace
context reaches an app, what goes wrong and how it shows, and how to set up a Rust app and
`aphid-server`. If you only want logs, printing JSON lines on stdout ([[logging]]) is simpler
and needs no SDK.

# What every service gets

hoster sets six variables on every service: the endpoint `http://host.containers.internal`, the
protocol `http/protobuf`, `OTEL_METRICS_EXPORTER=none`, `OTEL_SERVICE_NAME` (the service's
name), `OTEL_EXPORTER_OTLP_HEADERS` (the service's own bearer token) and
`OTEL_RESOURCE_ATTRIBUTES` (`service.namespace=<stack>,hoster.generation=<generation>`). The
table, and which of them are in the config hash, are in
[[stack-format#2-2-5-opentelemetry-export-adr-0069|Stack file format, §2.2.5]].

- **The stack file wins, per variable.** A name the service sets in `env` or `secrets` keeps the
  stack file's value, and hoster adds the others. Setting `OTEL_SERVICE_NAME` changes only the
  name shown; it never changes where data is stored.
- **The token decides where data goes.** Every export is stored under the stack and service the
  token names, whatever `service.name` or other resource attributes the payload claims.
- **Turning export off.** `OTEL_SDK_DISABLED = "true"` in `env` turns off SDKs that read it.
  The Rust SDK does not, so the example below checks it itself. An app that exports only when
  an endpoint is set (like `aphid-server`) is turned off with `OTEL_EXPORTER_OTLP_ENDPOINT = ""`.

## When an app starts exporting

The variables are added when hoster creates a container, so a running container does not have
them until it is replaced:

- **After the upgrade** to the hoster release that added them, each stack's first deployment
  replaces all of its services, by their update strategy, even where nothing else changed.
  Nothing redeploys by itself: a container exports once it has been replaced.
- **A brand new service** is refused (401) until its first deployment has finished, so the
  spans and records it sends while starting up for the first time are lost. Later
  deployments of the same service export from the first moment: the token is the same for
  every deployment.
- **A removed service** is refused once its removal has been applied.

# What hoster accepts

The receiver speaks OTLP over HTTP with protobuf bodies, the encoding
`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` asks for. It takes logs and traces only.

| Request | Answer |
|---|---|
| `POST /v1/traces`, `POST /v1/logs`, `Content-Type: application/x-protobuf`, plain or `Content-Encoding: gzip` | 200, with `partial_success` when part was dropped (see below) |
| No `Authorization: Bearer <token>`, or a token that does not verify or names no current service | 401 |
| Any other content type or encoding: OTLP's JSON encoding, `zstd` | 415 |
| A body over 4 MiB as sent, or over 16 MiB once inflated | 413 |
| A body that does not decode as an export request | 400 |
| `/v1/metrics`, or any other path | 404 |
| A fifth request of the same service while four are in progress | 429 with `Retry-After: 1` |

gRPC is not accepted. An SDK that defaults to gRPC (port 4317) has to be switched to OTLP/HTTP
with protobuf; most read `OTEL_EXPORTER_OTLP_PROTOCOL`, but some need the HTTP exporter chosen in
code or installed as a separate package. `nginx`'s OpenTelemetry module exports over gRPC only,
so nginx cannot send traces to hoster; its requests still have the proxy's spans (below).

The receiver is only reachable from containers on the same host, as
`http://host.containers.internal` on port 80. It is not a public endpoint and has no route.
`hoster doctor` checks that a container can reach it.

# The proxy's trace

hoster's proxy starts a trace for every request it routes to a stack and has two spans of its
own: a server span for the request and a client span for the request it makes to the app. It
sends the app a `traceparent` header, `00-<trace id>-<span id>-01`, whose trace id is the
request's id in the stack's access log and whose span id is the proxy's client span
([[stack-format#2-3-route|headers the app receives]]).

- **An app that continues it** (its SDK has the W3C trace context propagator and extracts the
  incoming header into its request span) has its spans in the same trace, nested under the
  proxy's client span. Log records it exports inside that span carry the same trace id.
- **An app with no SDK**, or one that ignores the header, still has a trace of two spans for each
  request: the proxy's, kept with the request's row in the access log.
- **A client's own `traceparent`** and `tracestate` are removed and never reach the app. A
  client cannot choose trace ids or join a trace it does not own, so a trace always starts at
  hoster's edge.

# What is kept, and for how long

**Log records** go into the same store as printed lines, with the stream `otlp`:

- the level from the record's severity number (1–24, folded onto `trace` to `error`), or from
  its severity text when there is no number;
- the body as the message (a string as it is, anything else as JSON), cut at 16 KiB like a long
  line;
- the attributes as fields, and the trace and span ids the record was made in;
- the deployment generation from `hoster.generation`.

**Spans** keep their trace, span and parent ids, name, kind, start and duration, status and
status message, attributes, events and the resource's attributes. Span links and the
instrumentation scope are not kept. A span without a valid trace or span id is rejected.

**Times** are the ones the SDK sent. A time later than the moment hoster received the export
becomes that moment, and so does a missing one: a clock running ahead cannot keep data past its
retention or sort it into the future.

**Retention.** Spans are kept 3 days and log records 7 days, like printed lines. All of it,
the access log included, shares the log store's 1 GiB cap: when `logs.db` is larger, the oldest
hour is deleted first ([[logging#what-is-kept-and-for-how-long|What is kept]]). There is no
sampling.

**Where to see it.** Exported log records appear in the stack's **Logs** tab and the
**Telemetry** page's **Logs** tab, merged with every other record of the stack. A row shows
the generation it came from, and a record with attributes opens on them as fields, with the
stream `otlp` in its details. Traces are on the **Telemetry** page's **Traces** tab (below).

# The Traces tab

**Telemetry → Traces** is the Telemetry page's first tab. It lists every trace whose root span
started in the range, across every stack or the one picked, newest first by default. Its tab
shows how many traces the range holds, and the Logs tab beside it how many log records.

- **Columns.** The root span's start (UTC) and name (the method and path of a proxied request),
  the stack, the services with a span in the trace as coloured chips, the number of spans, the
  root's duration with a bar scaled to the slowest trace on the page, and the status.
- **The proxy's spans** are listed as the service `hoster`, the name hoster's own log records
  have, in a colour of their own. Every proxied request has them.
- **app not instrumented** marks a request whose app was called but sent no spans of its own:
  only the proxy's two. A request hoster answered itself, such as a 503 before any upstream
  request started, has only the proxy's server span and no such label.
- **Status** is `error` when any span of the trace is in error: an app's span with an error
  status, or the proxy's for a 5xx answer or a failed upstream request.
- **Range.** The last 5m, 15m (the default), 1h, 6h or 24h. Spans are kept 3 days, so there is
  no 7d as on the Logs tab.
- **Sort** by newest, slowest or errors first. **Filters**: the status (`ok` or `error`),
  *slower than* (a root duration such as `500ms`), and the grep box, which matches span names
  (the root's, or any span an app sent) as text, ignoring case. There is no query language.
- **Selecting a trace** (its root span name, or `?trace=<trace id>` in the URL) marks its row
  and opens it below the list. The trace id of a proxied request is its request id in the
  access log. A trace that is no longer stored (older than 3 days, or never seen) says so.

Every option is a parameter of the page's URL, so a filtered list or a selected trace can be
bookmarked or linked.

The header's first line is the receiver's: a green dot while hoster stores what it receives
(red, with "not storing", when the log store cannot be opened), the endpoint services send to,
and the spans and log records stored per second over the last minute.

## The waterfall

The selected trace opens as a waterfall below the list:

- **The header** names the stack, the root span, the trace id and when the trace started
  (UTC), with its duration (from the earliest span's start to the latest span's end), the
  number of spans, how deep the tree goes, and how many spans are in error (red when any are).
- **The legend** lists each service with its number of spans, in the same colours as the
  list's chips. The proxy's spans are `hoster`.
- **The minimap** draws every span as a short line on the trace's time axis, red when it is
  in error, so a long trace's shape shows at a glance.
- **One row per span**, each under its parent and children by start time, indented by depth.
  A span in error has a red `!` and its bar a red ring. A tag says what a span calls, from its
  attributes: the database (`db.system`: `pg` for PostgreSQL, `redis`, `mysql`, …) or `exec`
  for a process (`process.command`).
- **The bars** share one axis from the trace's start to its end, labelled at its quarters,
  with the span's duration beside the bar (inside it when the bar is wide).
- **Selecting a span** (its row, or `&span=<span id>` in the URL) marks its row and opens its
  details under it; the link keeps the trace and the list's options. A trace opens on the
  span where it went wrong: the first span in error with no child in error (the deepest of
  the first error chain), or the root when nothing failed. Following the selected row again,
  or the close button, closes the details (`&span=none` in the URL).
- **Partial traces.** A span whose parent never arrived (lost, late, or not yet received) is
  drawn under the root and marked *parent missing*. A trace read stops at its first 1,000
  spans by start, and the waterfall then says how many more it has.

### Span details

The selected span's details show:

- **The title line**: the service's colour, the span's name and service, its kind (`server`,
  `client`, `internal`, …) and its status (`error`, `ok` or `unset`).
- **START** (how long after the trace's start it began), **DURATION** with its share of the
  trace, **SELF TIME** (its duration less the time its children cover, so time spent waiting
  on them is not counted twice), its **SPAN ID** and its **PARENT** (a link to the parent
  span, `— (root)` for a root, or the id marked *not in the trace* when the parent is
  missing).
- For a span in error, its **status message** on its own line in red.
- **ATTRIBUTES** as a list of keys and values, as the app sent them; `error.type` and a 5xx
  `http.response.status_code` are red. **RESOURCE**: the service's `service.name`,
  `service.version` and `telemetry.sdk.language`, and the stack (`service.namespace`) and
  deployment generation (`hoster.generation`) hoster gives every container.
- **EVENTS** with their name, when they happened (`@ 1.85 s (+1.80 s in span)`: from the
  trace's start, and from the span's), their attributes, and an exception's
  `exception.stacktrace` as it was sent.
- **LOGS FOR THIS SPAN**: the log records the app sent with this span's id, oldest first (the
  first 50), each with its time, level and message, and each a link to the Logs tab opened on
  that record. A record has a span id when the app sends it over OTLP from inside the span
  (see [[tracing#pick-one-stdout-or-otlp-for-logs|Pick one: stdout or OTLP for logs]]).

The proxy's spans have the request's attributes (method, path, host, status, client address,
the route's prefix, and the stack and service called), a resource of `service.name=hoster`,
and no events.

# Limits, and what a drop looks like

Each service may send, sustained, **1000 log records and 1000 spans a second**, with bursts of
up to 5 seconds' worth. An SDK's batch exporter, which sends every few seconds in batches of up
to 512, stays inside this as long as its average does. What is over a cap is dropped and shows
in three places:

- **The export's answer** is a 200 with OTLP `partial_success`: the number rejected and
  `1200 dropped: over this service's rate cap of 1000 a second`. An SDK does not retry what was
  rejected this way; some log it.
- **A marker record** in the service's logs, at `warn` with the stream `otlp`, where the drops
  happened: `1200 spans dropped: more than 1000 spans a second over OTLP`. There is one per run
  of drops, and at most one a minute while the overload lasts.
- **The System page's Log store card** has an **OTLP rate cap** row with the records and spans
  each service lost since hoster started.

If the log store itself falls behind, an export is answered the same way with
`… dropped: the log store is busy`, and the Log store card counts it under **channel full**.

A service also has at most **4 export requests** in progress at once; the next is answered 429
with `Retry-After: 1`, and the SDK retries it. SDKs send one batch per signal at a time, so this
is reached only when several processes in one container export at once.

A 401 is not retried by SDKs: what the app sent is lost, and the SDK usually says so on stderr,
which hoster keeps as the container's own log lines.

# Pick one: stdout or OTLP for logs

An app that prints its logs on stdout and also exports them over OTLP is stored twice: each
event once with the stream `stdout` and once with `otlp`, twice the volume against the 1 GiB
cap. Choose one per app:

- **Spans and logs over OTLP.** Install the SDK's log bridge and stop printing log lines. The
  records carry their trace and span ids.
- **Spans over OTLP, logs on stdout.** Export spans only and keep JSON lines on stdout
  ([[logging]]). Nothing is stored twice; the lines carry no trace id unless the app adds one.
- **Logs on stdout only.** No SDK, or export turned off. Each request still has the proxy's
  two spans.

hoster does not remove duplicates for you: the two copies come by different paths and cannot be
matched reliably.

# Examples

## Rust with `tracing`

`tracing-opentelemetry` turns `tracing` spans into OpenTelemetry spans, and
`opentelemetry-appender-tracing` turns `tracing` events into OpenTelemetry log records. These
versions were current when this page was written; the OpenTelemetry crates are 0.x and change
their API between minor releases, so keep `opentelemetry`, `opentelemetry_sdk`,
`opentelemetry-otlp`, `opentelemetry-http` and `opentelemetry-appender-tracing` on the same
version, with the `tracing-opentelemetry` release made for it.

```sh
cargo add axum tracing opentelemetry@0.33 opentelemetry_sdk@0.33 opentelemetry-http@0.33 \
    opentelemetry-appender-tracing@0.33 tracing-opentelemetry@0.34
cargo add opentelemetry-otlp@0.33 --no-default-features \
    --features http-proto,reqwest-blocking-client,trace,logs
cargo add tokio --features rt-multi-thread,net,signal
cargo add tower-http --features trace
cargo add tracing-subscriber --features env-filter,json
```

The app below exports spans and log records over OTLP when an endpoint is set, as it is under
hoster, and prints JSON lines when it is not, as on a laptop: one or the other, never both. The
exporters, the propagator and the resource are all set up from the `OTEL_*` variables; nothing
is hard-coded.

```rust
use std::error::Error;

use axum::{Router, extract::Request, routing::get};
use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_http::HeaderExtractor;
use opentelemetry_otlp::{LogExporter, SpanExporter};
use opentelemetry_sdk::{
    Resource, logs::SdkLoggerProvider, propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};
use tokio::signal::unix::{SignalKind, signal};
use tower_http::trace::TraceLayer;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::{EnvFilter, Layer as _, prelude::*};

/// The OpenTelemetry providers, when the environment configures an exporter.
struct Telemetry {
    tracer: SdkTracerProvider,
    logger: SdkLoggerProvider,
}

impl Telemetry {
    /// Exporters set up from the standard `OTEL_*` variables, or `None` when there is no
    /// endpoint (a local run) or `OTEL_SDK_DISABLED=true`, which the Rust SDK does not read.
    fn from_env() -> Result<Option<Self>, Box<dyn Error>> {
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_default();
        let disabled = std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v == "true");
        if endpoint.is_empty() || disabled {
            return Ok(None);
        }
        // Read an incoming `traceparent` (and write one on outgoing requests).
        global::set_text_map_propagator(TraceContextPropagator::new());
        // service.name from OTEL_SERVICE_NAME, the rest from OTEL_RESOURCE_ATTRIBUTES.
        let resource = Resource::builder().build();
        // Endpoint, headers and protocol come from OTEL_EXPORTER_OTLP_*.
        let tracer = SdkTracerProvider::builder()
            .with_batch_exporter(SpanExporter::builder().with_http().build()?)
            .with_resource(resource.clone())
            .build();
        let logger = SdkLoggerProvider::builder()
            .with_batch_exporter(LogExporter::builder().with_http().build()?)
            .with_resource(resource)
            .build();
        Ok(Some(Self { tracer, logger }))
    }

    /// Spans and log records over OTLP, or JSON lines on stdout without an exporter: one or
    /// the other, never both.
    fn install(telemetry: Option<&Self>) -> Result<(), Box<dyn Error>> {
        let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let registry = tracing_subscriber::registry();
        match telemetry {
            Some(telemetry) => {
                // The exporter's own HTTP client must not export its events.
                let mut logs = filter();
                for quiet in ["hyper", "reqwest", "h2", "opentelemetry"] {
                    logs = logs.add_directive(format!("{quiet}=off").parse()?);
                }
                let tracer = telemetry.tracer.tracer("my-app");
                registry
                    .with(tracing_opentelemetry::layer().with_tracer(tracer).with_filter(filter()))
                    .with(OpenTelemetryTracingBridge::new(&telemetry.logger).with_filter(logs))
                    .try_init()?;
            }
            None => registry
                .with(tracing_subscriber::fmt::layer().json().with_filter(filter()))
                .try_init()?,
        }
        Ok(())
    }

    /// Exports what is still queued.
    fn shutdown(self) {
        let _ = self.tracer.shutdown();
        let _ = self.logger.shutdown();
    }
}

/// One span per request, continuing the `traceparent` hoster's proxy sends.
fn request_span(request: &Request) -> tracing::Span {
    let span = tracing::info_span!(
        "request",
        otel.name = format!("{} {}", request.method(), request.uri().path()),
        otel.kind = "server",
        http.request.method = %request.method(),
        url.path = request.uri().path(),
    );
    let parent = global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(request.headers())));
    let _ = span.set_parent(parent);
    span
}

async fn hello() -> &'static str {
    tracing::info!("saying hello");
    "hello\n"
}

async fn serve() -> Result<(), Box<dyn Error>> {
    let app = Router::new()
        .route("/", get(hello))
        .layer(TraceLayer::new_for_http().make_span_with(request_span));
    let port: u16 = std::env::var("PORT").unwrap_or_else(|_| "8080".into()).parse()?;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    let mut sigterm = signal(SignalKind::terminate())?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            sigterm.recv().await;
        })
        .await?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    // The exporters use a blocking HTTP client on their own threads: set them up, and shut
    // them down, outside the async runtime.
    let telemetry = Telemetry::from_env()?;
    Telemetry::install(telemetry.as_ref())?;
    let result = tokio::runtime::Runtime::new()?.block_on(serve());
    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }
    result
}
```

What each part is for:

- **`request_span`** is what makes the app's spans nest under the proxy's. The propagator
  reads `traceparent` from the request's headers, and `set_parent` makes it the parent of the
  request's span. Every span and log event made while handling the request is inside that
  span. Without these lines the app's spans start traces of their own.
- **The blocking client** is the exporters' default HTTP client. It runs on the SDK's own
  threads, but it may not be created or dropped inside a Tokio runtime, so `main` builds the
  providers before the runtime and shuts them down after it, rather than using `#[tokio::main]`.
- **The shutdown on `SIGTERM`** exports what is still queued when hoster stops the container
  during a deployment. Without it the last few seconds of spans are lost.
- **The filter on the log bridge** keeps the exporter's own HTTP client from exporting its
  events, which would feed back into the exporter when `RUST_LOG` is set to `debug`.
- **Spans only:** to keep logs on stdout, replace the `OpenTelemetryTracingBridge` layer with
  the `fmt` JSON layer and drop the logger provider.

Set the level with `RUST_LOG` in the service's `env`, as for JSON lines.

## aphid-server

[aphid-server](https://aphid.lhelge.se/wiki/aphid-server/), the static-file server of
[[static-sites]], exports traces and logs over OTLP/HTTP from version 0.4.0 whenever
`OTEL_EXPORTER_OTLP_ENDPOINT` (or its `--otlp-endpoint` flag) is set. hoster sets it, so under hoster it
exports without any setting, once its container has been replaced. It reads the token and the
resource attributes from the other `OTEL_*` variables. For each request except `/healthz` it
exports:

- a server span named after the method (`GET`), continuing the proxy's `traceparent`, with the
  path, query, status and response size as attributes and an error status for a 5xx;
- one `info` log record, its access log line, inside that span.

By default aphid-server prints its log lines on stdout as well, so with export on every
access log line is stored twice. Choose one in the stack file. From version 0.5.1,
`APHID_LOG_FORMAT = "none"` stops the lines on stdout, so spans and logs come over OTLP only:

```toml
[services.docs.env]
APHID_LOG_FORMAT = "none"
```

Use it only on a hoster that has the receiver: with nothing on stdout and no export, the
service has no logs at all. To keep JSON lines on stdout and export nothing instead (no spans
either), which is also the only choice before 0.5.1:

```toml
[services.docs.env]
APHID_LOG_FORMAT = "json"
OTEL_EXPORTER_OTLP_ENDPOINT = ""
```

## nginx

nginx's OpenTelemetry module exports over gRPC only, which hoster does not accept. Requests to an
nginx service still have the proxy's two spans, and its access log lines are kept as text
([[logging]]).

# Why it works this way

The reasons, and the alternatives that were turned down (a dedicated port, gRPC, metrics,
sampling, honouring a client's `traceparent`), are in
[ADR-0069](https://github.com/LHelge/hoster/blob/main/docs/adr/0069-otlp-receiver-and-edge-trace-context.md).
