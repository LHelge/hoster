---
id: mhe
title: "Quiet the logs from scanners: unrouted requests and failed handshakes"
status: done
priority: P2
created: "2026-11-10T14:45:11.167008Z"
updated: "2026-11-11T15:07:53.151991840Z"
tags:
  - proxy
  - logging
  - observability
parent: kyn
attempts: 1
---

## Problem

The owner sees a lot of scanner noise in the journal of a live install (reported 2026-11-10).
Anything on the internet can make hoster write a log line, so the noise hides real problems and
a flood becomes log volume, with journald's rate limit as the only brake. Likely sources:

- **Access lines for requests hoster answers itself.** `AccessRecord::emit`
  (`src/proxy/router.rs`) logs every request at `info` under `hoster::access`: 404 for an unknown
  host or prefix, 421 for a Host that is not the SNI name, and on `:80` every 404 and redirect
  (`src/proxy/plain.rs`). The only filter today is the whole target
  (`RUST_LOG=info,hoster::access=off`), which also drops the lines for real traffic.
- **Pingora's own lines.** `pingora-core` logs `Downstream handshake error from <addr>` at
  `error` for every failed TLS handshake (`services/listening.rs`). With no default certificate
  (ADR-0032), every scanner that connects without SNI, or with an unknown name, fails the
  handshake by design, so each one is an `error` line. Malformed requests are logged by Pingora
  too.

## Do

1. Measure first: on a live install (or with a scanner-like script against the test harness),
   count lines per source over a period and write the numbers in this task. Fix what dominates.
2. Make scanner traffic filterable separately from real traffic and quiet by default. Expected
   shape, to be confirmed by step 1:
   - requests that matched no route (and all of `:80` except challenges that were answered) go
     to their own target (e.g. `hoster::access::unrouted`) at `debug`;
   - Pingora's per-connection handshake and parse failures are lowered or filtered by target in
     the default filter in `src/logging.rs`, without hiding Pingora's real errors (bind
     failures, panics, upstream errors).
3. Keep them countable: the `Traffic` statistics already count unrouted requests, so the UI
   still shows the volume. Failed handshakes are not counted anywhere; add a counter if it is
   cheap, otherwise note it.
4. Document the targets and how to turn the lines back on, next to the existing
   `RUST_LOG=info,hoster::access=off` note.

Don't add fail2ban-style blocking or rate limiting here; a limit is a policy choice that task
`95p` raises as an open question.

## Acceptance

- [ ] The measured numbers are in the task
- [ ] With the default log level, a no-SNI handshake, an unknown-host request and a `:80` probe
      each write no line at `info` or above; a test asserts it for the access lines
- [ ] Proxied requests and real proxy errors are logged as before
- [ ] The log targets are documented

## References

- `src/proxy/router.rs` (`AccessRecord`), `src/proxy/plain.rs`, `src/logging.rs`
  (`ACCESS_TARGET`, the default filter), `src/stats/`
- `docs/adr/0032-proxy-edge-behaviour.md`, `docs/adr/0045-observability-in-ui-only.md`
- Pingora source: `pingora-core-0.9.1/src/services/listening.rs`

## Result (2026-11-11)

**Measured** against the real binary (`hoster run` in the `tests/supervisor.rs` sandbox, default
`RUST_LOG=info`, a certificate seeded for the admin host), 20 of each, lines per event:

| Client behaviour | Lines | Level and target |
|---|---|---|
| `:443`: connect and close (port scan), plain HTTP, TLS without SNI, TLS for an unknown name, TLS for a host without a certificate yet | 1 | `ERROR pingora_core::services::listening` "Downstream handshake error from …" |
| `:443`, TLS session, `h2` without a valid preface | 1 | `ERROR pingora_core::apps` "H2 handshake error" |
| Either listener: part of a request header, then close | 1 | `ERROR pingora_proxy` "Fail to proxy: Downstream …" |
| `:80`: any request that parses (unknown host 404, known host 308, no host) | 1 | `INFO hoster::access` |
| `:443`, TLS session: unknown prefix 404, Host that is not the SNI name 421 | 1 | `INFO hoster::access` |
| Either listener: a request that does not parse (400 from Pingora), `CONNECT` (405); `:80`: connect and close | 0 | — |

So every probe of `:443` without a known name was an `ERROR` line, and every request to `:80`
an `INFO` line. No live install was measured (no access from here).

**Changed.**

- Access lines of requests that matched no route go to `hoster::access::unrouted` at `debug`;
  on `:80` that is everything but an answered ACME challenge.
- Pingora's three client-caused `error` lines above are re-logged at `debug` under
  `hoster::proxy::client` by the `log` bridge (`ClientNoise` in `src/logging.rs`). They are
  recognised by Pingora's target and the start of the message, since Pingora offers no hook
  for the first two; `scanner_traffic_is_logged_at_debug_under_its_own_targets` counts them
  and fails if an upgrade rewords them. `Accept() failed`, upstream and internal errors are
  untouched (unit test, and the `EMFILE` test still waits for Pingora's line).
- **Not done:** failed handshakes are still not counted anywhere. Counting them is cheap in
  the bridge, but showing the count needs a place in `Traffic` and the UI, which is its own
  task.
- Found on the way: the `tests/supervisor.rs` sandbox ran with the default ACME directory, so
  a test that ran for more than a moment registered an account with Let's Encrypt. The
  sandbox now points at a port nothing listens on.
