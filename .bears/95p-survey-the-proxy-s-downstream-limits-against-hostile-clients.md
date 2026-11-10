---
id: "95p"
title: Survey the proxy's downstream limits against hostile clients
status: open
priority: P2
created: "2026-11-10T12:56:33.888508Z"
updated: "2026-11-10T14:44:46.284802Z"
tags:
  - proxy
  - security
  - test
parent: kyn
---

## Problem

`src/proxy` sets per-route upstream timeouts (`router.rs`, the `options.read_timeout` /
`idle_timeout` block), but `Proxy::new` in `src/proxy/mod.rs` builds both services without
`HttpServerOptions`, `ServerConf` is mostly Pingora's defaults, and no filter sets a per-session
timeout. A client that can pin memory, file descriptors or worker threads in the proxy also stops
the orchestrator and the admin UI.

## Known from reading Pingora 0.9.1 (not yet observed)

A code review on 2026-11-10 read the defaults out of `pingora-core` 0.9.1. Nothing was run, so
each row still has to be confirmed against the real proxy.

| Case | Default | Where |
|---|---|---|
| HTTP/1.1 connection after its first request | Keep-alive is set to *infinite* (`respect_keepalive` → `set_keepalive(Some(0))`), so the wait for the next request header, and a dribbled one, has no timeout | `protocols/http/v1/server.rs` (`respect_keepalive`, `read_request_buf`) |
| First request header | 60 s per read, not in total; up to 1 MiB (`MAX_HEADER_SIZE`) and 256 headers | `protocols/http/v1/server.rs`, `v1/common.rs` |
| Request body read | 60 s per read | `protocols/http/v1/server.rs` (`read_body`) |
| Idle HTTP/2 connection | `h2_idle_timeout` is `None`: never closed | `apps/mod.rs` (`HttpServerOptions`) |
| Requests per kept-alive connection | `keepalive_request_limit` is `None` | `apps/mod.rs` |
| Client reading a response slowly | Downstream `write_timeout` and `min_send_rate` are `None` | `protocols/http/v1/server.rs`, `v2/server.rs` |
| TLS handshake | 60 s | `services/listening.rs` |
| HTTP/2 | 100 concurrent streams, 64 KiB header list | `protocols/http/v2/server.rs` |
| Connections per address, or in total | No cap; the fd limit is the only bound | — |

The knobs Pingora offers: `HttpServerOptions` (`h2_idle_timeout`, `keepalive_request_limit`)
through the proxy service's `server_options`, and per session `set_keepalive(Some(secs))`,
`set_read_timeout`, `set_write_timeout` and `set_min_send_rate`, which a `ProxyHttp` filter can
call. The built-in error pages already close the connection, so the infinite keep-alive is
reached only after a proxied request.

## Do

Find out, against the real proxy (the harness in `src/proxy/tests.rs`), what happens for each of
these on :443 and on the plain :80 listener (`plain.rs`), and write the result down as a table in
`docs/architecture.md` §9 or §6:

- a client that connects and sends nothing (TCP only, and mid-TLS-handshake);
- a client that sends request headers one byte at a time (slowloris), on a fresh connection and
  on a kept-alive one after a first request;
- a client that sends a request body very slowly, and one that reads the response very slowly
  (does a slow reader hold the upstream's in-flight count, and so a deploy's drain?);
- oversized request line, a single oversized header, and very many headers;
- idle keep-alive connections held open, HTTP/1.1 and HTTP/2;
- many concurrent connections from one address, up to the fd limit (what does the process do at
  `EMFILE`? does the admin UI on its Unix socket still answer?);
- HTTP/2: many concurrent streams, rapid reset (CVE-2023-44487 pattern), CONTINUATION flood,
  oversized HPACK header list, SETTINGS and PING floods.

For each: is it bounded, by what (Pingora default, h2 default, our code, nothing), and with which
value.

Then:

- Where a bound exists, add a test that pins it, so a Pingora upgrade that changes it fails CI.
- Where Pingora offers a setting and the safe value is obvious, set it in code and test it. The
  idle keep-alive timeout for HTTP/1.1 and `h2_idle_timeout` are expected to be in this group.
  Mind the routes with long streams (admin SSE, WebSockets, per-route `read` timeouts of
  ADR-0011): a downstream timeout must not cut a stream the route's own timeouts allow.
- Where a bound needs a policy choice (per-address connection caps, rate limiting, a global
  connection limit, a minimum send rate, new `hoster.toml` keys), do not pick one: add an open
  question with options and a recommendation and create a `decision` task (CLAUDE.md,
  "Architecture choices are the owner's").
- Check the unit's `LimitNOFILE` against the worst case found, and note it.

## Acceptance

- [ ] The table exists, with every row filled in from an observed run, not from reading docs or
      source
- [ ] Each existing bound has a test
- [ ] Each unbounded case is either fixed with an obvious setting or raised as an open question

## References

- `src/proxy/mod.rs` (`Proxy::new`, `ServerConf`), `src/proxy/router.rs`, `src/proxy/plain.rs`,
  `src/proxy/tests.rs`
- `docs/adr/0011-streaming-bodies-and-per-route-timeouts.md`, `docs/adr/0033-protocol-support-v1.md`
- `docs/architecture.md` §1 (Pingora behaviour verified so far), §9
- `deploy/` (the unit file)
- Pingora source: `~/.cargo/registry/src/*/pingora-core-0.9.1/src/` (paths in the table above)