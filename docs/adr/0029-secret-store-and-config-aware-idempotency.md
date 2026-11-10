# ADR-0029: Encrypted hoster secret store injected as env; config changes redeploy explicitly

- Status: Accepted (resolves OQ-02; refines ADR-0005); the optional "apply & redeploy" step is refined by ADR-0049: applying always redeploys; secrets can also be config files in the container's layer (not tmpfs) since ADR-0057
- Date: 2026-11-07

## Context

Apps need secrets such as database URLs and S3 keys. The stack file in the app repo may only
refer to them by name. hoster runs as a user service (ADR-0023). Separately: should a change to
config alone, with no new image, redeploy?

## Decision

**Secret store**

- Secrets are stored **encrypted at rest in SQLite** (ADR-0022) with an AEAD cipher
  (XChaCha20-Poly1305). Each secret has its own nonce. Associated data binds the ciphertext to
  the secret's name, so values can't be swapped between names.
- The key comes from a 0600 key file in `~/.config/hoster/` that hoster generates on first start.
  Where available (systemd ≥ 256), a user-scoped systemd credential can supply it instead. The
  key never lives in the state directory next to the DB.
- Secret names are namespaced per stack (`<stack>/<name>`). The stack file refers to them through
  `secrets = { ENV_NAME = "<stack>/<name>" }`.
- The web UI is **write-only** for values: set, replace, delete and list names with their
  last-changed time. Values are never shown again.
- Secrets are injected as **environment variables** at container create, and never logged.
- The same store holds hoster's own secrets, currently registry credentials (ADR-0025). Webhook
  secrets no longer exist (ADR-0047). The GitHub OAuth client secret stays in a file, because the
  store isn't needed before login.

**Config-aware idempotency (refines ADR-0005)**

- The idempotency key per service becomes **(image digest, config hash)**. The config hash covers
  the service's resolved spec: env, the secret *versions* it references, command, volumes,
  resources and routes.
- Webhook and poller deploys are still no-ops when both are unchanged.
- Applying a stack revision or saving a secret shows the affected services and offers **"apply &
  redeploy"**. Nothing redeploys silently in the background.

## Alternatives considered

- **Podman secrets**: the default file driver stores them unencrypted, and env-type secrets need
  the libpod API rather than the compat API.
- **A per-stack `.env` file over SSH**: plaintext, and no UI.
- **Digest-only idempotency**: config edits would wait silently until the next image deploy.

## Consequences

- **Back up the key file** separately from the DB. Without it, secrets cannot be recovered.
- Secrets are visible in `podman inspect` to the `hoster` user, an inherent limit of env
  injection. File-mounted secrets on tmpfs may come later, designed so they do not reopen host
  bind mounts (ADR-0019).
- Rotating a secret is "replace + redeploy". Rotating the key needs a re-encrypt command, to be
  added later.
