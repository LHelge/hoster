# ADR-0014: TLS via Pingora's rustls backend with a custom ResolvesServerCert over an ArcSwap'd SNI map

- Status: Accepted
- Date: 2026-11-07

## Context

Certificates appear and renew while the process is running, one per hostname. The TLS listener
must pick the right one by SNI on every handshake, without a restart.

## Decision

Build Pingora with the `rustls` feature. Implement `rustls::server::ResolvesServerCert` as
`CertResolver`, backed by `ArcSwap<HashMap<String, Arc<CertifiedKey>>>`, and install it with
`TlsSettings::set_cert_resolver` (verified in pingora-core 0.9.1; when it is set, cert and key
paths are ignored). The ACME task replaces map snapshots, and handshakes do a lock-free load.

## Alternatives considered

- **The BoringSSL (or OpenSSL) certificate callback** (`TlsAccept::certificate_callback`).
  Rejected:
  - It needs a C toolchain, cmake and Go for the BoringSSL build. That slows builds, complicates
    cross-compiling and static linking, and adds C code to the attack surface.
  - The callback API is tied to the `openssl_derived` backends, and switching later costs about
    the same as choosing now.
  - The rustls backend explicitly does not invoke that callback (it logs a warning pointing to
    `ResolvesServerCert`), so the two are not interchangeable. The rustls path is the supported
    route to dynamic certificates.
  - rustls is memory-safe and already used by instant-acme and the HTTP clients.
- **Static cert files and a restart on renewal**: drops connections on every renewal. Rejected.
- **s2n backend**: less common in the Rust ecosystem. No advantage here.

## Consequences

- `pingora-rustls` uses rustls 0.23 with the **ring** provider and installs it as the process
  default. `CertifiedKey`s must be built with ring, and the other crates should use ring too
  (ADR-0037).
- An unknown SNI fails the handshake; there is no default certificate (ADR-0032).
- Some TLS features that are specific to BoringSSL (e.g. certain post-quantum or OCSP-stapling
  setups) depend on what rustls supports.
