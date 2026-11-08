---
id: tbf
title: "M9: Release, install and self-update"
type: epic
status: done
priority: P1
created: "2026-11-08T08:24:49.431326457Z"
updated: "2026-11-08T10:06:47.714485139Z"
---

Semver-tag releases with git-cliff notes for x86_64 and aarch64; a curlable, idempotent install.sh; versioned binaries in ~/.local/lib/hoster with self-update from the UI and CLI; reversible migrations so the newer binary can roll back. See ADR-0052.