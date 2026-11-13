# ADR-0010: Preserve path prefixes by default, strip_prefix opt-in, always set X-Forwarded-Prefix, keep Host

- Status: Accepted; `traceparent` is replaced and sent upstream since ADR-0069
- Date: 2026-11-07

## Context

Apps behind a path prefix either know their mount point, which is the common and robust case, or
expect to live at `/`. Silently rewriting paths breaks redirects, cookies with `Path=` and
generated links. Apps also need the original host for absolute URLs, virtual hosting and
signatures.

## Decision

- **Default.** The prefix is forwarded unchanged.
- **`strip_prefix = true`** removes the matched prefix segments before forwarding. An empty
  remainder becomes `/`.
- **`X-Forwarded-Prefix`** is set to the matched prefix either way, so apps can build correct
  URLs.
- **`Host`** is forwarded exactly as the client sent it.

## Alternatives considered

- **Strip by default** (as in Traefik's StripPrefix-style setups): apps generate wrong absolute
  links unless they honor `X-Forwarded-Prefix`. Rejected as the default.
- **Rewrite `Host` to the upstream address**: breaks apps that check Host and any signature that
  covers it. Rejected.
- **Configurable path rewrite rules**: more power than needed. Rejected for now.

## Consequences

- An app at a prefix must either be prefix-aware or opt into `strip_prefix`.
- hoster overwrites client-supplied `X-Forwarded-Prefix`, `-Proto` and `-Host`, because it is the
  edge (see architecture §6.3).
