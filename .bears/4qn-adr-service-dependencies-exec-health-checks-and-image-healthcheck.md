---
id: "4qn"
title: "ADR: service dependencies, exec health checks and image HEALTHCHECK"
status: open
priority: P1
created: "2026-11-09T17:21:18.065202598Z"
updated: "2026-11-09T17:21:18.065202598Z"
tags:
  - decision
  - docs
  - adr
parent: qq9
---

## Summary
Write the ADR (next free number) for M11. The owner has decided the direction: `depends_on`, exec checks **and** image HEALTHCHECK. This task writes down the precise semantics below and sends the ADR to `review_task` for the owner to confirm. Where a point below is genuinely open, present options with a recommendation in the ADR rather than deciding silently.

## Points the ADR must settle (proposed answers)
1. **`depends_on` semantics.** A list of service names in the same stack. "Start after these are healthy"; there are no compose-style `condition` variants. Self-dependencies, unknown names and cycles are errors.
2. **Dependencies that are not part of the deployment.** If a dependency is unchanged in this deployment, it is already live, so there is no wait. (Should we check it's currently healthy? Recommend no: the monitor already reports that.)
3. **Interaction with ADR-0048 phases.**
   - A stop-first service **without a previous generation** has nothing to stop, so it is started in the start-first phase. This makes a first deploy of db (stop-first) + app work.
   - Within each phase, services go in topological order, and each one starts only once its in-deployment dependencies have passed their probes. Independent services still start in parallel waves.
   - A start-first dependent of a changing stop-first service runs against the *old* dependency until the commit; that's acceptable and should be documented.
4. **Reconciler (ADR-0028).** When several containers of a stack are down (host reboot), restart them in dependency order. A dependent waits while its dependency is not running/healthy, instead of burning crash-loop budget.
5. **`health.exec`.**
   - Array of strings, run with podman exec in the new container; exit code 0 means healthy.
   - The output tail is recorded in deployment events, truncated, and never includes env.
   - The usual `interval` / `timeout` / `healthy_after` / `start_period` apply.
   - It is mutually exclusive with `http` / `tcp`, and it is now allowed on non-routed services. This amends ADR-0038's "health not allowed on non-routed services".
6. **Image HEALTHCHECK default.**
   - With no `health` table, use the image config's `Healthcheck`: `CMD` maps to exec argv, `CMD-SHELL` to `["/bin/sh", "-c", …]`, and its Interval / Timeout / StartPeriod / Retries map onto ours. `NONE` disables it.
   - Precedence for routed services: recommend image HEALTHCHECK **over** the `GET /healthz` default, because third-party images rarely serve `/healthz`. That amends ADR-0051; owner to confirm.
   - The continuous health monitor uses the same check.
7. **Images without a shell.** `CMD-SHELL` in a distroless image fails; report it clearly.

## Acceptance Criteria
- [ ] `docs/adr/NNNN-….md` written in the template, with alternatives (compose `condition`s, readiness by retrying only, honouring only explicit exec) and consequences.
- [ ] Status lines / Corrections of amended ADRs updated as ADR-README rules allow; index row added to `docs/adr/README.md`.
- [ ] `docs/architecture.md` §5 updated: start waves and the stop-first-without-previous rule.
- [ ] Task sent to review for owner sign-off.

## References
- ADR-0028, ADR-0038, ADR-0048, ADR-0051; `docs/adr/README.md` (amendment rules)
- `docs/architecture.md` §5.1
- `src/orchestrator/engine.rs` (`start_new`, `replace_stop_first`, `probe`)