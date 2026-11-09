---
id: ggf
title: "Registry: tag listing and version-line matching"
status: open
priority: P2
created: "2026-11-09T17:22:16.232195833Z"
updated: "2026-11-09T17:22:16.232195833Z"
tags:
  - registry
depends_on:
  - "7ah"
parent: "6z3"
---

## Summary
Give the generic OCI registry client the two operations update detection needs, and model version tags as a type.

## Acceptance Criteria
- [ ] `RegistryClient` lists a repository's tags (`GET /v2/<repo>/tags/list`, following `Link: <…>; rel="next"` pagination, capped at a sane maximum, e.g. 10k tags) and resolves a tag's digest with a HEAD manifest request. Accept headers cover OCI index / manifest and Docker manifest list / v2. Both use the existing challenge-driven token flow and per-registry credentials.
- [ ] A 429 is surfaced as a distinct error carrying `Retry-After` when present, so the checker can back off per registry.
- [ ] A `TagVersion` type (`FromStr`, `Ord`, `Display`) per the ADR: optional `v`, 1–3 numeric components, optional suffix; pre-releases flagged. It answers `same_line(&other)` and `bump_kind(&other) -> Option<Patch | Minor | Major>`.
- [ ] `TagVersion::newest_in_line(current, tags, max_bump)` picks the candidate. Ties and odd tags (`1.2.3.4`, `latest`, `16-alpine3.20`) behave as the ADR says.

## Testing
- `src/registry/client_tests.rs` with wiremock: pagination, a HEAD digest, a 429 with and without `Retry-After`, a Docker Hub-style token server.
- `TagVersion` table tests: postgres-style (`16`, `16.4`, `16.4-alpine`), `v`-prefixed semver, valkey `8.0.1`, a pre-release, an unparseable tag.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M14 ADR (task 7ah); ADR-0025
- `src/registry/client.rs`, `src/registry/client_tests.rs`, `src/stack/image.rs`