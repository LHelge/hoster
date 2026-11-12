---
id: gv2
title: "Docs: sending traces and logs from an app over OTLP"
status: open
priority: P3
created: "2026-11-12T19:36:46.443048830Z"
updated: "2026-11-12T19:36:46.443048830Z"
tags:
  - docs
  - observability
depends_on:
  - "5up"
parent: xv9
---

## Summary
A wiki page for app authors: what hoster injects, how to make a Rust app send spans and logs with `tracing`, and what hoster keeps.

## Acceptance Criteria
- [ ] New `site/content/wiki/tracing.md` (or a section of the logging page if that reads better): the injected `OTEL_*` variables; that only OTLP/HTTP protobuf is accepted; logs and traces only, no metrics; spans kept 3 days, logs 7, under the shared 1 GiB cap; the per-service caps and what a dropped span looks like
- [ ] A Rust example with `tracing-opentelemetry` and `opentelemetry-appender-tracing` that reads the standard variables, with crate versions current at the time of writing (the crates are 0.x; verify against their docs)
- [ ] `aphid-server` as the worked example: which setting turns its OTLP export on, and what its spans look like in the waterfall
- [ ] How the proxy's `traceparent` makes an app's spans nest under the proxy's (`buj`), and that an app without OTel still has the proxy's spans if the ADR keeps them
- [ ] Links from the logging page and the stack-format guide; `aphid build` in `site/` passes

## References
- OQ-42, the ADR from `yjp`, ADR-0067, ADR-0068
- `site/content/wiki/`