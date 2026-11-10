---
id: k8x
title: "Image references: reject a registry port with a sign (`localhost:+80/app:1`)"
status: proposed
priority: P3
created: "2026-11-09T23:46:42.225711696Z"
updated: "2026-11-09T23:46:44.885526316Z"
tags:
  - stack
  - bug
---

## Summary
The image reference parser accepts a registry port written with a leading `+` (`localhost:+80/app:1`), because it parses the port with `u16::from_str`, which allows a sign. Podman would reject such a reference at pull time. The stack JSON Schema (zcu) already rejects it, so the parser and schema disagree on this one input.

## Acceptance Criteria
- [ ] A registry port must be ASCII digits only; `+80` and `-1` are validation errors with the image's span.
- [ ] Add `localhost:+80/app:1` to `images_agree` in `src/stack/schema/tests.rs` (both now reject it).

## References
- Image reference parsing (grep `ImageRef` / `FromStr` in `src/stack/`)
- `src/stack/schema.rs` image pattern, `src/stack/schema/tests.rs` `images_agree`

Found while reviewing zcu (2026-11-09).