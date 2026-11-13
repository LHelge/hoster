# ADR-0069: hoster receives OTLP logs and traces from containers on the proxy's port 80, and the proxy starts a trace for every request

- Status: Accepted (resolves OQ-42; amends ADR-0010, ADR-0018, ADR-0029, ADR-0032, ADR-0045, ADR-0051, ADR-0067 and ADR-0068)
- Date: 2026-11-13

## Context

JSON on stdout gives logs (ADR-0067). It cannot give a trace waterfall: that needs spans with a
start, an end, a parent and a shared trace id, which apps send through an OpenTelemetry SDK
over OTLP. The owner's Rust apps can (`tracing-opentelemetry`, `opentelemetry-appender-tracing`;
`aphid-server` 0.4.1 already does), most languages have SDKs, and most third-party images send
nothing. hoster had no receiver. The trace pages of `docs/design/Telemetry.dc.html` (epic `qnh`)
are built on one.

Spike `h4q` (`docs/spikes/container-to-host-listener.md`, 2026-11-13) found how a container
reaches the host under rootless podman with pasta:

- `host.containers.internal` is `169.254.1.2` in every container's `/etc/hosts`, and pasta
  delivers a connection to it to the host's interface address, with that same address as the
  peer. It reaches listeners on the wildcard address, the proxy's `:80` among them, and never
  ones on `127.0.0.1`.
- A container cannot reach the host by the host's public address, so a public route does not
  hairpin (proposed task `kf3`).

The owner decided the points marked *(owner)* on 2026-11-12 and 2026-11-13 (task `yjp`). The
rest is the design that follows from them.

## Decision

**1. hoster has an OTLP receiver** *(owner)*. It accepts what containers push, stores it in the
log store (ADR-0067) and shows it in the UI. It is a receiver with storage, not an
OpenTelemetry Collector: no pipelines, no processors, no export.

**2. Protocol and signals** *(owner)*. OTLP/HTTP with protobuf bodies only: `POST /v1/logs` and
`POST /v1/traces`, gzip accepted. No gRPC and no JSON encoding. Logs and traces only;
`/v1/metrics` answers 404.

**3. The path from a container** *(owner)*. The receiver is served by the proxy's existing
`:80` listener, as `http://host.containers.internal`. The proxy hands a request to the
receiver when both hold:

- its `Host` is `host.containers.internal`, and
- its peer address is one of the host's own addresses (an IPv4-mapped IPv6 peer is unmapped
  first).

A request with that `Host` from any other peer gets the 404 of an unknown host (ADR-0032). The
route is plain HTTP and is matched before the redirect to HTTPS. No port is opened, no
certificate is issued, and nothing host-wide changes. `host.containers.internal` is reserved:
a stack file cannot route it. `hoster doctor` checks that a container resolves the name to
`169.254.1.2` and reaches the receiver.

The peer check keeps outside clients out. It is not identity: every container, every process
on the host and other users' containers pass it.

**4. Identity** *(owner)*. Each service has a bearer token that names it:
`<stack>.<service>.<mac>`, where the MAC is computed over the stack and service names with a
key derived from the secret store's key (ADR-0029) for this purpose alone. Nothing is stored.
The receiver recomputes the MAC, compares in constant time, and requires that the stack and
service exist. Stack and service come from the token; what the payload claims (`service.name`
and other resource attributes) is kept for display and never decides where data is stored. A
token is the same for every generation of a service. One service's token cannot be rotated.

**5. Injected variables** *(owner)*. Every service gets, unless its stack file sets the same
name as `env` or a secret:

| Variable | Value |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://host.containers.internal` |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` |
| `OTEL_METRICS_EXPORTER` | `none` |
| `OTEL_SERVICE_NAME` | the service's name |
| `OTEL_EXPORTER_OTLP_HEADERS` | `Authorization=Bearer%20<token>` |
| `OTEL_RESOURCE_ATTRIBUTES` | the stack and the deployment generation |

The first four are part of the service's environment as `PORT` is (ADR-0051) and of its config
hash (ADR-0029). The last two are added when the container is created and are not hashed: the
generation differs for every deployment, and the token follows from names the hash already
covers. The UI shows the token masked, as a secret.

**So every service's config hash changes once** *(owner)*. The first deployment of a stack
after the upgrade replaces all its services, by their update strategy, even where no image or
stack file changed. Nothing redeploys by itself, and a container exports only once it has been
replaced. The release notes say so.

**6. Storage.** A `spans` table in `logs.db`: trace id, span id, parent span id, stack,
service, name, kind, start and duration in µs, status, attributes, resource and events as
JSON, and the time received. OTLP log records go into `log_records` with stream `otlp`, their
attributes as fields, their trace and span ids filled, and severity mapped onto the five
levels as for JSON lines. Span and record times are the ones the payload gives. One recorder
writes everything (ADR-0067 §6); the receiver never waits for the store.

**7. Retention and volume** *(owner)*. Spans are kept **3 days**; OTLP log records 7 days like
other records; the 1 GiB cap covers all tables. No sampling. Each service has rate caps for
spans and for records per second, a cap on concurrent requests and a body size limit, all
constants. What is over a rate cap is dropped, reported as OTLP `partial_success` and marked
by one record per gap; over the concurrency cap the answer is 429 with `Retry-After`.

**8. Trace context at the edge** *(owner)*. The proxy starts a trace for every request it
routes to a stack. The trace id is the request id (ADR-0067 §5), which already has the shape.
A `traceparent` and `tracestate` sent by the client are removed, and the proxy sends its own
`traceparent` upstream, sampled, with its upstream span as the parent. hoster's own records
made inside a request (ADR-0068) carry the trace id.

**9. The proxy's spans** *(owner)*. Every request has two: a server span for the request and a
client span for the upstream request. They are not stored as spans. The request row gains the
two span ids and the time the upstream request started, and the spans are built from the row
when a trace is read. An app that sends nothing still has a 2-span trace.

## Alternatives considered

- **No receiver**: stdout JSON only, correlated by a request id header. No waterfall.
- **Documenting Jaeger or OpenObserve as a stack**: needs the same path from containers, and
  its UI sits outside hoster's.
- **A dedicated receiver port** reached as `host.containers.internal:<port>`: works, but the
  listener cannot be on loopback (binding to the `lo` device was refused too), so the port is
  open on the public address and wants a firewall rule hoster cannot make without root.
- **A public HTTPS route through the proxy**: refused from a container by public DNS. Pinned
  to `169.254.1.2` with an extra-hosts entry it works, at the cost of a hostname, a
  certificate, a CA bundle in every image and a TLS handshake per connection.
- **A Unix socket mounted into the container**: `OTEL_EXPORTER_OTLP_ENDPOINT` allows only
  `http` and `https`, so it cannot be configured by environment, and it needs a bind mount
  (ADR-0019).
- **pasta's `--map-host-loopback`** to reach a loopback listener: opens every `127.0.0.1`
  port, other stacks' upstream ports included, to every container.
- **Opt-in per service** (the agent's recommendation): no hash change; the owner chose every
  service so that an app with an SDK exports without stack-file changes.
- **Injected variables outside the config hash** (the agent's recommendation): nothing
  redeploys, but a service exports only after its next deployment for another reason, and the
  hash no longer covers the whole environment.
- **A random token per service in the secret store**: can be rotated per service; needs a
  table, a lookup cache and removal with the stack.
- **gRPC as well**: nginx's OTel module exports gRPC only, so nginx cannot send to hoster. A
  second protocol stack for one third-party image.
- **Metrics**: overlap ADR-0062 and ADR-0066, which measure from outside, and need series caps.
- **Honouring a client's `traceparent`**: lets a client choose trace ids and join traces it
  does not own.
- **Head sampling**: loses the trace someone is looking for; the rate caps bound volume instead.
- **Proxy spans as rows in `spans`**: one shape for every span, three writes per request
  instead of one. **No proxy spans**: uninstrumented apps would have no traces.

## Consequences

- hoster now knows one protocol beyond HTTP routing (ADR-0018): it decodes OTLP. It still knows
  nothing about particular apps.
- Containers have had a path to wildcard listeners on the host all along; this ADR uses it
  and adds none. ADR-0046 is unchanged: stacks get no path to each other.
- Every container reaches the proxy from the host's own address, so all exports share that
  address's 256 connections (ADR-0065). Exporters keep a connection or two each, so this
  allows over a hundred services; the cap is not changed.
- Receiver requests are not routed to a stack, so they write no request rows and no traffic
  buckets.
- Podman 5.4 with passt 2024_08_14 or later is required for the name to resolve; every
  distribution with podman 5.4 that was checked has it. Stack networks are IPv4 only, so a
  host without an IPv4 address cannot receive.
- An app with an OTel SDK that reads the environment starts exporting after its next
  deployment without being asked. One that logs on stdout and also exports logs is stored
  twice, once per stream; the guide has to say to pick one.
- Third-party images ignore the variables. The token is visible in `podman inspect` to the
  `hoster` user, as secrets are (ADR-0029).
- A leaked token lets any local process write logs and spans as that service until the secret
  store's key changes, which changes every token.
- Not yet shown, and for the build tasks to show: that outside clients fail the peer check
  (reasoned from the kernel dropping outside packets with a local source; not tested from a
  second machine), and that Pingora can match the route before the HTTPS redirect. If either
  fails, the path is a new question, with the dedicated port as the first alternative.
- The request row grows by two span ids and a time; ADR-0010's forwarded headers gain
  `traceparent`.
