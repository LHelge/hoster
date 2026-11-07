# ADR-0027: Stack files are applied through the web UI; webhooks carry only image digests

- Status: Accepted (resolves OQ-10)
- Date: 2026-11-07

## Context

A stack file defines routes, env, secrets references and volumes. The webhook secret lives in CI.
Whatever the webhook is allowed to change is what a leaked secret can change.

## Decision

- Stack files live in the app repository and are **applied through the web UI**. The admin
  uploads or pastes the file. hoster validates it (schema, collisions, tag resolution per
  ADR-0025), shows a diff against the current revision, and applies it on confirmation.
- Every applied revision is stored in SQLite with its raw TOML and hash, and the UI shows which
  revision is active.
- The deploy webhook carries **only image digests** for existing services. A payload field for a
  stack file is reserved, and rejected for now.

## Alternatives considered

- **Stack file in the webhook payload**: GitOps-like, but a leaked webhook secret could then
  change routes, env and volumes.
- **hoster pulls the stack file from the repo**: needs repo read credentials. It may come with
  the deferred GitHub App.

## Consequences

- Config changes are deliberate and manual. CI can only change which image runs.
- The file in the repo and the applied revision can drift. The UI shows the applied hash, so
  drift is visible.
