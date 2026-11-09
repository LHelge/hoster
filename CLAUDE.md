# CLAUDE.md

hoster is a single-binary deployment orchestrator for one VPS: rootless podman, an embedded
Pingora proxy with ACME, zero-downtime deploys by digest, and an Axum admin UI.

## What to read (keep context small)

The documentation is ~45k tokens in total. Don't read it all; the Bears task is the entry point.

1. **Always:** this file, the task body (`get_task`), and `docs/adr/README.md` (the one-line-per-ADR
   index, ~2k tokens).
2. **Per task:** only the ADRs and `docs/architecture.md` sections the task's *References* list
   names. Read ADRs individually (they are 20–90 lines each) and architecture.md by section with
   `offset`/`limit` or grep for the `## N.` heading — never the whole file or all ADRs.
3. **If unfamiliar with the project:** `spec.md` once (~3k tokens) for the user-facing picture;
   `site/content/wiki/stack-format.md` when touching stack parsing, routing or deploys.

The user-facing guides (install, stack format, deploying from CI, static sites, backup) are the
aphid site in `site/content/wiki/`, published at https://docs.hoster.lhelge.se. Link between them
with `[[slug]]` or `[[slug#anchor|text]]`; `aphid build` in `site/` fails on a broken one.
`site/content/blog/` holds one post per release (README "Cutting a release"), and `site/theme/` is
the site's own aphid theme, which copies the admin UI's tokens and fonts from `assets/`.
`docs/` holds the internal design docs (ADRs, architecture, open questions, spikes).
4. **Don't read** `docs/open-questions.md` unless you are raising a new question; it is history.
   `docs/architecture.md` §1 ("Verified against") is reference material for library behaviour,
   useful only when writing code against Pingora, bollard, instant-acme or the registry API.

If a task's References are missing or wrong, fix the task body rather than reading more.

## Planning and tracking: Bears

Use [Bears](https://github.com/LHelge/bea-rs) (MCP tools) for all planning and progress tracking.

- Pick work with `list_ready`, mark it with `start_task`, and finish with `review_task` or
  `complete_task`.
- Mark a task done in the same PR that integrates it: run `complete_task` before committing, and
  include its `.bears/` file in the task's commit. A task's status should never need a separate
  follow-up commit or PR after the merge.
- If you get stuck, use `release_task`.
- File uncertain work with `propose_task` instead of adding it to the backlog.
- A high `attempts` count means earlier tries failed. Change approach or escalate rather than
  repeating them.
- **Architecture choices are the owner's.** If you hit a decision that no ADR covers, add it to
  `docs/open-questions.md` with options and a recommendation, and create a `decision` task. Don't
  decide it yourself.

## Git

- **One branch per epic, one commit per task.** Name the branch `epic/<epic-id>-<slug>` (e.g.
  `epic/tbf-release-install-update`). Each Bears task in the epic lands as exactly one
  Conventional Commit on that branch, with `Bears: <task-id>` in the body. Squash fixups and
  review fixes into the task's commit before pushing.
- Keep the epic branch rebased on `main`, and open one PR per epic. Merge it with a **rebase
  merge** so each task stays its own commit on `main`. Don't squash-merge an epic, and never make
  merge commits.
- Work outside an epic (docs, chores, small fixes) goes on its own short-lived branch and PR.
- Releases go through `scripts/release.sh prepare X.Y.Z`, then `tag X.Y.Z` once its PR is merged
  (README "Cutting a release", ADR-0052). Don't bump the version or push `v*` tags by hand.
  Nothing is committed straight to `main`.
- Use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `docs:`,
  `refactor:`, `test:`, `chore:` and so on.
- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before every commit, and make
  sure tests pass.

## Rust style

- Add dependencies with `cargo add` so you get the latest version. Never hand-write version
  numbers from memory.
- Behaviour lives on types and traits, not in free functions.
- Use the built-in traits (`From`/`TryFrom`, `FromStr`, `Display`, `Default`, `AsRef`, …) instead
  of ad-hoc `to_x()` / `from_x()` conversion functions.
- Every migration is reversible: create it with `sqlx migrate add -r <name>` and write a
  `.down.sql` that restores the previous schema exactly (ADR-0052; the store tests enforce it).
- Keep modules deep: a small, narrow public API that hides a substantial implementation. Avoid
  many thin modules that expose their internals.
