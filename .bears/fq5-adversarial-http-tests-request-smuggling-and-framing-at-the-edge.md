---
id: fq5
title: "Adversarial HTTP tests: request smuggling and framing at the edge"
status: open
priority: P2
created: "2026-11-10T12:56:33.900656Z"
updated: "2026-11-10T12:56:33.900656Z"
tags:
  - proxy
  - security
  - test
parent: kyn
---

## Problem

Request smuggling is the most likely serious bug class for an embedded Rust proxy, and Pingora
used as a direct ingress has had advisories for it (fixed by 0.8.0; confirm the exact advisories
and that the 0.9 pin includes the fixes). hoster proxies h1 and h2 downstream to h1 upstream
(ADR-0033) and reuses upstream connections, which is the setup smuggling needs. No test sends
malformed framing through the proxy today.

## Do

Add tests that write raw bytes to the proxy and assert both what the client gets and what the
upstream actually receives (a raw TCP upstream that records bytes, not an HTTP server that
normalises them). For each case the proxy must either reject the request (400, connection
closed) or forward one unambiguous request. It must never leave bytes on a reused upstream
connection that the upstream reads as a second request.

HTTP/1:

- `Content-Length` together with `Transfer-Encoding: chunked`; duplicate and conflicting
  `Content-Length`; `Content-Length` with a sign, whitespace or a list;
- obfuscated `Transfer-Encoding` (`xchunked`, `chunked, identity`, leading/trailing whitespace,
  a tab before the colon, mixed case, repeated header);
- malformed chunk sizes and chunk extensions, a bare LF or bare CR as line terminator, obs-fold
  header continuation, a NUL or CR inside a header value;
- HTTP/1.0 requests with a body, with and without `Connection: keep-alive`;
- `Upgrade` requests: an upgrade the upstream refuses (non-101) followed by pipelined bytes, and
  an `Upgrade` with a body. After a failed upgrade the connection must not be treated as a
  tunnel;
- a pipelined second request after each of the above, to show where it ends up;
- absolute-form request targets and a `Host` header that disagrees with them or with SNI
  (ADR-0032 says 421 on a Host/SNI mismatch), duplicate `Host` headers.

HTTP/2 downgraded to h1 upstream:

- `content-length` that disagrees with the DATA frames;
- `transfer-encoding` and other connection-specific headers in an h2 request;
- CR, LF or NUL in header names and values, and in `:path` / `:authority`;
- `:authority` that disagrees with `host` or with SNI.

Routing (ADR-0009, ADR-0010):

- `..`, `//`, `%2f`, `%2e%2e` and `;` path tricks around a prefix boundary, to show the route
  matched is the route the upstream would resolve, and that `strip` can't be used to reach a
  path outside the prefix;
- client-supplied `X-Forwarded-*` headers are replaced, not appended to or trusted.

A case that turns out exploitable is a bug: fix it in hoster if the fix is ours, report it
upstream if it is Pingora's, and say so in the task.

## Acceptance

- [ ] Every case above is a test that states the expected outcome
- [ ] The Pingora advisories are named in a comment or in `docs/architecture.md` §1 with the
      version that fixed each
- [ ] Any exploitable case is fixed or has its own task

## References

- `src/proxy/router.rs`, `src/proxy/tests.rs`
- `docs/adr/0009-host-and-longest-segment-prefix-routing.md`,
  `docs/adr/0010-prefix-preservation-and-forwarded-headers.md`,
  `docs/adr/0032-proxy-edge-behaviour.md`, `docs/adr/0033-protocol-support-v1.md`
- `docs/architecture.md` §6 (routing), §1 (Pingora behaviour)