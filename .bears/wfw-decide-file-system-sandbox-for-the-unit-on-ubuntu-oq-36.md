---
id: wfw
title: "Decide: file-system sandbox for the unit on Ubuntu (OQ-36)"
status: done
priority: P1
created: "2026-11-08T11:58:18.020088500Z"
updated: "2026-11-11T21:49:53.991697332Z"
tags:
  - decision
---

## Summary
On Ubuntu ≥ 23.10 (`kernel.apparmor_restrict_unprivileged_userns = 1`) the user unit gets no mount namespace: ProtectSystem/ProtectHome/ReadWritePaths/PrivateTmp etc. are silently skipped; only NoNewPrivileges and seccomp apply. Found on the first real install (Ubuntu 26.04, v0.1.0). Options and recommendation in docs/open-questions.md OQ-36: A accept (current), B lift the restriction via sysctl, C AppArmor profile, D system service with `User=hoster` (recommended).

## Acceptance Criteria
- [ ] Owner decides; ADR written (supersedes/amends ADR-0023 as needed)
- [ ] Follow-up implementation task(s) filed: unit, install.sh, docs/install.md, doctor `unit_sandbox`, scripts/test-systemd-unit.sh expectations

## References
- docs/open-questions.md OQ-36
- ADR-0023 (and its 2026-11-08 correction), ADR-0052