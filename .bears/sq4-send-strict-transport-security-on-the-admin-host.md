---
id: sq4
title: Send Strict-Transport-Security on the admin host
status: done
priority: P3
created: "2026-11-10T14:45:11.157392Z"
updated: "2026-11-11T15:09:22.931318007Z"
tags:
  - proxy
  - web
  - security
parent: kyn
attempts: 1
---

## Problem

Nothing in `src/proxy` or `src/web` sends `Strict-Transport-Security`. A first visit to the admin
host over `http://` gets a 308 from the `:80` service, and that plain-HTTP hop can be intercepted
before the browser ever learns the host is HTTPS-only. The admin host carries the session cookie
and the OAuth flow, and it is only ever served over TLS with a certificate hoster issued.

## Do

- Send `Strict-Transport-Security: max-age=<about a year>` on every response for the reserved
  admin host. No `includeSubDomains` (the admin host's parent domain is not hoster's to speak
  for) and no `preload`.
- Put it where the admin app's other security headers are set (the CSP of ADR-0021/0054 in
  `src/web`), so it sits next to them and is covered by the same tests. The proxy's built-in
  error pages for the admin host don't need it.
- Routed app hosts are out of scope: whether an app wants HSTS, and with which scope, is the
  app's choice, and it can send the header itself. Say so in the stack-format guide in one line.

## Acceptance

- [ ] A response from the admin app carries the header; a test asserts it
- [ ] The stack-format guide has the note about app hosts

## References

- `src/web/mod.rs` (where the security headers are set), `src/proxy/plain.rs` (the 308)
- `docs/adr/0031-admin-ui-exposure-identity-sessions.md`, `docs/adr/0032-proxy-edge-behaviour.md`
- `site/content/wiki/stack-format.md`

## Result (2026-11-11)

`SecurityHeaders` in `src/web/mod.rs` adds `Strict-Transport-Security: max-age=31536000` (no
`includeSubDomains`, no `preload`) to every response of the admin app, asserted in
`every_response_carries_the_security_headers`. The proxy's own error pages for the admin host
do not carry it. Browsers ignore the header on the plain-HTTP dev listener (`web.dev_listen`),
so it is sent there too rather than making the header set depend on the listener. The
stack-format guide (§2.3) says that apps set their own HSTS.
