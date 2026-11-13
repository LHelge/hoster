---
id: u9m
title: "OTLP: verify from a second machine that outside clients cannot reach the receiver"
status: open
priority: P2
created: "2026-11-13T18:41:38.471751462Z"
updated: "2026-11-13T18:41:44.024360676Z"
tags:
  - observability
  - proxy
  - security
  - test
depends_on:
  - "5up"
parent: xv9
---

## Summary
ADR-0069 §3 keeps outside clients out of the OTLP receiver by a peer check on the proxy's `:80`: only a peer with one of the host's own addresses is routed. That outside clients fail it was reasoned (the kernel drops outside packets that claim a local source), not tested: spike `h4q` and task `5up` had one machine. Split from `5up` on 2026-11-13. Needs a host running this branch and a second machine that can reach it; an agent on the development machine cannot do it.

## Acceptance Criteria
- [ ] From a second machine: `POST http://<host's public address>/v1/traces` with `Host: host.containers.internal` gets the unknown host's 404, with and without a valid token
- [ ] From a second machine: a connection attempt that claims one of the host's own addresses as its source (for example with `hping3 -a` or `nping --source-ip`) gets no answer, checked with a packet capture on the host
- [ ] On that host: a container of a deployed stack exports on port 80 itself (the development machine could not bind below 1024) and its spans are stored
- [ ] If the host sits behind anything that connects to hoster from a local address (a tunnel daemon, a local reverse proxy), note that outside requests then pass the peer check and only the token guards the receiver; say so in `site/content/wiki/` if it is not there
- [ ] The result is recorded in `docs/spikes/container-to-host-listener.md`; if an outside client gets through, stop and raise an open question (ADR-0069, Consequences)

## References
- ADR-0069 (§3, Consequences), `docs/spikes/container-to-host-listener.md`
- `src/proxy/receiver.rs`, `src/proxy/plain.rs`