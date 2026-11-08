---
id: zty
title: Self-update history on the System page
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.051695889Z"
updated: "2026-11-08T18:22:30.051695889Z"
tags:
  - design-followup
  - update
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The "Update history" table of `docs/design/System.dc.html`: from → to version, when, result (applied / rolled back + reason) and the connection gap.

## Why it isn't in the design pass

hoster doesn't record updates or rollbacks anywhere queryable. The self-update and `hoster rollback` flows (ADR-0052) would have to write a row, and a rollback has to write it with the older binary's schema in mind (the newer binary runs the down-migrations).

## Size and shape

Small to medium: one table with a reversible migration, written by update and rollback, plus the table on System. Mind the rollback ordering: write the history before migrating down, or keep it outside the migrated schema.

References: ADR-0052, `src/update/`, `src/store/`, `src/web/system.rs`.