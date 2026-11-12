---
id: s99
title: "Docs: logging from an app, and the log store in the guides"
status: open
priority: P3
created: "2026-11-12T19:34:19.257840877Z"
updated: "2026-11-12T19:34:19.257840877Z"
tags:
  - docs
  - observability
depends_on:
  - "7n3"
  - fsw
parent: f8z
---

## Summary
A wiki page on how an app's logs end up in hoster and how to get levels and fields: JSON on stdout, the keys hoster reads, and what is kept for how long. Update the guides that the log store changes.

## Acceptance Criteria
- [ ] New `site/content/wiki/logging.md`: why JSON lines (no SDK, no network path); the key table of ADR-0067 §3 (level, message, the numeric scales); that text lines are kept whole with no level; the 16 KiB cut; the line-rate cap and its marker; 7 days or 1 GiB, whichever comes first; that hoster's own `info` lines appear as service `hoster` (ADR-0068)
- [ ] Worked examples: a Rust app with `tracing-subscriber`'s JSON formatter (the exact builder calls), `aphid-server` with `APHID_LOG_FORMAT=json`, and one line each for Node (pino) and Go (`slog` JSON handler)
- [ ] A privacy paragraph: whatever an app prints, secrets included, is on disk for 7 days in `logs.db` with the state database's permissions; client addresses and request paths too, once the access log lands
- [ ] `site/content/wiki/stack-format.md` links the page where logs are mentioned; the backup guide says `logs.db` is not backed up and a restored server starts with empty logs (ADR-0067 §9)
- [ ] `aphid build` in `site/` passes (no broken `[[links]]`)

## Implementation Notes
- Verify the `tracing-subscriber` builder calls against the current crate docs, not memory
- Keep the key table in step with the parser task's constants; link the ADR from the page's last paragraph for readers who want the reasons

## References
- ADR-0067, ADR-0068, ADR-0030
- `site/content/wiki/`, `site/content/wiki/stack-format.md`