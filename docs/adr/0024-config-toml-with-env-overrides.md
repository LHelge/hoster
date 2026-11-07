# ADR-0024: hoster configuration is a TOML file with env overrides; secrets kept out of it

- Status: Accepted (resolves OQ-27)
- Date: 2026-11-07

## Context

hoster needs non-secret settings, such as the admin host, ACME directory and port range, and a
handful of secrets: the GitHub OAuth client secret, registry credentials and the secret-store key.

## Decision

- **Settings file.** `~hoster/.config/hoster/hoster.toml` (`$XDG_CONFIG_HOME/hoster/hoster.toml`,
  overridable with `--config`). It holds:
  - admin hostname
  - ACME directory URL and contact
  - upstream port range
  - Pingora threads, timeouts and grace periods
  - poll interval and similar settings

  It is parsed with serde and `deny_unknown_fields`, and durations are humantime strings.
- **Env overrides.** Every setting can be overridden by a `HOSTER_*` environment variable, for
  example `HOSTER_ACME__DIRECTORY_URL`, with `__` as the nesting separator.
- **Admin identity.** `HOSTER_ADMIN_GITHUB` comes from the environment (ADR-0021) and is
  required.
- **Secrets** never live in the TOML file. They come from separate files with mode 0600 or from
  systemd credentials (`$CREDENTIALS_DIRECTORY`). The file names those sources by path, never by
  value.

## Alternatives considered

- **Env vars only**: long unit files, and no comments or structure for nested settings.
- **TOML including secrets**: simplest to run, but secrets would leak into config backups and
  edits.

## Consequences

- An annotated `hoster.example.toml` ships with the binary.
- Same format as stack files (stack-format.md §1).
