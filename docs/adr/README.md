# Architecture Decision Records

Each ADR records one decision: its context, the alternatives that were considered (including the
rejected ones) and its consequences. ADRs are immutable once accepted, with two exceptions: the
**Status** line may be updated to point at a superseding ADR, and a dated **Correction** section
may be appended when a factual claim in the record turns out to be wrong (see ADR-0025,
ADR-0041). A changed *decision* always gets a new ADR that supersedes the old one.

Undecided items live in [../open-questions.md](../open-questions.md). When one is resolved, it
becomes a new ADR here.

| ADR | Decision | Notable rejected alternatives |
|---|---|---|
| [0001](0001-single-binary-on-host-under-systemd.md) | One self-contained binary on the host under systemd | Running the orchestrator in a container |
| [0002](0002-rootless-podman-via-bollard.md) | Rootless podman socket via bollard | Rootful Docker, podman CLI |
| [0003](0003-images-built-in-ci-pushed-to-ghcr.md) | Images built in GitHub CI, pushed to ghcr | Building on the server |
| [0004](0004-hmac-signed-webhook-with-digest.md) | Signed webhook carrying the digest (HMAC part superseded by 0047) | Tag-only payloads, SSH from CI (GitHub App deferred) |
| [0005](0005-digest-polling-fallback-and-idempotency.md) | ghcr digest polling fallback; idempotent by digest | Webhook-only, delivery-ID dedupe |
| [0006](0006-stacks-with-per-stack-network.md) | Stacks of containers, one podman network per stack | Shared network, podman pods |
| [0007](0007-route-model-and-loopback-only-publishing.md) | Route model; only routed services publish, on 127.0.0.1, allocated ports | 0.0.0.0 + firewall, user-chosen ports |
| [0008](0008-embedded-pingora-proxy.md) | Embedded Pingora on its own thread, channels + ArcSwap | **Traefik/Caddy sidecar**, nginx reloads |
| [0009](0009-host-and-longest-segment-prefix-routing.md) | Host + longest prefix on segment boundaries | Raw string prefix, regex routes |
| [0010](0010-prefix-preservation-and-forwarded-headers.md) | Preserve prefix by default; strip opt-in; X-Forwarded-Prefix; Host unchanged | Strip by default, Host rewrite |
| [0011](0011-streaming-bodies-and-per-route-timeouts.md) | Streamed bodies; per-route timeouts | Buffering, global timeouts |
| [0012](0012-reject-cross-stack-route-collisions.md) | Reject cross-stack (host, prefix) collisions | Last-writer-wins, host-level ownership |
| [0013](0013-dedicated-hostnames-for-signature-stable-services.md) | Signature-stable services (S3) get their own hostname | **Path-routing S3** |
| [0014](0014-rustls-with-custom-cert-resolver.md) | rustls + custom `ResolvesServerCert` over ArcSwap SNI map | **BoringSSL certificate callback** |
| [0015](0015-acme-http01-via-instant-acme.md) | instant-acme, HTTP-01 on :80 (redirect otherwise), background renewal | TLS-ALPN-01, certbot |
| [0016](0016-persist-acme-state-defer-dns01.md) | Persist account key and certs on disk; DNS-01 deferred | In-memory, DNS-01 now |
| [0017](0017-zero-downtime-deploy-sequence.md) | Start → health-check → atomic swap → drain → remove | Recreate, fixed blue/green ports |
| [0018](0018-no-app-specific-features.md) | No app-specific features | Templates, built-in DBs |
| [0019](0019-no-container-runtime-access-for-apps.md) | No podman socket or nested containers for apps | **Giving apps a podman socket**, socket proxies |
| [0020](0020-single-host-only.md) | Single host only | Designing for clustering |
| [0021](0021-web-admin-ui-axum-askama-htmx-github-oauth.md) | Web admin UI: Axum + Askama + vendored HTMX 4; GitHub OAuth; admin handle from env | CLI-only, SPA, local accounts |
| [0022](0022-sqlite-via-sqlx-state-store.md) | State store: SQLite via sqlx (resolves OQ-01) | rusqlite, redb, JSON files, labels-only |
| [0023](0023-systemd-user-service-with-linger.md) | hoster runs as a systemd user service with linger and net.ipv4.ip_unprivileged_port_start=80 (resolves OQ-09) | see ADR |
| [0024](0024-config-toml-with-env-overrides.md) | Config is ~/.config/hoster/hoster.toml with HOSTER_* env overrides; secrets kept in separate files or credentials (resolves OQ-27) | see ADR |
| [0025](0025-registry-credentials-and-image-references.md) | Classic PAT with read:packages for ghcr, per-registry credentials in the secret store (resolves OQ-06) | see ADR |
| [0026](0026-upstream-port-allocation.md) | Round-robin port allocation over a configured range (default 20000-29999), persisted in SQLite (resolves OQ-20) | see ADR |
| [0027](0027-stacks-applied-via-ui-webhooks-carry-digests.md) | Stack files applied through the web UI; webhooks carry only image digests (resolves OQ-10) | see ADR |
| [0028](0028-hoster-reconciles-container-restarts.md) | hoster reconciles and restarts containers itself; podman restart policy off (resolves OQ-17) | see ADR |
| [0029](0029-secret-store-and-config-aware-idempotency.md) | Encrypted secret store in SQLite, injected as env; idempotency on digest + config hash (resolves OQ-02; "apply & redeploy" option refined by 0049) | see ADR |
| [0030](0030-named-volumes-and-documented-backups.md) | Named podman volumes kept on stack removal; backups documented with a hoster backup db command (resolves OQ-03) | see ADR |
| [0031](0031-admin-ui-exposure-identity-sessions.md) | Admin UI on a reserved host via Unix socket; GitHub handle compared; SQLite sessions with CSRF tokens; no break-glass (resolves OQ-05) | see ADR |
| [0032](0032-proxy-edge-behaviour.md) | Strict edge: no default cert, 421 on Host/SNI mismatch, 404 for unknown hosts on HTTP (resolves OQ-19) | see ADR |
| [0033](0033-protocol-support-v1.md) | v1 protocols: h1+h2 downstream, WebSockets, h1 upstream (h2c later) (resolves OQ-22) | see ADR |
| [0034](0034-hoster-restarts-accept-gap.md) | Accept a short ingress gap on hoster restart; spike systemd socket activation later (resolves OQ-18) | see ADR |
| [0035](0035-confirmed-implementation-details.md) | Confirmed implementation details from architecture.md (resolves OQ-28) | see ADR |
| [0036](0036-certificates-issued-at-apply-non-blocking.md) | Certificates requested at stack apply after a DNS check; deploys never wait (resolves OQ-16; DNS check advisory per 0051) | see ADR |
| [0037](0037-ring-crypto-provider-everywhere.md) | ring crypto provider everywhere (resolves OQ-21) | see ADR |
| [0038](0038-host-driven-health-probes-no-limits-v1.md) | hoster-driven HTTP/TCP health probes; no resource limits in v1 (resolves OQ-07; default path per 0051) | see ADR |
| [0039](0039-per-service-update-strategy.md) | Per-service update = start-first (default) or stop-first (resolves OQ-14; mixed-stack ordering superseded by 0048) | see ADR |
| [0040](0040-deploy-concurrency-and-coalescing.md) | One active deployment per stack; queue keeps only the newest target (resolves OQ-15) | see ADR |
| [0041](0041-rollback-and-garbage-collection.md) | Rollback by redeploying a previous deployment; keep the last 3 successful images (resolves OQ-08) | see ADR |
| [0042](0042-webhook-endpoint-payload-and-replay.md) | Deploy webhook endpoint and multi-service payload (auth parts superseded by 0047) (resolves OQ-11, OQ-12) | see ADR |
| [0043](0043-edge-triggered-poller.md) | Poller deploys only when a tracked tag's digest changes (resolves OQ-13; first-sighting rule per 0051) | see ADR |
| [0044](0044-container-logs-streamed-to-ui.md) | Container logs kept by podman and streamed live into the web UI (resolves OQ-04) | see ADR |
| [0045](0045-observability-in-ui-only.md) | Observability in the web UI only; no metrics endpoint or alerting in v1 (resolves OQ-26) | see ADR |
| [0046](0046-no-cross-stack-networking.md) | No direct cross-stack networking in v1 (resolves OQ-23) | see ADR |
| [0047](0047-oidc-authenticated-deploy-webhook.md) | Deploy webhook authenticated with GitHub Actions OIDC tokens only (resolves OQ-29) | see ADR |
| [0048](0048-atomic-stack-commit-mixed-update-strategies.md) | A deployment commits the whole stack at once; stop-first services are replaced after start-first ones are healthy (resolves OQ-30; supersedes ADR-0039's ordering) | Half-applied `Failed`, rejecting mixed deployments |
| [0049](0049-apply-always-redeploys.md) | Applying a revision or changing a secret redeploys the affected services immediately; no staged apply (resolves OQ-32; refines ADR-0029) | Next-deploy pickup, applied/deployed pointer pair |
| [0050](0050-orchestrator-handle-with-async-methods.md) | The orchestrator is an `Arc<Orchestrator>` with async methods and one internal lock, not a command-channel actor (resolves OQ-33) | Actor + `Command` enum, hybrid |
| [0051](0051-poller-bootstrap-advisory-dns-health-default-port.md) | Poller deploys on first sighting of a never-run service; advisory DNS check; default health `GET /healthz`; `PORT` injected; no host redirects in v1 (resolves OQ-34; amends 0036, 0038, 0043) | Hard DNS gate, prefix-following health default, `[[redirects]]` target |
| [0052](0052-release-install-and-self-update.md) | Semver tags release musl x86_64/aarch64 binaries with git-cliff notes; curlable idempotent `install.sh`; versioned binaries in `~/.local/lib/hoster` with self-update from the UI or CLI (checksum only, refused while deploying); reversible migrations, with rollback run by the newer binary (amends 0023) | Root-owned binary with sudo update, signing, auto-update, preflight/auto-revert |
| [0053](0053-lucide-icons-as-one-svg-sprite.md) | Admin UI icons are Lucide glyphs from `icondata`, named by meaning in one `Icon` enum and served as one content-hashed SVG sprite (`immutable`, one year) used with `<use>` so they take `currentColor` | Tabler/Heroicons, one file per icon with CSS masks, `<img>`, inline SVG per page, vendored SVG files |

## Template

```markdown
# ADR-NNNN: <decision>

- Status: Proposed | Accepted | Superseded by ADR-XXXX
- Date: YYYY-MM-DD

## Context
## Decision
## Alternatives considered
## Consequences
```
