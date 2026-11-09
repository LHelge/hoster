---
id: xw9
title: Cut releases without git-cliff installed locally
status: done
priority: P2
created: "2026-11-09T11:00:57.700062931Z"
updated: "2026-11-09T11:01:00.024020334Z"
tags:
  - release
attempts: 1
---

`release.sh prepare` drops the git-cliff preview from the release PR body, so a release needs only git, cargo and gh locally. The release workflow keeps rendering the real notes with the pinned git-cliff when the tag is pushed.

## References
- scripts/release.sh, scripts/test-release.sh
- docs/adr/0052-release-install-and-self-update.md (correction 2026-11-09)
- README.md "Cutting a release"