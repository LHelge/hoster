---
id: h4q
title: "Spike: how a container reaches a hoster listener under rootless podman"
status: done
priority: P2
created: "2026-11-12T13:14:07.256779219Z"
updated: "2026-11-13T15:30:09.944180018Z"
tags:
  - spike
  - runtime
parent: xv9
attempts: 1
---

## Summary
Find out how a container on a per-stack network (ADR-0006) can send HTTP to a listener owned by hoster on the host, without host-wide changes and without giving containers a path to anything else on the host. Feeds OQ-42, question 2.

## Acceptance Criteria
- [ ] A write-up in `docs/spikes/` (as `per-address-connections.md`) with what was run, on which podman and network backend versions, and the result of each option
- [ ] Option A tried: a listener on the host reached from a container as `host.containers.internal`. Record which address the name resolves to, which address hoster must bind for it to work (loopback or not), whether other host services become reachable the same way, and whether it behaves the same on the podman versions hoster supports (5.4 or later, ADR-0057) with pasta
- [ ] Option B tried: a route on a hoster-owned host that hairpins through the proxy (ADR-0046), with the cost per export noted (TLS, proxy caps of ADR-0065 counting the host's own address)
- [ ] Option C assessed on paper: a Unix socket mounted into the container, and which OTel SDKs can export OTLP/HTTP to one
- [ ] A recommendation, and OQ-42 updated with the findings

## Implementation Notes
- A throwaway listener and `curl` from a container are enough; no OTLP needed
- Test with a container on a stack network created the way `src/runtime/podman.rs` creates them, not on the default network
- Do not change `install.sh`, sysctls or firewall rules; if an option needs one, that is a finding

## References
- docs/open-questions.md (OQ-42)
- ADR-0002, ADR-0006, ADR-0007, ADR-0019, ADR-0046, ADR-0065
- `src/runtime/podman.rs`, `docs/spikes/`