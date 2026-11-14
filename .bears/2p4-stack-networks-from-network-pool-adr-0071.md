---
id: "2p4"
title: Stack networks from network_pool (ADR-0071)
status: open
priority: P3
created: "2026-11-14T22:07:36.747541943Z"
updated: "2026-11-14T22:07:36.747541943Z"
tags:
  - runtime
  - config
  - docs
---

## Summary
Implement ADR-0071: every new stack network takes a free /24 from hoster.toml's `network_pool`, so apps can trust one documented range for forwarded headers instead of `10.0.0.0/8`.

## Acceptance Criteria
- [ ] `network_pool` in `Config` (top level, like `upstream_ports`), `HOSTER_NETWORK_POOL`, default `10.89.0.0/16`; startup refuses a range outside RFC 1918 or smaller than a /24; shown in Settings' effective configuration
- [ ] `ensure_network` (`src/runtime/podman.rs`) lists podman's networks and creates `hoster-<stack>` with the lowest /24 of the pool that overlaps none of them (bollard `Ipam` config); a full pool is an error naming `network_pool`; an existing network is left as it is
- [ ] The fake runtime (`src/runtime/fake.rs`) records the subnet, and tests cover allocation, skipping taken /24s (hoster's and foreign), a full pool and an existing network
- [ ] `hoster doctor` advises on a stack network outside the pool (remove and apply the stack again to move it; volumes are kept)
- [ ] Verified on a real podman (rootless, pasta) which address a routed container sees hoster's connections come from, and that it is inside the stack's /24; recorded in architecture §8
- [ ] Docs: `site/content/wiki/install.md` (config key), `site/content/wiki/stack-format.md` near the forwarding-headers table ("trust `network_pool`", with an nginx `set_real_ip_from` example); `aphid build` passes

## References
- ADR-0071, ADR-0006, ADR-0024, ADR-0046, `src/config.rs`, `src/runtime/podman.rs` (`ensure_network`), `src/runtime/fake.rs`, `src/doctor/`, `docs/architecture.md` §8