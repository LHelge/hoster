---
id: qq8
title: "OCI registry client: token discovery and tag→digest resolution"
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.339259844Z"
updated: "2026-11-07T23:20:45.280161461Z"
tags:
  - registry
depends_on:
  - p7a
  - rhq
parent: "5ch"
attempts: 1
---

## Summary
Generic OCI distribution client (ADR-0025): resolve `repo:tag` to a digest on any registry (ghcr verified in architecture §1.4; Docker Hub uses a separate token server discovered via `WWW-Authenticate`).

## Acceptance Criteria
- [ ] Token: parse the `WWW-Authenticate: Bearer realm=…,service=…,scope=…` challenge and request a token (anonymous or basic auth with the registry host's credentials); works for ghcr.io and docker.io
- [ ] HEAD `/v2/<repo>/manifests/<tag>` with Accept for OCI index, Docker manifest list, OCI manifest, Docker v2 manifest; return `Docker-Content-Digest`
- [ ] Token cached until expiry; 401 → refresh once; 429 → typed RateLimited with retry-after
- [ ] reqwest with rustls (ring)
- [ ] Test (manually) whether ghcr accepts a fine-grained PAT for pulls; record the result in ADR-0025 / docs
- [ ] Test (manually) whether ghcr accepts a GitHub App installation token with `packages: read` for token exchange and pulls; record the result in ADR-0025's correction section (currently assumed unsupported)

## Testing
- wiremock tests for token + HEAD + 401 challenge + 429
- Gated live tests against a public ghcr image and a public Docker Hub image
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §1.4
- ADR-0005