---
id: vqy
title: DNS diagnostics for certificates and apply pre-flight
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.033162343Z"
updated: "2026-11-08T18:22:30.033162343Z"
tags:
  - design-followup
  - acme
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The **DNS check panel** of `docs/design/Certificates.dc.html`: A/AAAA/CAA/TTL per resolver (1.1.1.1, 8.8.8.8, 9.9.9.9 and the authoritative nameserver) against hoster's public addresses; an **HTTP-01 reachability probe** (`GET http://host/.well-known/acme-challenge/<probe>` answered by hoster itself); and a plain-language **verdict** ("all resolvers agree, so not propagation; the A record points at the old host"). Also the **pre-flight** lines in the apply preview (`docs/design/NewStack.dc.html`): DNS of each new host, route conflicts and port 80 reachability before applying.

## Why it isn't in the design pass

The current check is one system-resolver lookup compared with the public addresses (ADR-0036/0051).

## Size and shape

Medium. A resolver client (for example hickory) that queries specific servers and walks to the authoritative one; a probe token route on :80; verdict rules as a small typed enum. The HTTP-01 self-probe may hairpin through the VPS's own public IP, so check whether that works on common providers. DNS-01 stays out (ADR-0016).

## Open points

Outbound DNS to public resolvers from the VPS (privacy and firewalls); whether the probe needs an external vantage point.

References: ADR-0015, ADR-0016, ADR-0036, ADR-0051, `src/acme/`, `src/web/certificates.rs`.