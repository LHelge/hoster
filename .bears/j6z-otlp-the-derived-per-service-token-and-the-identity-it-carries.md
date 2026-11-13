---
id: j6z
title: "OTLP: the derived per-service token and the identity it carries"
status: done
priority: P2
created: "2026-11-12T19:36:02.174497613Z"
updated: "2026-11-13T17:33:07.599382753Z"
tags:
  - observability
  - secrets
  - stack
depends_on:
  - yjp
parent: xv9
attempts: 1
---

## Summary
Every managed service has a bearer token that names it and that the receiver can verify without a lookup (ADR-0069 §4): `<stack>.<service>.<mac>`. Nothing is stored. hoster takes stack and service from the token and ignores what the payload claims. Rewritten from ADR-0069 on 2026-11-13; the first draft stored random tokens.

## Acceptance Criteria
- [ ] A token type with the behaviour on it: made from a stack and service name and the key; `Display` as `<stack>.<service>.<mac>`; `FromStr` for what a client sent; verification that recomputes the MAC and compares in constant time
- [ ] The MAC key is derived from the secret store's key (ADR-0029) with a fixed label for this purpose alone, so a token reveals nothing usable against stored secrets; the MAC is at least 128 bits, encoded so the whole token is safe in a header and in the URL-encoded `OTEL_EXPORTER_OTLP_HEADERS` value
- [ ] Names cannot contain `.` (`^[a-z][a-z0-9-]{0,30}$`), so the token splits unambiguously; anything that does not split into three parts fails verification
- [ ] Verification also requires that the stack and service exist now, from state the receiver can read without a database query per request; a removed stack's token stops working
- [ ] The token is the same for every generation of a service, so an old and a new generation can both export during a start-first deploy
- [ ] No migration, no table, no cache; per-service rotation is out of scope (ADR-0069 §4) and the epic notes it

## Implementation Notes
- Add the MAC and key-derivation crates with `cargo add` (`sha2` is already a dependency; check what the secret store's cipher crate brings before adding more)
- The injection of the token is task `5up`; this task provides the type

## Testing
- Unit tests: round trip, a token for another service or stack fails, a tampered MAC fails, malformed input fails, a different key fails, an unknown stack or service fails
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§4), ADR-0029
- `src/secrets/`, `src/stack/mod.rs` (`Name`)