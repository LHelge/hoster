# ADR-0013: Services that need signature-stable paths get their own hostname

- Status: Accepted
- Date: 2026-11-07

## Context

S3-compatible storage such as rustfs authenticates requests with AWS SigV4. The signature covers
the HTTP method, the canonical URI path, the query and selected headers, `Host` included.
Presigned URLs embed the signature the same way. Any path rewriting between client and server
invalidates it.

## Decision

Services like these are routed on a dedicated hostname at prefix `/`, e.g.
`s3.notes.example.com`, never under a path prefix of a shared host.

## Alternatives considered

- **Path-routing S3** (`notes.example.com/s3/…`). Rejected:
  - With `strip_prefix`, the server sees a different path from the one the client signed, so
    signatures fail.
  - Without `strip_prefix`, the server must understand a path prefix in front of the bucket name.
    S3 servers treat the first path segment as the bucket, so `/s3/bucket/key` breaks path-style
    addressing.
  - SDKs construct URLs themselves, and many cannot be configured with a path prefix on the
    endpoint, or handle it inconsistently.
  - Virtual-hosted-style addressing (`bucket.s3.…`) needs host-based routing anyway.
- **Special-casing S3 in the proxy** (re-signing): this is an app-specific feature, ruled out by
  ADR-0018.

## Consequences

- More hostnames, and one more certificate each. Cheap with HTTP-01.
- Virtual-hosted-style buckets (`<bucket>.s3.example.com`) need wildcard routing and certificates.
  Until DNS-01 exists (ADR-0016), use path-style addressing on the dedicated host.
- The stack format documents this pattern (see stack-format.md).
