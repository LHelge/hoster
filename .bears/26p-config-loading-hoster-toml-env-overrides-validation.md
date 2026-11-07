---
id: "26p"
title: "Config loading: hoster.toml, env overrides, validation"
status: open
priority: P1
created: "2026-11-07T14:45:17.155672714Z"
updated: "2026-11-07T14:45:17.155672714Z"
tags:
  - core
depends_on:
  - mnu
  - t94
parent: "8xd"
---

## Summary
Load and validate hoster's own configuration (ADR-0024: `~/.config/hoster/hoster.toml`, `HOSTER_*` env overrides with `__` nesting, secrets in separate files/credentials), with env var overrides and clear errors, and initialize tracing.

## Acceptance Criteria
- [ ] `Config` struct with serde + `deny_unknown_fields`: state_dir, admin_host, acme (directory_url, contact), upstream port range, proxy (threads, grace_period, graceful_shutdown_timeout, default timeouts), poll interval, webhook OIDC audience (default `https://<admin_host>`, ADR-0047)
- [ ] `HOSTER_ADMIN_GITHUB` read from the environment and required (ADR-0021)
- [ ] Config path from `--config` or `$XDG_CONFIG_HOME/hoster/hoster.toml`; env overrides `HOSTER_*` (with `__` for nesting) documented; state dir defaults to `$XDG_DATA_HOME/hoster` (ADR-0023)
- [ ] Validation errors name the offending key and are printed without a backtrace
- [ ] Tracing initialized (env-filter, compact text by default)
- [ ] An annotated example config is committed under `deploy/hoster.example.toml`

## Implementation Notes
- Durations parsed with `humantime-serde`
- Secrets (OAuth client secret, registry creds, secret-store key) must NOT be fields here — leave hooks for systemd credentials (`$CREDENTIALS_DIRECTORY`)

## Edge Cases & Considerations
- Port range must be within 1024–65535 and contain ≥ 16 ports
- admin_host must be a lowercase FQDN

## Testing
- Unit tests: valid example parses; unknown key rejected; env override precedence; invalid durations
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0024
- ADR-0021