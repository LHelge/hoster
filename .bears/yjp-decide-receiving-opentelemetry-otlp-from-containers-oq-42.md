---
id: yjp
title: "Decide: receiving OpenTelemetry (OTLP) from containers (OQ-42)"
status: open
priority: P2
created: "2026-11-12T13:14:26.591091139Z"
updated: "2026-11-12T13:14:26.591091139Z"
tags:
  - decision
depends_on:
  - h4q
parent: xv9
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-42 before the OTLP receiver epic is broken down. The first question is whether to build it at all.

## Acceptance Criteria
- [ ] The owner has tried OTel output from one app (`aphid-server` with `OTEL_EXPORTER_OTLP_ENDPOINT` set) against a local Jaeger or Aspire Dashboard, and says whether traces are worth receiving in hoster
- [ ] Owner has chosen an option for each numbered question of OQ-42 (docs/open-questions.md): whether, the container-to-receiver path (from spike `h4q`), identity and opt-in, protocol, signals, trace context at the edge, volume limits
- [ ] The decision is recorded as a new ADR in docs/adr/ (amending ADR-0018, 0044, 0045, 0046 as needed) and listed in docs/adr/README.md; a "no" is recorded too
- [ ] OQ-42 is marked resolved in docs/open-questions.md
- [ ] Epic `xv9` is broken down, or cancelled together with `qnh` (trace waterfall) on a "no"; the `traceparent` part of `usf` is updated to match

## Implementation Notes
- An agent may prepare a comparison and a draft ADR, but the choice is the owner's. Do not mark done without explicit owner sign-off
- The Rust OTel crates are 0.x (0.33 on 2026-11-12) and change between releases; verify claims about them against current docs
- Verify whether nginx's OTel module can export over HTTP before relying on the "gRPC only" note in OQ-42
- Documentation-only; no code

## References
- docs/open-questions.md (OQ-42, OQ-41, OQ-38)
- ADR-0018, ADR-0029, ADR-0044, ADR-0045, ADR-0046
- `docs/spikes/` (result of `h4q`)