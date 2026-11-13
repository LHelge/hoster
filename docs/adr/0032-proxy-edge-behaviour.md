# ADR-0032: Proxy edge behaviour for unknown hosts, Host/SNI mismatch and errors

- Status: Accepted (resolves OQ-19); connections beyond the caps are refused at accept since ADR-0065; `host.containers.internal` on `:80` is the OTLP receiver for local peers since ADR-0069
- Date: 2026-11-07

## Decision

| Situation | Response |
|---|---|
| TLS for an SNI with no certificate | The handshake fails: the resolver returns `None`. There is **no default certificate**. |
| `Host` ≠ TLS SNI | **421 Misdirected Request** |
| Known host, no matching prefix | Plain built-in **404** |
| Plain HTTP for a host not in the RouteTable | **404**, no redirect. ACME challenge paths are still answered. |
| Plain HTTP for a known host | **308** to `https://` |
| Upstream connect refused or reset | **502** with a minimal built-in page |
| Service in a stop-first gap (ADR-0039) | **503** with `Retry-After` |

## Alternatives considered

- **Redirect all plain HTTP**: a simpler rule, but it turns :80 into an open redirector for any
  host name.
- **Self-signed default certificate and no Host/SNI check**: browsers show warnings anyway, the
  certificate reveals information, and one stack's TLS connection could be used to reach
  another (domain fronting).

## Consequences

- HTTP/2 connection coalescing across our hosts cannot happen, because certificates are
  per-host (ADR-0035 #4) and 421 is the standard answer if a client tries.
- Error pages are built in and minimal. Custom pages per stack are not planned.
