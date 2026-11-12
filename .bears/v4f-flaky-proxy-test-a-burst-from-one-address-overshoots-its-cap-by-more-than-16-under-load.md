---
id: v4f
title: "Flaky proxy test: a burst from one address overshoots its cap by more than 16 under load"
status: done
priority: P2
created: "2026-11-12T07:49:19.560895596Z"
updated: "2026-11-12T09:05:48.296942882Z"
tags:
  - proxy
  - ci
  - flaky-test
attempts: 1
---

## What happens

`proxy::tests::limits::a_burst_from_one_address_overshoots_its_cap_by_little` (`src/proxy/tests/limits.rs:659`, from commit "refuse connections beyond 256 per IPv4 address or IPv6 /64", task 4vg) fails intermittently. It opens 1000 connections from 127.0.0.1 and asserts the per-address count ends up in `256..=272`.

Observed:

- CI on `main` at `chore(release): v0.7.0`: **274** counted (run 43176019328, job 131040340364).
- Several full local runs on 2026-11-12: **280** counted. The test passes when run alone, and also passed in some full runs.

## Why

This is by design (ADR-0065, spike v5v). The `ConnectionFilter` checks the address's count at accept, but the slot is taken a moment later in the connection's own task, when the L4 stream's tracer is attached. Every connection accepted in between passes the check. The spike measured at most 3 extra on an idle machine. Under full-suite parallel load, or on a CI runner, the accept loop runs further ahead of the claiming tasks, so the overshoot exceeds the test's +16 margin. The total descriptor cap still bounds it.

## Options

1. **Widen the assertion** to what the design guarantees (for example, `PER_ADDRESS..=PER_ADDRESS + 64`), or bound it by the burst size and the total cap rather than a fixed slack. Cheap, and keeps the design.
2. **Reserve the slot in the filter.** Check and increment atomically at accept, and hand the reservation to the stream's tracer or release it on a refused or failed handshake. This removes the overshoot, but it is the design the spike set aside, so it needs an owner decision (ADR-0065 amendment).
3. Mark the test `#[ignore]`, or run it in its own nextest group serially. This hides the variance rather than fixing it.

Recommendation: option 1 now (the overshoot is documented and bounded), and option 2 only if a real overshoot ever matters.

References: ADR-0065; `docs/architecture.md` §9.1; `src/proxy/tests/limits.rs`; the proxy's `Admission`.