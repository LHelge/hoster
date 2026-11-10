---
id: "95p"
title: Survey the proxy's downstream limits against hostile clients
status: open
priority: P2
created: "2026-11-10T12:56:33.888508Z"
updated: "2026-11-10T12:56:33.888508Z"
tags:
  - proxy
  - security
  - test
parent: kyn
---

## Problem

`src/proxy` sets per-route upstream timeouts (`router.rs`, the `options.read_timeout` /
`idle_timeout` block), but `ServerConf` in `src/proxy/mod.rs` is mostly Pingora's defaults and
nothing in the module sets downstream header-read timeouts, header size limits, connection caps
or HTTP/2 stream limits. We don't know what Pingora 0.9 does by default in each case. A client
that can pin memory, file descriptors or worker threads in the proxy also stops the orchestrator
and the admin UI.

## Do

Find out, against the real proxy (the harness in `src/proxy/tests.rs`), what happens for each of
these on :443 and on the plain :80 listener (`plain.rs`), and write the result down as a table in
`docs/architecture.md` §9 or §6:

- a client that connects and sends nothing (TCP only, and mid-TLS-handshake);
- a client that sends request headers one byte at a time (slowloris);
- a client that sends a request body very slowly, and one that reads the response very slowly;
- oversized request line, a single oversized header, and very many headers;
- idle keep-alive connections held open;
- many concurrent connections from one address, up to the fd limit (what does the process do at
  `EMFILE`? does the admin UI on its Unix socket still answer?);
- HTTP/2: many concurrent streams, rapid reset (CVE-2023-44487 pattern), CONTINUATION flood,
  oversized HPACK header list, SETTINGS and PING floods.

For each: is it bounded, by what (Pingora default, h2 default, our code, nothing), and with which
value.

Then:

- Where a bound exists, add a test that pins it, so a Pingora upgrade that changes it fails CI.
- Where Pingora offers a setting and the safe value is obvious (e.g. a header-read timeout), set
  it in code and test it.
- Where a bound needs a policy choice (per-address connection caps, rate limiting, a global
  connection limit, new `hoster.toml` keys), do not pick one: add an open question with options
  and a recommendation and create a `decision` task (CLAUDE.md, "Architecture choices are the
  owner's").
- Check the unit's `LimitNOFILE` against the worst case found, and note it.

## Acceptance

- [ ] The table exists, with every row filled in from an observed run, not from reading docs
- [ ] Each existing bound has a test
- [ ] Each unbounded case is either fixed with an obvious setting or raised as an open question

## References

- `src/proxy/mod.rs` (`ServerConf`), `src/proxy/router.rs`, `src/proxy/plain.rs`, `src/proxy/tests.rs`
- `docs/adr/0011-streaming-bodies-and-per-route-timeouts.md`, `docs/adr/0033-protocol-support-v1.md`
- `docs/architecture.md` §1 (Pingora behaviour verified so far), §9
- `deploy/` (the unit file)