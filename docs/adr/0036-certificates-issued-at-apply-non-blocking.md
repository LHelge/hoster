# ADR-0036: Certificates are requested at stack apply; deploys never wait for the CA

- Status: Accepted (resolves OQ-16); the DNS check is advisory since ADR-0051
- Date: 2026-11-07

## Decision

- **When issuance starts.** When a stack revision that adds a hostname is applied (ADR-0027), the
  actor sends `AcmeCommand::Ensure{hosts}` right away, before any image is deployed. HTTP-01 is
  served on :80 and does not depend on the route being live.
- **Deploys don't wait.** Until the certificate exists, HTTPS handshakes for that host fail
  (ADR-0032), and the UI shows **"TLS pending"** with the last ACME error.
- **DNS check first.** Before attempting issuance, hoster checks that the host's A/AAAA records
  point at this server. If they don't, issuance is deferred and the UI says why, so Let's Encrypt
  rate limits aren't spent on doomed attempts.
- **Failures** back off per host (ADR-0035 #5).
- **Hosts removed from all stacks:** the certificate is kept until it expires and is not renewed.

## Alternatives considered

- **Block the deploy until the certificate is issued**: no TLS-error window, but a CA outage
  would block deploys of new hosts.
- **Self-signed fallback**: browser warnings and HSTS pitfalls.

## Consequences

- A brand-new host can briefly fail TLS after its first deploy, typically for seconds and only if
  the deploy beats issuance.
- The admin host is ensured at startup the same way.
