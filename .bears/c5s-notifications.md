---
id: c5s
title: Notifications
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.061186156Z"
updated: "2026-11-08T18:22:30.061186156Z"
tags:
  - design-followup
  - observability
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The design's Settings → **notifications** section (drawn only as a nav item in `docs/design/Settings.dc.html`): tell the operator about failed deploys, crash loops, certificates failing or close to expiry, and updates. ADR-0045 names this as "the main gap to revisit after v1, e.g. with a single notification webhook".

## Size and shape

Medium to large, depending on channels: one outbound webhook (Slack/Discord/ntfy-compatible JSON) is medium; email would need SMTP config and secrets. It builds naturally on the event-log epic (notify = a filter over events).

## Starts with a decision

Channels, which events, de-duplication and rate limiting, and where the channel config lives (hoster.toml + secret store per ADR-0024). Needs a new ADR that amends ADR-0045.

References: ADR-0024, ADR-0029, ADR-0045.