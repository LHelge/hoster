---
id: dyb
title: "Spike: stack parser as WASM in the browser editor"
status: proposed
priority: P3
created: "2026-11-10T17:52:08.846824817Z"
updated: "2026-11-10T17:52:14.385989284Z"
tags:
  - spike
  - web
  - stack
---

## Summary
Owner idea (2026-11-10, M15 planning): run `Stack::parse`, the diagnostics and a Rust tokenizer in the browser as WASM, so the editor reuses the backend's code with no server round-trip. ADR-0060 deferred it in favour of server-side live diagnostics. This spike measures the cost so the owner can decide.

## Questions to answer
- **Crate split:** can `src/stack/` (with diagnostics and schema) move into a workspace crate depending only on serde, toml and schemars? Which types and calls cross the boundary (routing collisions, secrets, `Counted`)? What changes for CI and the musl release builds?
- **Toolchain:** `wasm32-unknown-unknown` with `wasm-bindgen` (the CLI and crate versions must match) versus hand-written `extern "C"` exports. Build on every `cargo build` (build.rs), or commit the `.wasm` with a drift check in CI?
- **Size:** of the `.wasm`, raw and compressed.
- **CSP:** `'wasm-unsafe-eval'` in `script-src`. Confirm it is the minimum, and that browser support covers our targets.
- **Benefit:** what it gives over x97's debounced server check. Latency, offline use, and a tokenizer in Rust tested with `cargo test`.

## Acceptance Criteria
- [ ] A spike note in `docs/spikes/` with the measurements and a recommendation.
- [ ] If the recommendation is yes, an open question with options in `docs/open-questions.md` and a `decision` task for the owner. Nothing changes in the product.

## References
- ADR-0060 (Alternatives), ADR-0054 (CSP), ADR-0052 (release builds)
- `src/stack/`, `src/web/mod.rs` (`CSP`), `.github/workflows/`