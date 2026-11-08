---
id: t33
title: Self-update also refreshes the systemd unit from the release
status: proposed
priority: P2
created: "2026-11-08T11:58:34.777014766Z"
updated: "2026-11-08T11:58:40.791193798Z"
tags:
  - core
  - ops
---

## Summary
`hoster update` / the System page only swap the binary. Fixes to `hoster.service` (like the v0.1.1 AppArmor fix) only reach a host when install.sh is re-run as root. Proposal: the updater also downloads `hoster.service` from the release, verifies it against SHA256SUMS, installs it to `~/.config/systemd/user/hoster.service` atomically and runs `systemctl --user daemon-reload` before restarting; rollback restores the previous unit (keep a copy per version). Drop-ins in `hoster.service.d/` are untouched. Needs an ADR-0052 amendment (owner's call), hence a proposal.

## Acceptance Criteria
- [ ] Owner accepts; ADR-0052 amended
- [ ] Updater installs the verified unit + daemon-reload; rollback restores the previous one
- [ ] Unit installed by the updater is identical to what install.sh would install
- [ ] Tests over the updater fakes; docs/install.md updated

## References
- ADR-0052, ADR-0023
- src/update/