# ADR-0015: ACME with instant-acme over HTTP-01 on the port-80 listener; background renewal

- Status: Accepted
- Date: 2026-11-07

## Context

Every routed host needs a publicly trusted certificate, issued and renewed with no manual steps.
hoster already owns :80.

## Decision

- **Client.** instant-acme (verified 0.8.5) drives the ACME flow: account, order, HTTP-01
  challenge, finalize, certificate.
- **Port 80.** The `:80` Pingora service answers `/.well-known/acme-challenge/<token>` from an
  `ArcSwap`'d challenge map and redirects everything else to HTTPS with a 308.
- **Renewal.** A background tokio task renews certificates once less than 1/3 of their lifetime
  remains, or within the ACME Renewal Information (ARI) window where the CA supports it
  (instant-acme's `renewal_info`, behind the `time` feature).

## Alternatives considered

- **TLS-ALPN-01**: needs challenge handling inside the TLS handshake path, which ties it to the
  resolver. HTTP-01 is simpler, since we own :80 anyway.
- **DNS-01**: needed only for wildcards. It needs DNS provider APIs and credentials. Deferred
  (ADR-0016).
- **rustls-acme / tokio-rustls-acme** (managed TLS-ALPN-01 acceptors): they would take over the
  acceptor, which conflicts with Pingora owning the listener.
- **Certbot or lego as external tools**: a second process with file handoff and reload hooks.
  Rejected.

## Consequences

- :80 must be reachable from the internet, and DNS must point at the VPS before the first
  issuance.
- Let's Encrypt rate limits apply. hoster backs off per host, and staging is configurable.
- instant-acme's default features use aws-lc-rs while Pingora uses ring. hoster uses ring (ADR-0037).
- Certificates are requested at stack apply and deploys never wait for them (ADR-0036).
