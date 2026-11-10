---
id: mhe
title: "Quiet the logs from scanners: unrouted requests and failed handshakes"
status: open
priority: P2
created: "2026-11-10T14:45:11.167008Z"
updated: "2026-11-10T14:45:11.167008Z"
tags:
  - proxy
  - logging
  - observability
parent: kyn
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