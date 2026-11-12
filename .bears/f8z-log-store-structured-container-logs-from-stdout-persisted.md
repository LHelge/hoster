---
id: f8z
title: "Log store: structured container logs from stdout, persisted"
type: epic
status: done
priority: P3
created: "2026-11-12T13:13:16.805209672Z"
updated: "2026-11-12T22:20:11.627824447Z"
tags:
  - observability
  - runtime
  - store
---

From the telemetry investigation of 2026-11-12. OQ-41 was decided on 2026-11-12 as ADR-0067, which is the specification for this epic, amended the same day by ADR-0068 (hoster's own events at info and above). Broken down on 2026-11-12.

## What

hoster follows the stdout/stderr stream of every managed container, parses each line (a JSON object is mapped to level, message, fields; any other line is kept whole) and persists the records, so logs survive container cleanup and can be queried by stack, service, time, level and text. No UI in this epic beyond what proves it works; the stack logs UI is epic `psq`.

Parts (ADR-0067 and ADR-0068 have the details):

- A log follower per running managed container (bollard `logs` with follow), started and stopped with the container's generation, resuming from a persisted cursor after a hoster restart (podman's size-capped `k8s-file` stays as the buffer, ADR-0044).
- hoster's own `tracing` events at `info` and above, as service `hoster` (ADR-0068).
- The line parser: `tracing-subscriber` JSON and the common key spellings of other loggers; deploy generation and stream recorded per line.
- The store: a separate `logs.db` with its own reversible migrations, batched writes, a query API (range, stack, service, level, substring, paging) for the UI.
- Housekeeping: retention by age, a total size cap, a per-container rate cap, its disk use shown on the System page, a `doctor` check, and what `hoster backup db` does with it.
- Docs: a wiki page on logging from an app (JSON on stdout, the keys hoster reads), linked from the stack-format guide.

## Why

Logs are gone when a generation is cleaned up, cannot be searched, and have no levels. The owner's apps already log through `tracing`; its JSON formatter needs no SDK and no network path, and works for every container. This store is also where the edge request log (`usf`) and OTLP data (`xv9`) land.

## Size and shape

Large. The follower lifecycle (deploys, restarts, crash loops, hoster restarts without duplicates or gaps) is the hard part; the store follows ADR-0062's recorder pattern.

## Tasks

`7ne` (logs.db, schema) and `azv` (line parser) first, in either order; `r8w` (recorder) and `gh4` (query API) after `7ne`; `7n3` (followers) after `azv` and `r8w`, then `tuh` (resume after restart); `5d4` (retention) and `fsw` (hoster's own events) after `r8w`; `uw9` (System page, doctor) and `s99` (docs) last.

References: ADR-0067, ADR-0068, ADR-0044, ADR-0045, ADR-0022, ADR-0062, ADR-0030, `src/runtime/podman.rs` (`ContainerLog`), `src/web/containers.rs`, `src/store/`, `src/stats/`.