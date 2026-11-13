---
id: kf3
title: Cross-stack calls by public route are refused under pasta (ADR-0046)
status: proposed
priority: P2
created: "2026-11-13T15:30:06.166305767Z"
updated: "2026-11-13T15:30:09.920703408Z"
tags:
  - runtime
  - proxy
  - docs
---

## Summary
ADR-0046 says stack A reaches stack B through B's public HTTPS routes, which hairpin through the proxy. Spike `h4q` (2026-11-13, podman 6.1.4, pasta) found that a container cannot connect to the host's own interface address: pasta copies that address into the rootless network namespace, so the connection ends there and is refused. On a host whose public address is on its interface (most VPS providers), a container resolving another stack's hostname by public DNS therefore gets "connection refused". Only seen with a throwaway listener; not yet reproduced with two real stacks through the proxy.

## Acceptance Criteria
- [ ] Reproduced, or disproved, with two stacks on a running hoster: a container in A requests a route of B by its public hostname
- [ ] If confirmed: an open question with options for the owner (for example an extra-hosts entry per container that pins every hoster-routed hostname to `host-gateway`, i.e. `169.254.1.2`; or documenting it as unsupported) and a `decision` task
- [ ] ADR-0046 gets a dated Correction section once the owner has decided, and `site/content/wiki/` says what works

## Implementation Notes
- Pinning with `ExtraHosts: ["<host>:host-gateway"]` worked in the spike through the compat API. It would have to follow route changes in other stacks, which a container's hosts file cannot do without a recreate
- Every container reaches the proxy from the host's own address, so ADR-0065's 256 connections per address are shared by all of them
- Hosts with a 1:1 mapped public address (AWS, GCP, Azure) were not tested

## References
- `docs/spikes/container-to-host-listener.md` (option B)
- ADR-0046, ADR-0006, ADR-0065
- `src/runtime/podman.rs`