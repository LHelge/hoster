---
id: sak
title: "Certificates page: summary tiles, failing-cert alert, table, bulk DNS check and retry"
status: done
priority: P2
created: "2026-11-08T18:20:58.848799952Z"
updated: "2026-11-08T22:32:06.457145877Z"
tags:
  - web
  - ui
  - acme
depends_on:
  - xhh
parent: "8bf"
attempts: 1
---

Redesign `/certificates` after `docs/design/Certificates.dc.html` (without the multi-resolver DNS panel, which is the DNS-diagnostics follow-up).

- **Header actions**: **Check all DNS** (ghost; runs the existing advisory DNS check for every host and refreshes the DNS column, through htmx) and **Retry failing (n)** (primary; queues a retry for every host with an error or no certificate). Both are new small endpoints on top of the existing per-host ones, with CSRF, a flash with the outcome, and tests.
- **Summary tiles**: held, valid (more than 30 days left, or the renewal threshold hoster uses), renewing/pending, failed (with attempts if known), and an ACME tile with the directory (Let's Encrypt prod/staging/custom, derived from config) and the renewal threshold.
- **Alert banner** (fail tint, `role="alert"`) for the worst problem: a failing host that still has a certificate expiring soon, or a host with no certificate after failed attempts. Show the last error, the next attempt, and "check DNS" / "retry now" buttons for that host.
- **Table** in a card: domain (mono); **stack** (owner of the route, linked); issuer; challenge (`http-01`); expires (date, days left and a `Meter` out of 90); status pill (valid / pending / failed / expiring); DNS result (the existing `cert_dns.html` fragment as pills); actions "check dns" and "retry". Sorted by expiry, problems first.
- Public addresses and the advisory-DNS explanation become a muted help line under the header.

References: ADR-0015, ADR-0016, ADR-0036, ADR-0051, `templates/certificates.html`, `templates/cert_dns.html`, `src/web/certificates.rs`, `src/acme/`, `docs/design/Certificates.dc.html`.