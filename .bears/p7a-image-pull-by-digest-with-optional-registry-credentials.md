---
id: p7a
title: Image pull by digest with optional registry credentials
status: open
priority: P1
created: "2026-11-07T14:45:17.178714816Z"
updated: "2026-11-07T14:45:17.178714816Z"
tags:
  - runtime
  - registry
depends_on:
  - usu
  - rhq
parent: "5ch"
---

## Summary
Pull images strictly by digest (`repo@sha256:…`) through bollard, with credentials per registry host per ADR-0025 (classic PAT `read:packages` for ghcr; credentials keyed by registry host).

## Acceptance Criteria
- [ ] pull_image(reference with digest) streams progress to tracing at debug level and returns when complete
- [ ] Pull only accepts digest references (`PinnedImage` newtype); tag resolution happens earlier, in the registry client at stack apply
- [ ] `RegistryCredentials` provider trait keyed by registry host (ghcr.io, docker.io, …); X-Registry-Auth passed when present; initial implementation reads 0600 files until the secret store (ADR-0029) exists
- [ ] Already-present image short-circuits (inspect first)

## Implementation Notes
- bollard `create_image` with `from_image` = repo@digest

## Edge Cases & Considerations
- Pull of a digest that doesn't exist → typed NotFound error
- Auth failure → typed error with registry host, without leaking the token in logs

## Testing
- Gated integration test pulling a public ghcr image by digest
- Unit test for reference parsing (tag+digest, digest only, no digest rejected)
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0025, ADR-0025
- ADR-0003