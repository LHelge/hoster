---
id: asu
title: "Trace views: light-theme contrast of listed scatter dots and forced-colors rules for the waterfall"
status: proposed
priority: P3
created: "2026-11-17T20:11:02.462046695Z"
updated: "2026-11-17T20:11:02.482825404Z"
tags:
  - web
  - observability
  - charts
---

## Summary
Two small design gaps noted by the implementers of epic `qnh` on 2026-11-17, not checked by the owner yet:
- In the light theme (Latte), the listed traces of the duration scatter look alike whether ok or in error: the dark teal and dark red fills sit inside a purple accent ring (`scatter-listed`, `scatter-ok`, `scatter-error` in `assets/hoster.css`).
- The waterfall has no `forced-colors` rules, while other charts have some; service colours, the error ring and the critical-path dimming may be lost in Windows high-contrast mode.

## Acceptance Criteria
- [ ] Listed ok and error dots are told apart at a glance in both themes
- [ ] The waterfall is readable under `forced-colors: active` (bars, error mark, selected row)

## References
- ADR-0054, `docs/design/README.md` (Telemetry notes)
- `assets/hoster.css`, `src/web/charts/scatter.rs`