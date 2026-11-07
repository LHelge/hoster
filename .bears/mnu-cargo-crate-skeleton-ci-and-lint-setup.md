---
id: mnu
title: Cargo crate skeleton, CI and lint setup
status: done
priority: P1
created: "2026-11-07T14:45:17.153121669Z"
updated: "2026-11-07T22:11:30.693368167Z"
tags:
  - core
  - ci
parent: "8xd"
attempts: 1
---

## Summary
Create the single `hoster` binary crate with the module layout from docs/architecture.md §10 (empty modules with doc comments), toolchain pinning and GitHub Actions CI.

## Acceptance Criteria
- [ ] `cargo build` produces a `hoster` binary; edition 2024, `rust-version` ≥ 1.85 (Pingora 0.9.1 MSRV) pinned in Cargo.toml and rust-toolchain.toml
- [ ] `Cargo.toml` has `license = "MIT OR Apache-2.0"`; `LICENSE-MIT` and `LICENSE-APACHE` already exist at the repo root and the README states the dual license
- [ ] Modules exist as stubs: config, stack, routing, orchestrator, runtime, store, proxy, acme, registry, web
- [ ] CI workflow runs fmt, clippy (-D warnings), test on push/PR
- [ ] README has a short 'Development' section (build, test, lint) and a 'License' section
- [ ] No dependencies beyond what the skeleton needs (tokio, tracing, anyhow/thiserror, clap)

## Implementation Notes
- Use `clap` derive for subcommands: `run` (default), `doctor`, `backup db` (stubs)
- Error strategy: `thiserror` in library-ish modules, `anyhow` at the binary edge
- Do not add Pingora/bollard yet — later tasks add their deps

## Testing
- A trivial unit test per module stub so CI exercises the test step
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §10
- ADR-0001