---
id: "77n"
title: "Runtime options: `shm_size`, `tmpfs`, `init`"
status: done
priority: P2
created: "2026-11-09T17:22:16.215170357Z"
updated: "2026-11-10T21:07:28.800165922Z"
tags:
  - stack
  - runtime
depends_on:
  - w2w
parent: "9q5"
attempts: 1
---

## Summary
Implement ADR-0059 §1, §3 and §4 for the three runtime options, end to end: stack format → model → `ContainerSpec` → create config → config hash. Introduce the shared `ByteSize` type here; the limits task (xxb) reuses it for `memory`.

## Acceptance Criteria
- [x] **`ByteSize(u64)`** in `src/stack/` with `FromStr`, `Display` and serde through `FromStr` (ADR-0059 §3):
  - An integer then `B`, `KB`/`MB`/`GB` (10³, 10⁶, 10⁹) or `KiB`/`MiB`/`GiB` (2¹⁰, 2²⁰, 2³⁰). Case-sensitive, no fraction, no space.
  - Errors: a bare integer or Docker's `512m` (both suggest `"512MiB"`), unknown units, and more than i64::MAX bytes.
  - `Display` uses the largest unit that divides the value exactly (GiB > GB > MiB > MB > KiB > KB, else B), and its output parses back to the same bytes.
- [x] **`shm_size: Option<ByteSize>`.** Unset keeps podman's 65536000 bytes. Below 1 MiB is an error.
- [x] **`tmpfs: BTreeMap<path, ByteSize>`.** Each size must be at least 1 MiB. A path is an error (with field path and span) when it:
  - is not absolute and normalised (no `.`/`..` segments, no `//`, no trailing `/`);
  - is `/` or `/dev/shm`, or is `/proc`, `/sys`, `/dev` or lies under one of them;
  - overlaps another mount target of the service: equal to, an ancestor of or below a volume path, another tmpfs path, or a config-file target if M12 has landed.
- [x] **`init: bool`.** `false` is the same as unset. Nothing checks for the binary: a missing catatonit fails at create, which aborts the deploy.
- [x] **Create** (`ContainerSpec` in `src/runtime/mod.rs`, mapping in `src/runtime/podman.rs`), only when set:
  - `ShmSize` set to the bytes;
  - `Tmpfs` as `{path: "rw,nosuid,nodev,mode=1777,size=<bytes>"}`, with no `noexec` and no per-mount options;
  - `Init: true`.
  Unset options stay unset. `Privileged` and `PublishAllPorts` stay pinned to `false`, and none of the never-set fields in architecture §8 appears.
- [x] **Hash** (`ConfigHash::new`, `src/orchestrator/plan.rs`): each key is added only when set (`shm_size` as bytes, `tmpfs` as a sorted path → bytes map, `init` only when `true`), so no existing hash changes. An empty `tmpfs` counts as unset. A change recreates the container through a normal deploy (ADR-0049).
- [x] Verified on live podman:
  - `df /dev/shm` shows `shm_size`;
  - a tmpfs mount has its size and mode 1777, and is writable by a non-root image on an existing directory (`nginx-unprivileged`, `/var/cache/nginx`);
  - PID 1 is `/run/podman-init` with `init = true`.
- [x] stack-format.md §2.2 documents the three fields, `ByteSize` syntax, and with `read_only` which tmpfs podman adds on its own (unsized `/tmp`, `/run`, `/var/tmp`, replaced only at a declared path). It also lists what stays unexpressible (ADR-0059 Alternatives).
- [x] The stack JSON Schema covers the new fields (see Implementation Notes).
- [x] The third-party images guide (`site/content/wiki/third-party-images.md`, task k2v) fills in its `shm_size` / `tmpfs` / `init` compose-mapping row and covers the options in its worked example, where they help.

## Implementation Notes
- `src/stack/raw.rs` (`RawService`, validation pass next to the duration handling), `src/stack/mod.rs` (`Service`). Reuse M12's normalised container-path type if it has landed; otherwise add one here for tmpfs and the volume overlap check. Keep the public API narrow.
- The 1 MiB floor is a field rule, not a `ByteSize` rule: `ByteSize` parses any value up to i64::MAX.
- **Schema:** the stack JSON Schema (`src/stack/schema.rs` via `schemars` on the raw types, `site/static/stack.schema.json` with a drift test) and `hoster stack check` are on main. Give the new raw fields schema attributes (a `ByteSize` string pattern, the tmpfs map) and regenerate. The drift test fails until you do. `hoster stack check` then picks up the new validation through the parser.
- **Editor (M15):** if the editor-help task (qjx) has landed, add snippets for the three options. The field reference follows the schema on its own.

## Edge Cases
- `tmpfs = {}` and `init = false` hash and create exactly like a stack without them.
- A tmpfs on `/tmp` with `read_only = true` replaces podman's unsized `/tmp` only. `/run` and `/var/tmp` stay podman's.
- Mode 1777 is deliberate: without it an existing image directory turns `root:root` and a non-root user cannot write (architecture §1.8).
- `"0B"` parses as a `ByteSize`; the field floor rejects it for `shm_size` and tmpfs, so podman's "0 means default" never applies.

## Testing
- `ByteSize`: round-trip, every unit, the `512m` and bare-integer suggestions, overflow, `Display` picking the largest exact unit.
- Stack snapshots (`src/stack/tests.rs`) for each path rule and overlap; a create-config mapping test in `podman.rs`; config-hash tests, including an unchanged hash for a stack without the new keys.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0059 §1, §3, §4 (and Alternatives for the unexpressible list); ADR-0019, ADR-0049, ADR-0056 (hash only when set)
- `docs/architecture.md` §1.8 (Fields, Tmpfs, Init), §8 (Create-time container config)
- `site/content/wiki/stack-format.md` §2.2
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/orchestrator/plan.rs`
