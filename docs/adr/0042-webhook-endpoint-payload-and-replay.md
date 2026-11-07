# ADR-0042: Deploy webhook: endpoint, payload, per-stack secrets, timestamp-window replay protection

- Status: Accepted (resolves OQ-11, OQ-12); authentication, secrets and replay parts superseded by ADR-0047
- Date: 2026-11-07

## Decision

- **Endpoint:** `POST https://<admin-host>/hooks/deploy`, served by the same Axum app as the UI
  (ADR-0031). It is authenticated by HMAC, not by a session.
- **Payload** (JSON, at most 64 KiB):

  ```json
  {
    "stack": "notes",
    "images": { "api": "sha256:…", "web": "sha256:…" },
    "tag": "v1.4.2",
    "source": { "repo": "acme/notes", "sha": "0f3c…", "run_id": 123456 },
    "timestamp": 1762520000
  }
  ```

  - All services listed in `images` deploy together as **one deployment**.
  - `tag` and `source` are informational and appear in the deploy history.
  - Unknown services are rejected.
  - A `stack_file` field is reserved and rejected (ADR-0027).
- **Signature:** `X-Hoster-Signature: sha256=<hex HMAC-SHA256 of the raw body>`. It is verified
  in constant time **before** the body is parsed.
- **Secrets are per stack.** hoster generates them, shows each one once in the UI, and supports
  rotation with a short overlap during which both secrets are valid. They are stored in the
  secret store (ADR-0029).
- **Digests must come from the stack's own repositories.** Each digest has to belong to the
  repository named in that service's `image`. hoster checks this by resolving `repo@digest` in the
  registry before deploying, so a leaked secret cannot deploy foreign images.
- **Replay protection is the timestamp window only.** `timestamp` is part of the signed body, and
  requests outside **±5 min** of server time are rejected.
- **Responses:** `200 {"result":"noop"}`, `202 {"deployment_id":…}`, or 4xx with a reason that
  never reveals secret details.

## Alternatives considered

- **One global secret**: a leak would affect every stack.
- **Timestamp plus nonce cache**: closes the replay window completely, at the cost of a little
  state.
- **Monotonic digest check**: blocks intentional rollbacks through CI.

## Consequences

- **Accepted risk:** a captured request can be replayed within 5 minutes. It only has an effect
  if a different deploy of that stack has happened in the meantime, which would then be rolled
  back to the replayed digest. TLS protects the request in transit, so capturing it requires
  access to CI logs or the runner.
- The server clock must be NTP-synced. `hoster doctor` checks this (`timedatectl`).
