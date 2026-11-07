# ADR-0037: Use the ring crypto provider everywhere

- Status: Accepted (resolves OQ-21)
- Date: 2026-11-07

## Decision

All rustls users in hoster use **ring**:

- Pingora's `pingora-rustls` is fixed to ring and installs it as the process default (verified in
  0.9.1).
- instant-acme: `default-features = false`, features `ring`, `hyper-rustls`, `rcgen`, `time`.
- reqwest (OAuth, registry client): rustls with ring, no aws-lc.
- `CertifiedKey`s are built with `rustls::crypto::ring::sign::any_supported_type`.

## Alternatives considered

- **Mixed providers** (instant-acme's default aws-lc-rs next to ring): two crypto libraries get
  compiled, aws-lc needs a C/cmake build, and it becomes unclear which provider is the process
  default.

## Consequences

- Smaller binary, simpler build.
- Revisit if post-quantum key exchange becomes a requirement and aws-lc-rs is the only provider
  that supports it well.
- CI checks with `cargo tree -i aws-lc-rs` that aws-lc-rs isn't pulled in by accident.
