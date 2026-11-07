# ADR-0031: Admin UI on a reserved host via Unix socket; handle comparison; SQLite sessions with CSRF tokens; no break-glass

- Status: Accepted (resolves OQ-05; refines ADR-0021)
- Date: 2026-11-07

## Context

ADR-0021 fixed the admin UI's technology and login method. This record settles how the UI is
reached, how the admin is identified, how sessions and CSRF work, and what happens if GitHub login
is unavailable.

## Decision

- **Exposure.** A **reserved admin hostname** from config, e.g. `hoster.example.com`, is routed
  by Pingora to Axum listening on a **0600 Unix socket** in the state directory (Pingora
  `HttpPeer::new_uds`, verified in 0.9.1).
  - The reserved host takes part in collision checks, so no stack can claim it (ADR-0012).
  - It gets ACME certificates like any other host.
  - `/hooks/*` (webhook) is served by the same Axum app.
- **Identity.** After GitHub OAuth (authorization code + `state` + PKCE, no scopes), the GitHub
  **login handle** is compared case-insensitively with `HOSTER_ADMIN_GITHUB`.
- **Sessions.** Server-side sessions are stored in SQLite (ADR-0022) and can be revoked,
  including "log out everywhere".
  - Cookie: `__Host-hoster_session`, `Secure`, `HttpOnly`, `SameSite=Lax`.
  - Timeouts: 12 h absolute, plus an idle timeout.
- **CSRF.**
  - Every state-changing request is a POST.
  - POSTs are checked for `Origin`/`Sec-Fetch-Site` and must carry a per-session CSRF token,
    which HTMX sends through `hx-headers`.
- **No break-glass.** If GitHub login is unavailable, the operator uses SSH, files and the DB
  directly until it is back.

## Alternatives considered

- **Loopback TCP for Axum**: reachable by any local process, and possibly from containers through
  host loopback.
- **A separate TLS port for Axum**: a second TLS stack and an extra open port.
- **Comparing the numeric GitHub user id**, resolved from the handle or given directly: protects
  against handle re-registration, at the cost of a lookup or a less readable setting.
- **Signed stateless cookies**: cannot be revoked before they expire.
- **A one-time login-link CLI**: adds a credential-minting path.

## Consequences

- **Accepted risk:** if the admin renames their GitHub account, someone else could register the
  old handle and log in. Renaming therefore means updating `HOSTER_ADMIN_GITHUB` straight away.
  - Optional cheap hardening, not part of this decision: log the numeric id on each login and warn
    when it differs from the last one seen.
- An outage of GitHub or the OAuth app locks the UI. Running apps are unaffected. The webhook
  (ADR-0047) depends on GitHub Actions and GitHub's JWKS, which usually fail together with OAuth,
  so during a GitHub outage neither deploys nor rollbacks are possible. A local break-glass path
  was considered and declined for v1 (OQ-31, 2026-11-07); this record stands.
- Containers can never reach the admin socket, because it is a Unix socket on the host
  filesystem.
