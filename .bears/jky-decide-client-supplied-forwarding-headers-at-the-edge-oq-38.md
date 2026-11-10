---
id: jky
title: "Decide: client-supplied forwarding headers at the edge (OQ-38)"
status: open
priority: P2
created: "2026-11-10T14:45:11.143835Z"
updated: "2026-11-10T14:45:11.143835Z"
tags:
  - decision
  - proxy
  - security
---

## Summary

hoster is the edge, but `Forwarding::apply` appends to a client-supplied `X-Forwarded-For`
instead of replacing it, and passes `Forwarded`, `X-Real-IP`, `X-Forwarded-Port`,
`X-Forwarded-Ssl`, `X-Forwarded-Scheme`, `X-Original-URL` and `X-Rewrite-URL` through untouched.
An app that reads the leftmost `X-Forwarded-For` entry, or prefers one of the unfiltered headers,
gets a client-chosen address or URL. Options and recommendation in docs/open-questions.md OQ-38:
A keep and document, B replace `X-Forwarded-For` and strip the rest (recommended), C also set
`X-Real-IP` and `Forwarded`, D add `trusted_proxies`.

## Acceptance Criteria

- [ ] Owner decides; ADR written (amends ADR-0010) and architecture §6.3 updated
- [ ] Follow-up implementation task filed under epic `kyn`: `Forwarding::apply`, its unit tests
      and the proxy tests that expect an appended chain, the stack-format guide

## References

- docs/open-questions.md OQ-38
- `docs/adr/0010-prefix-preservation-and-forwarded-headers.md`, `docs/architecture.md` §6.3
- `src/proxy/router.rs` (`Forwarding::apply`)