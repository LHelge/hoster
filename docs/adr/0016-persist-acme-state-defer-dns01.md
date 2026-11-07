# ADR-0016: Persist the ACME account key and certificates to disk; defer DNS-01

- Status: Accepted
- Date: 2026-11-07

## Context

Re-issuing every certificate on each restart would hit CA rate limits quickly and make startup
depend on the CA being reachable.

## Decision

- **Account.** `AccountCredentials` from instant-acme (serde-serializable) is stored as
  `acme/account.json`, mode 0600.
- **Certificates.** Each certificate chain and private key is stored under `certs/<host>/`, mode
  0600 for keys, with a small `meta.json`. Writes are atomic: temp file, then rename.
- **Startup.** All valid certificates are loaded into `CertMap` before Pingora starts.
- **DNS-01** is deferred until wildcard certificates are needed.

## Alternatives considered

- **Certificates in the state store** (e.g. SQLite blobs): workable, but files are easier to
  inspect, back up and reuse in an emergency. The store decision (ADR-0022, SQLite) does not affect
  this one.
- **In-memory only**: rate-limit and availability risk. Rejected.
- **DNS-01 now**: provider-specific API credentials on the server, with no current need.

## Consequences

- The state directory holds key material and must be in backups (ADR-0030) and protected by file
  permissions.
- Wildcard hosts and virtual-hosted S3 buckets are unavailable until DNS-01 lands.
