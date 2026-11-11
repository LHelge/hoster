# Spike: counting open connections per client address in Pingora 0.9.1

- Task: Bears `v5v`, for ADR-0065's per-address cap (built in `4vg`)
- Date: 2026-11-12
- Prototype and proof: [`src/proxy/tests/address_slots.rs`](../../src/proxy/tests/address_slots.rs)
  (test-only; `cargo nextest run -E 'test(address_slots)'`). It stays in the tree so a Pingora
  upgrade that breaks either hook fails CI.
- **Recommendation: build it on the L4 stream's `tracer`**, attached by a pre-TLS callback on
  `:443` and by a thin `ServerApp` wrapper on `:80`. A `ServerApp` wrapper alone, as ADR-0065
  sketched, misses connections that are still in or fail their TLS handshake. This note is
  input for `4vg`, not a decision; ADR-0065 already allows "that design, or an equivalent".

## Question

`ConnectionFilter::should_accept` is asked at accept and has no callback when a connection
closes. Can hoster count each connection exactly once, per client address, and release it on
every end: HTTP/1.1 keep-alive and HTTP/2, TLS and plain, errors, aborts and shutdown?

## How Pingora 0.9.1 runs a connection

`Service::run_endpoint` (`services/listening.rs`) accepts in a loop. The filter runs inside
`accept()` (`listeners/l4.rs`); a refused stream is dropped there. An accepted one gets its own
task:

```text
accept()  ── filter ──▶ spawn {
                          timeout(60 s, handshake())      // pre-TLS callback, then TLS (on :443)
                            Ok  → handle_event: loop { app.process_new(stream) } while Some
                            Err → log "Downstream handshake error", drop the stream
                        }
```

So the app, and any `ServerApp` wrapper around it, first sees a TLS connection **after** its
handshake. A client stalling in the handshake holds the connection for up to 60 s, and one whose
handshake fails is dropped, without the wrapper ever seeing either. A wrapper can count only
connections that reached HTTP; the per-address cap would not hold against a handshake flood
from one address, which is exactly the kind of flood ADR-0065 is about.

## Findings

### The L4 stream's `tracer` is a close hook that covers every end

`protocols::l4::stream::Stream` has a public `tracer: Option<Tracer>` field, and its `Drop`
calls `tracer.on_disconnected()`. Pingora sets it only on upstream connections
(`connectors/l4.rs`, from `PeerOptions::tracer`); accepted streams always have `None`. The L4
stream is owned by whatever wraps it (the TLS stream, the HTTP/1.1 session, the h2 connection)
until the connection is gone, so its drop is the connection's end, whatever the reason:

| Case (test) | Slot taken | Released |
|---|---|---|
| HTTP/1.1 keep-alive, 3 requests on `:443` and on `:80` (`a_kept_alive_connection_is_counted_once`) | once per connection | on close |
| HTTP/2, 50 concurrent streams (`an_h2_connection_with_many_streams_is_counted_once`) | once | on close |
| Client gone mid-header (both listeners), or while its response is pending (h1 and h2) (`a_client_that_aborts_mid_request_is_released`) | yes | at once, or when the pending response is written |
| Stalled ClientHello, non-TLS bytes, client rejecting the certificate (`a_failed_tls_handshake_is_released`) | yes, before the handshake | when the handshake fails or the client leaves |
| Graceful and fast shutdown with idle, busy, mid-request and mid-handshake connections (`shutdown_releases_every_slot`) | — | all, by the time the server thread has exited |

In every case the count returns to zero and the per-address map is empty (a key is removed at
zero, so memory is bounded by the open connections, at most the total cap of 4096).

`Tracer` is `Clone` through `Tracing::boxed_clone`. Nothing clones a downstream stream's tracer,
but a clone must not release the slot a second time, so the prototype's clone is inert.

### Attaching the slot

- **`:443`: `PreTlsProcess`.** `Listeners::set_pre_tls_callback` (via `Service::endpoints()`)
  gets `&mut L4Stream` as the first thing the connection's task does, before the TLS handshake.
  It is only called on TLS endpoints.
- **`:80`: a `ServerApp` wrapper.** The plain stream reaches `process_new` as `Box<dyn IO>`
  holding the L4 stream itself, so `IO::into_any` downcasts it back (no allocation) to set the
  tracer, and the box goes on to the inner app. It is idempotent (a reused keep-alive stream
  already has its slot), so the wrapper does not need to own the keep-alive loop. hoster's `:80`
  service is then `Service::new(name, Claiming { app: http_proxy(conf, inner), .. })` instead of
  `ProxyServiceBuilder::build()`, with `server_options` set on the `HttpProxy` directly.

The peer address comes from the stream's socket digest in both places; a connection without
one (not TCP) is not counted, as the filter lets it through.

### Where the count happens, and how far a burst overshoots

The filter checks the count; the slot is taken a moment later in the connection's task. A burst
can therefore get a few connections past the check before their slots are counted. Measured
with every connection already completed in the kernel backlog before the first accept, the
worst case (`a_burst_overshoots_the_cap_by_little`):

| Cap | Burst from one address | Pingora threads | Slots taken (runs) |
|---|---|---|---|
| 64 | 1000 | 2 | 65 (11×), 66 (2×), 67 (2×) |
| 256 | 3000 | 8 | 256 (1×), 257 (7×) |

Counting in the filter instead (a reservation the slot then takes over) would make the check
exact, but a reservation leaks for good whenever the stream is dropped between the filter and
the slot: `accept()` returns an error after the filter when setting `TCP_NODELAY` fails, and
dropped tasks at shutdown never run. A leaked count locks an address out until restart. A
handful over the cap is harmless, since the total cap still bounds everything, so the count
belongs with the slot.

## Design to build (`4vg`)

- `Admission` (ADR-0065's total cap, `src/proxy/admission.rs`) gains the per-address map
  (`Mutex<HashMap<Key, usize>>`, `Key` = IPv4 address or IPv6 /64, IPv4-mapped as IPv4) and a
  `Slot: Tracing` that releases on `on_disconnected` and clones inert.
- `should_accept` refuses when the total is at its cap **or** the address has 256 open, and
  counts either refusal in the same metric.
- `Admission` implements `PreTlsProcess`, set on the `:443` service's endpoints; the `:80`
  service's app is wrapped in a claiming `ServerApp`.
- The tests here move to the real proxy harness (`src/proxy/tests/limits.rs`), plus `4vg`'s
  own: the 257th connection refused while another address connects (bind the client to
  `127.0.0.2`), and two addresses in one /64 sharing a budget.

## Risks

- The tracer is meant for upstream tracing. Using it downstream relies on two facts of 0.9.1:
  Pingora never sets it on accepted streams, and `Drop` calls `on_disconnected`. Both are
  pinned by the tests above. If an upgrade breaks either, nothing else in Pingora's public API
  sees a failed handshake end; that would be the moment to ask Pingora for a close callback on
  `ConnectionFilter` (or carry a patch), with the `ServerApp` wrapper alone as a stopgap that
  caps only connections that reached HTTP.
- One mutex for the map, taken on every accept and close. At the scale of one VPS (the total
  cap is 4096 connections) that is not a bottleneck; it can be sharded if it ever shows up.
