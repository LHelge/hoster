---
id: "76n"
title: "Machine: host CPU times, available memory and filesystem usage"
status: done
priority: P1
created: "2026-11-12T00:07:43.308823615Z"
updated: "2026-11-12T00:13:32.986031394Z"
tags:
  - observability
parent: qmw
attempts: 1
---

## Summary

Teach `Machine` (`src/machine.rs`) the host readings the resource sampler (ADR-0066) needs. Raw readings only; deltas and percentages are the sampler's job.

## What

- **CPU times** from the aggregate `cpu` line of `/proc/stat`: a small type holding busy and total jiffies (busy = everything but idle and iowait, as ADR-0066 says), with a method giving the busy fraction between two readings (`None` when no time passed or the counters went backwards).
- **Memory in use**: `MemTotal` and `MemAvailable` from `/proc/meminfo`. `Machine::memory()` and the existing `Memory` type are already used by limits and the System page; extend rather than duplicate the parsing.
- **Filesystem usage** of a path via `statvfs` (rustix; add the feature with `cargo add rustix --features fs`): total bytes and bytes available to an unprivileged user. The path is absolute and real (not under the test root); test it against a temp dir.
- Everything is `Option`/unknown on a read or parse failure, as the rest of `Machine` is; tests use `Machine::under` with written fixture files, as the existing ones do.

## Acceptance Criteria

- [ ] The three readings exist on `Machine` (behaviour on types, no free functions) with unit tests, including malformed and missing files
- [ ] No new dependency beyond a rustix feature
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0066, ADR-0059 (§5, for what `Machine` is); `src/machine.rs`.