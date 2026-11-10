---
id: nhn
title: Config files in the UI and docs
status: open
priority: P2
created: "2026-11-09T17:23:25.591456230Z"
updated: "2026-11-09T23:58:38.823017608Z"
tags:
  - web
  - docs
depends_on:
  - rp5
parent: xgf
---

## Summary
Show config files where operators look for them (ADR-0057 §10), and write the `files` part of the stack-format reference.

## Acceptance Criteria
- [ ] The stack overview service rows list files: container path, mode, owner (resolved defaults, "container user" for secret files without `owner`), and size for inline content or the secret name. File content, inline or secret, is never rendered.
- [ ] The apply preview's plan names services redeployed *because* a file changed, e.g. "app: file /etc/app/config.yml changed".
- [ ] The environment tab lists secrets used by files alongside env secrets (from m9b's `Service` secret-reference accessor), so a missing one is obvious before deploying.
- [ ] stack-format.md §2.2 documents `files`: format, `mode` / `owner` syntax and defaults (`content` `0:0 0444`, `secret` `0400` owned by the container's user), path rules and caps, and that files are not writable by a non-root container user, with `read_only = true` recommended to make them read-only (except under `/run`, `/tmp`, `/var/tmp`).
- [ ] It documents `owner` for images that start as root and drop privileges themselves (nginx, Mosquitto, postgres), which podman sees as root; the podman 5.4 requirement (Ubuntu 24.04 and Debian 12 can't use `files`; the message is 2eq's); that rollback restores inline content but secret files get the current value; and the RHEL-family caveat that podman's `mounts.conf` may mount over `/run/secrets/`, hiding files uploaded there.
- [ ] A worked example: a generic metrics or reverse-proxy image configured by a file plus a secret file, nothing app-specific in hoster. The redaction note covers file secrets line by line. `aphid build` passes.

## Edge Cases
- A service with many files or long paths keeps the overview row readable (wrap or collapse). A file whose secret is missing shows the secret name with the same missing marker as env secrets.

## Testing
- Web tests in `src/web/stacks/tests.rs` (and `environment_tests.rs`) for overview, preview and environment rendering, including that neither inline nor secret content appears in the HTML.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `aphid build` in `site/`.

## References
- ADR-0057 (§4, §5, §8–§10, Consequences); ADR-0029, ADR-0054
- `docs/architecture.md` §1.7 (owner and tmpfs behaviour), §5.5 (redaction)
- `src/web/stacks/overview.rs`, `src/web/stacks/apply.rs`, `src/web/stacks/environment.rs`, `templates/`
- `site/content/wiki/stack-format.md` §2.2
