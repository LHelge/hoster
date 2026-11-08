---
id: "7mg"
title: "Web UI: stack secrets (write-only), registry credentials, re-resolve tags"
status: in_progress
priority: P1
created: "2026-11-07T21:12:34.638757908Z"
updated: "2026-11-08T01:32:39.687427862Z"
tags:
  - web
  - security
depends_on:
  - ax3
  - "3g2"
parent: ame
attempts: 2
---

## Summary
The pages spec.md §5.2/§5.4 promise but no task built: **Stack → Secrets** (ADR-0029), **Settings → Registries** (ADR-0025) and the **"re-resolve tags"** action (ADR-0025). Until these exist, private images and app secrets cannot be configured from the UI.

## Acceptance Criteria
- [ ] Stack → Secrets: list names with last-changed time; set/replace/delete a value; values are never rendered back. Before saving, the confirmation lists the services that reference the secret and will be redeployed; on confirm the secret is stored and those services are redeployed in one deployment (ADR-0049)
- [ ] Settings → Registries: per registry host (`ghcr.io`, `docker.io`, …) username + token, stored in the secret store; token write-only; "test" action does an anonymous-or-authenticated token request through the registry client. Registry credentials are hoster's own and trigger no redeploy
- [ ] Stack page: "re-resolve tags" resolves third-party images, shows the diff and the affected services, and on confirm applies the new revision and redeploys them (ADR-0025, ADR-0049)
- [ ] All actions are POSTs with CSRF protection (ADR-0031); secrets never appear in logs or error messages

## Implementation Notes
- Askama templates + HTMX partials, same pattern as `3g2`
- Registry credential storage replaces the interim 0600-file provider from `p7a`

## Edge Cases & Considerations
- Deleting a secret still referenced by a stack revision → the affected services would fail validation at their next deploy; warn and name them, require typed confirmation
- Registry test failure → show the registry's error class (401/429/network), never the token

## Testing
- Handler tests with a fake secret store and fake registry client; template render tests asserting no secret value appears in HTML
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- spec.md §5.2 step 3, §5.4
- ADR-0025, ADR-0029, ADR-0031, ADR-0049