---
id: nnq
title: "Stack format: `files` table"
status: open
priority: P2
created: "2026-11-09T17:22:16.205452185Z"
updated: "2026-11-09T23:58:38.823017608Z"
tags:
  - stack
depends_on:
  - "5jg"
parent: xgf
---

## Summary
Parse and validate `[services.<svc>.files]` exactly as ADR-0057 §1 and §2 specify, carry it in the model, include it in the config hash (§7) and the stack JSON Schema. The upload, secret wiring and podman version check are rp5's.

## Acceptance Criteria
- [ ] `Service.files: BTreeMap<ContainerPath, ConfigFile>`. `ConfigFile` holds the source (`Inline(String)` or `Secret(SecretRef)` of the same stack), an optional `FileMode` and an optional `FileOwner`, each with `FromStr` / `Display`. `ContainerPath` is a validated absolute path type (`FromStr` / `Display` / `AsRef<str>`), reusable by `tmpfs` (M13).
- [ ] Format (§1): an inline table with exactly one of `content` (a TOML string) or `secret = "<stack>/<name>"`. `mode` is an octal string `"0000"`–`"0777"` (setuid, setgid and sticky rejected); `owner` is a numeric `"uid:gid"`, both required, each at most 65535. Owner names, unknown keys, a malformed secret name and a secret of another stack are errors.
- [ ] Path rules (§2), all collected with spans: absolute and normalized (no empty, `.` or `..` component, no trailing `/`, not `/`, no NUL); unique per service; not an ancestor of another of its files; not under `/proc`, `/sys` or `/dev`; not `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`, `/run/.containerenv`, `/etc/passwd` or `/etc/group`; neither equal to, under nor an ancestor of a service `volumes` path. The reserved-path and volume checks are a method rp5 can re-run on a resolved path.
- [ ] Caps: inline content 256 KiB per file and 1 MiB per stack (secret files don't count). The apply routes set `DefaultBodyLimit` to 8 MiB (axum's default is 2 MB); a larger body gets axum's 413.
- [ ] `ConfigHash::new` adds a `files` key only when the service has files, so no existing hash changes. Per file: `{path, mode, owner, sha256(content)}` or `{path, mode, owner, secret, version}`, with defaults resolved (`"user"` for the container-user owner of secret files). Secret entries take their version from `SecretVersions`; making the collection include file-only secrets is rp5's.
- [ ] JSON Schema: the stack JSON Schema (`src/stack/schema.rs`, `site/static/stack.schema.json`, drift test; task zcu) lands on `epic/emu-stack-authoring` first. Once this branch is rebased onto it, extend the schema with `files` (path keys, `content` / `secret` one-of, `mode` / `owner` patterns, `additionalProperties: false`) and regenerate the committed file.

## Edge Cases
- `content = ""` is a valid empty file. `mode = "644"` is rejected (the ADR's range is four-digit strings) and so is `"1755"`. `owner = "1000"` without a gid is an error, as is `owner = "65536:0"`. A path differing only by a trailing `/` is rejected, not normalized.

## Testing
- `src/stack/tests.rs` snapshots for every format and path rule and both caps.
- Config-hash tests in `src/orchestrator/plan.rs`: a stack without `files` keeps its hash; content, mode or owner change → new hash; secret version change → new hash; an unrelated service unchanged.
- A web test that an apply body over 2 MB but under 8 MiB is accepted.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0057 (§1, §2, §7); ADR-0029
- `docs/architecture.md` §8 (Config files bullet)
- `site/content/wiki/stack-format.md` §2.2 (the reference text itself is nhn's)
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/stack/schema.rs` (after rebase), `src/orchestrator/plan.rs`, `src/web/stacks/apply.rs`
