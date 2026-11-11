# Open questions

Status: OQ-38, OQ-39 and OQ-40 are open. All others are resolved (OQ-30–OQ-34 were raised by the pre-implementation review and decided the same day, 2026-11-07).

Resolved questions were decided by the project owner and recorded as an ADR. Each section keeps
a pointer to the ADR that resolved it. New open questions are added here with options and a
recommendation, and moved into an ADR once decided.

Index:

| # | Topic | Blocks milestone |
|---|---|---|
| [OQ-01](#oq-01-state-store) | ~~State store~~ **Resolved → ADR-0022 (SQLite via sqlx)** | — |
| [OQ-02](#oq-02-secrets-and-env-injection) | ~~Secrets and env injection~~ **Resolved → ADR-0029** | — |
| [OQ-03](#oq-03-persistent-volumes-and-backups) | ~~Persistent volumes and backups~~ **Resolved → ADR-0030** | — |
| [OQ-04](#oq-04-log-access) | ~~Log access~~ **Resolved → ADR-0044** | — |
| [OQ-05](#oq-05-admin-ui-exposure-sessions-and-identity-remaining-details) | ~~Admin UI exposure, sessions, identity (remaining details after ADR-0021)~~ **Resolved → ADR-0031** | — |
| [OQ-06](#oq-06-private-ghcr-pull-credentials) | ~~Private ghcr pull credentials~~ **Resolved → ADR-0025** | — |
| [OQ-07](#oq-07-health-checks-and-resource-limits) | ~~Health checks and resource limits~~ **Resolved → ADR-0038** | — |
| [OQ-08](#oq-08-rollback) | ~~Rollback~~ **Resolved → ADR-0041** | — |
| [OQ-09](#oq-09-systemd-setup-and-binding-80443) | ~~systemd setup and binding 80/443~~ **Resolved → ADR-0023** | — |
| [OQ-10](#oq-10-where-stack-definitions-live) | ~~Where stack definitions live~~ **Resolved → ADR-0027** | — |
| [OQ-11](#oq-11-webhook-exposure-payload-and-secret-scoping) | ~~Webhook exposure, payload, secret scoping~~ **Resolved → ADR-0042** | — |
| [OQ-12](#oq-12-webhook-replay-protection) | ~~Webhook replay protection~~ **Resolved → ADR-0042** | — |
| [OQ-13](#oq-13-poller-semantics) | ~~Poller semantics~~ **Resolved → ADR-0043** | — |
| [OQ-14](#oq-14-update-strategy-for-services-that-cannot-run-twice) | ~~Update strategy for services that can't run twice~~ **Resolved → ADR-0039** | — |
| [OQ-15](#oq-15-deploy-concurrency-and-coalescing) | ~~Deploy concurrency and coalescing~~ **Resolved → ADR-0040** | — |
| [OQ-16](#oq-16-certificate-issuance-vs-route-activation) | ~~Certificate issuance vs route activation~~ **Resolved → ADR-0036** | — |
| [OQ-17](#oq-17-who-restarts-containers-after-a-reboot-or-crash) | ~~Who restarts containers after reboot/crash~~ **Resolved → ADR-0028** | — |
| [OQ-18](#oq-18-restarting-and-upgrading-hoster-itself) | ~~Restarting/upgrading hoster itself~~ **Resolved → ADR-0034** | — |
| [OQ-19](#oq-19-unknown-hosts-snihost-mismatch-default-certificate) | ~~Unknown hosts, SNI/Host mismatch, default cert~~ **Resolved → ADR-0032** | — |
| [OQ-20](#oq-20-upstream-port-allocation) | ~~Upstream port allocation~~ **Resolved → ADR-0026** | — |
| [OQ-21](#oq-21-rustls-crypto-provider) | ~~rustls crypto provider~~ **Resolved → ADR-0037** | — |
| [OQ-22](#oq-22-protocol-support) | ~~Protocol support (h2, WebSocket, gRPC)~~ **Resolved → ADR-0033** | — |
| [OQ-23](#oq-23-cross-stack-communication) | ~~Cross-stack communication~~ **Resolved → ADR-0046** | — |
| [OQ-24](#oq-24-image-and-container-garbage-collection) | ~~Image/container garbage collection~~ **Resolved → ADR-0041** | — |
| [OQ-25](#oq-25-third-party-non-ghcr-images) | ~~Third-party (non-ghcr) images~~ **Resolved → ADR-0025** | — |
| [OQ-26](#oq-26-metrics-and-alerting) | ~~Metrics and alerting~~ **Resolved → ADR-0045** | — |
| [OQ-27](#oq-27-hoster-configuration-file) | ~~hoster's own configuration~~ **Resolved → ADR-0024** | — |
| [OQ-28](#oq-28-provisional-details-assumed-in-architecturemd) | ~~Provisional details assumed in architecture.md~~ **Resolved → ADR-0035** | — |
| [OQ-29](#oq-29-github-actions-oidc-instead-of-hmac-for-the-deploy-webhook) | ~~GitHub Actions OIDC instead of HMAC for the deploy webhook~~ **Resolved → ADR-0047** | — |
| [OQ-30](#oq-30-mixed-stop-firststart-first-ordering-and-atomic-stack-commit) | ~~Mixed stop-first/start-first ordering and atomic stack commit~~ **Resolved → ADR-0048** | — |
| [OQ-31](#oq-31-local-admin-path-break-glass) | ~~Local admin path (break-glass CLI over a second Unix socket)~~ **Resolved → ADR-0031 reaffirmed, no local path in v1** | — |
| [OQ-32](#oq-32-when-an-applied-but-not-redeployed-revision-takes-effect) | ~~When an applied-but-not-redeployed revision takes effect~~ **Resolved → ADR-0049 (apply always redeploys)** | — |
| [OQ-33](#oq-33-orchestrator-actor-or-shared-handle) | ~~Orchestrator: actor + Command enum, or shared handle with async methods~~ **Resolved → ADR-0050 (handle)** | — |
| [OQ-34](#oq-34-poller-bootstrap-advisory-dns-check-health-default-path-port-host-redirects) | ~~Poller bootstrap, advisory DNS check, health default path, `PORT` injection, host redirects~~ **Resolved → ADR-0051** | — |
| [OQ-35](#oq-35-ambiguous-podman-object-names-across-stacks) | ~~Ambiguous podman object names across stacks~~ **Resolved → ADR-0063 (reject at apply, label check kept)** | — |
| [OQ-38](#oq-38-client-supplied-forwarding-headers-at-the-edge) | Client-supplied forwarding headers at the edge (`X-Forwarded-For` appended, `Forwarded` and `X-Real-IP` passed through) | — |

---

## OQ-01: State store

**Resolved 2026-11-07 → [ADR-0022](adr/0022-sqlite-via-sqlx-state-store.md): SQLite through sqlx.**
The original options were rusqlite, redb, JSON files and labels-only. Podman labels remain a
secondary source for reconciliation, and certificates stay as files (ADR-0016).

## OQ-02: Secrets and env injection

**Resolved 2026-11-07 → [ADR-0029](adr/0029-secret-store-and-config-aware-idempotency.md): Encrypted secret store in SQLite, injected as env; idempotency on digest + config hash.**

## OQ-03: Persistent volumes and backups

**Resolved 2026-11-07 → [ADR-0030](adr/0030-named-volumes-and-documented-backups.md): Named podman volumes kept on stack removal; backups documented with a hoster backup db command.**

## OQ-04: Log access

**Resolved 2026-11-07 → [ADR-0044](adr/0044-container-logs-streamed-to-ui.md): Container logs kept by podman and streamed live into the web UI.**

## OQ-05: Admin UI exposure, sessions and identity (remaining details)

**Resolved 2026-11-07 → [ADR-0031](adr/0031-admin-ui-exposure-identity-sessions.md): Admin UI on a reserved host via Unix socket; GitHub handle compared; SQLite sessions with CSRF tokens; no break-glass.**

## OQ-06: Private ghcr pull credentials

**Resolved 2026-11-07 → [ADR-0025](adr/0025-registry-credentials-and-image-references.md): Classic PAT with read:packages for ghcr, per-registry credentials in the secret store.**

## OQ-07: Health checks and resource limits

**Resolved 2026-11-07 → [ADR-0038](adr/0038-host-driven-health-probes-no-limits-v1.md): hoster-driven HTTP/TCP health probes; no resource limits in v1.**

## OQ-08: Rollback

**Resolved 2026-11-07 → [ADR-0041](adr/0041-rollback-and-garbage-collection.md): Rollback by redeploying a previous deployment; keep the last 3 successful images.**

## OQ-09: systemd setup and binding 80/443

**Resolved 2026-11-07 → [ADR-0023](adr/0023-systemd-user-service-with-linger.md): hoster runs as a systemd user service with linger and net.ipv4.ip_unprivileged_port_start=80.**

## OQ-10: Where stack definitions live

**Resolved 2026-11-07 → [ADR-0027](adr/0027-stacks-applied-via-ui-webhooks-carry-digests.md): Stack files applied through the web UI; webhooks carry only image digests.**

## OQ-11: Webhook exposure, payload and secret scoping

**Resolved 2026-11-07 → [ADR-0042](adr/0042-webhook-endpoint-payload-and-replay.md) (endpoint and multi-service payload) and [ADR-0047](adr/0047-oidc-authenticated-deploy-webhook.md) (authentication: GitHub Actions OIDC, no shared secrets).**

## OQ-12: Webhook replay protection

**Resolved 2026-11-07 → [ADR-0047](adr/0047-oidc-authenticated-deploy-webhook.md): single-use `jti` plus token expiry; the ±5 min timestamp window of ADR-0042 is superseded.**

## OQ-13: Poller semantics

**Resolved 2026-11-07 → [ADR-0043](adr/0043-edge-triggered-poller.md): Poller deploys only when a tracked tag's digest changes.**

## OQ-14: Update strategy for services that cannot run twice

**Resolved 2026-11-07 → [ADR-0039](adr/0039-per-service-update-strategy.md): Per-service update = start-first (default) or stop-first.**

## OQ-15: Deploy concurrency and coalescing

**Resolved 2026-11-07 → [ADR-0040](adr/0040-deploy-concurrency-and-coalescing.md): One active deployment per stack; queue keeps only the newest target.**

## OQ-16: Certificate issuance vs route activation

**Resolved 2026-11-07 → [ADR-0036](adr/0036-certificates-issued-at-apply-non-blocking.md): Certificates requested at stack apply after a DNS check; deploys never wait.**

## OQ-17: Who restarts containers after a reboot or crash

**Resolved 2026-11-07 → [ADR-0028](adr/0028-hoster-reconciles-container-restarts.md): hoster reconciles and restarts containers itself; podman restart policy off.**

## OQ-18: Restarting and upgrading hoster itself

**Resolved 2026-11-07 → [ADR-0034](adr/0034-hoster-restarts-accept-gap.md): Accept a short ingress gap on hoster restart; spike systemd socket activation later.**

## OQ-19: Unknown hosts, SNI/Host mismatch, default certificate

**Resolved 2026-11-07 → [ADR-0032](adr/0032-proxy-edge-behaviour.md): Strict edge: no default cert, 421 on Host/SNI mismatch, 404 for unknown hosts on HTTP.**

## OQ-20: Upstream port allocation

**Resolved 2026-11-07 → [ADR-0026](adr/0026-upstream-port-allocation.md): Round-robin port allocation over a configured range (default 20000-29999), persisted in SQLite.**

## OQ-21: rustls crypto provider

**Resolved 2026-11-07 → [ADR-0037](adr/0037-ring-crypto-provider-everywhere.md): ring crypto provider everywhere.**

## OQ-22: Protocol support

**Resolved 2026-11-07 → [ADR-0033](adr/0033-protocol-support-v1.md): v1 protocols: h1+h2 downstream, WebSockets, h1 upstream (h2c later).**

## OQ-23: Cross-stack communication

**Resolved 2026-11-07 → [ADR-0046](adr/0046-no-cross-stack-networking.md): No direct cross-stack networking in v1.**

## OQ-24: Image and container garbage collection

**Resolved 2026-11-07 → [ADR-0041](adr/0041-rollback-and-garbage-collection.md): Rollback by redeploying a previous deployment; keep the last 3 successful images.**

## OQ-25: Third-party (non-ghcr) images

**Resolved 2026-11-07 → [ADR-0025](adr/0025-registry-credentials-and-image-references.md): Any registry; third-party tags resolved to a digest at stack apply.**

## OQ-26: Metrics and alerting

**Resolved 2026-11-07 → [ADR-0045](adr/0045-observability-in-ui-only.md): Observability in the web UI only; no metrics endpoint or alerting in v1.**

## OQ-27: hoster configuration file

**Resolved 2026-11-07 → [ADR-0024](adr/0024-config-toml-with-env-overrides.md): Config is ~/.config/hoster/hoster.toml with HOSTER_* env overrides; secrets kept in separate files or credentials.**

## OQ-28: Provisional details assumed in architecture.md

**Resolved 2026-11-07 → [ADR-0035](adr/0035-confirmed-implementation-details.md): Confirmed implementation details from architecture.md.**

## OQ-29: GitHub Actions OIDC instead of HMAC for the deploy webhook

**Resolved 2026-11-07 → [ADR-0047](adr/0047-oidc-authenticated-deploy-webhook.md): Deploy webhook authenticated with GitHub Actions OIDC tokens only.**

---

## OQ-30: Mixed stop-first/start-first ordering and atomic stack commit

**Resolved 2026-11-07 → [ADR-0048](adr/0048-atomic-stack-commit-mixed-update-strategies.md): option B. Start-first services start and pass health first, stop-first services are replaced in a `Replacing` step, then one swap and one transaction commit the whole stack.**

<details><summary>Original discussion</summary>

**Problem.** ADR-0039 says that in one deployment "stop-first services go first, then start-first
services", each following its own sequence. A stop-first service therefore commits (old stopped,
new swapped in, old removed) before the start-first services have passed their health checks. If a
start-first service then fails, the deployment ends `Failed` with the stop-first services at the
new generation and the rest at the old one. Nothing in architecture §5 describes that outcome, and
"redeploy the previous deployment" is the only way back.

**Options.**

- **A. Keep ADR-0039's order and define the outcome.** Document that stop-first services commit
  independently, record the deployment as `Failed (partially applied)` and list which services
  moved. Simple, but the stack's deployment is no longer one atomic unit.
- **B. Reverse the order so the whole stack commits in one swap** *(recommended)*:
  1. start and health-check every changed start-first service (no swap yet)
  2. for each changed stop-first service: publish `Unavailable` for its routes, stop old, start
     new, health-check
  3. one RouteTable publish and one transaction writing all per-service pointers
  4. drain and remove old containers
  
  The stop-first gap is the same length as before. A failure anywhere before step 3 leads to a
  clean `Aborting`: restart the stopped stop-first containers, remove all new containers, restore
  routes. Peak resource use is the same as ADR-0017 already accepts.
- **C. Reject deployments that change both kinds at once.** Explicit, but forces two deploys for
  the common "bump storage and api together" case.

**Recommendation:** B. It makes "a deployment is one atomic change to a stack" true for every
stack shape and removes a state that would otherwise need its own recovery story.

</details>

## OQ-31: Local admin path (break-glass)

**Resolved 2026-11-07 → option A: [ADR-0031](adr/0031-admin-ui-exposure-identity-sessions.md) stands.** No local socket or CLI in v1; during a GitHub outage the operator waits or works on the host directly. The owner accepted that a GitHub outage blocks deploys and rollbacks. Revisit after v1 if it bites. The dev-only loopback listener in `9st` is unaffected.

<details><summary>Original discussion</summary>

**Problem.** ADR-0031 says: if GitHub login is unavailable, use SSH, files and the DB directly.
A GitHub outage normally takes Actions and OAuth down together, so during one neither the webhook
nor the UI can deploy or roll back, and "the DB directly" means hand-editing SQLite while hoster
runs. ADR-0021 rejected a CLI as *the* admin interface; it did not rule one out as a supplement.
The Axum app already listens on a Unix socket for Pingora, so the cost of a local path is small.

**Options.**

- **A. No break-glass (status quo, ADR-0031).**
- **B. A second, local-only Unix socket plus CLI subcommands** *(recommended)*. Axum listens on
  `admin.sock` (for Pingora, session-authenticated) and `local.sock` (0600, no session: file
  permissions are the authentication, the same model as the podman socket). `hoster apply <file>`,
  `hoster deploy <stack> <svc>=<digest>…`, `hoster rollback <stack> <deployment>` and
  `hoster status` talk to `local.sock` over HTTP and reuse the UI handlers. Side benefits: M3 is
  usable and testable before the proxy and TLS exist; the e2e test (`4nb`) drives hoster without a
  browser; scripted/GitOps-style applies become possible without weakening ADR-0027 (the file is
  still applied by someone with shell access, not by CI).
- **C. One-time login link minted by a CLI.** Rejected in ADR-0031 as a credential-minting path;
  it also still needs the UI to be reachable.

**Recommendation:** B. It is a few hundred lines, reuses existing handlers, and turns the only
"edit the database by hand" procedure in the design into ordinary commands.

</details>

## OQ-32: When an applied-but-not-redeployed revision takes effect

**Resolved 2026-11-07 → [ADR-0049](adr/0049-apply-always-redeploys.md): option C. Applying a revision or changing a secret redeploys the affected services on confirmation; there is no staged apply, so the running config always equals the active revision outside a failed deployment.**

<details><summary>Original discussion</summary>

**Problem.** spec.md §6 and ADR-0029 promise that "config changes redeploy only when you confirm
them in the UI" and "nothing redeploys silently in the background". But the idempotency key is
(image digest, config hash), and a webhook is a no-op only when *both* are unchanged. After an
"apply" without "redeploy", a webhook carrying the digest that is already running (a CI re-run,
or a release that only changed another service) has a different config hash and therefore
deploys the pending revision. The two statements cannot both be true; the behaviour must be one or
the other.

**Options.**

- **A. Apply makes the revision active; the next deploy of any kind uses it** *(recommended)*.
  "Apply" = this is now the desired config; "apply & redeploy" = and do it now. A later webhook,
  poller or manual deploy for the stack builds its containers from the active revision, whatever
  changed. The UI shows "revision N applied, M services pending redeploy" so the state is visible.
  spec.md §6 is reworded to "config changes take effect at the next deploy of the stack, or
  immediately if you choose apply & redeploy".
- **B. Two pointers: applied revision and deployed revision; webhooks pin to the deployed one.**
  Keeps the promise literally, but a webhook then starts containers from a revision the admin has
  already replaced, which is surprising in the other direction, and every deploy has to carry a
  revision id.
- **C. Apply always redeploys.** Removes the distinction; the admin loses the ability to stage a
  config change for the next release.

**Recommendation:** A. It is what the (digest, config hash) key already implies, and "the next
release picks up the config you applied" is the behaviour most operators expect.

</details>

## OQ-33: Orchestrator: actor, or shared handle?

**Resolved 2026-11-07 → [ADR-0050](adr/0050-orchestrator-handle-with-async-methods.md): option B. `Arc<Orchestrator>` with async methods and one internal lock; deploy tasks call `transition()`. The `Command`/`DeployEvent`/`oneshot` channels are gone.**

<details><summary>Original discussion</summary>

**Problem.** architecture.md models the orchestrator as an actor: a single task reading a bounded
`mpsc<Command>`, replying through `oneshot`s, with deploy tasks reporting back over a second
channel. Every operation then exists three times: as an enum variant, as a reply type, and as the
handler arm. No ADR fixes this shape. ADR-0008 settles channels + ArcSwap for the *proxy*
boundary, and ADR-0035 #1 settles only that the orchestrator (not a deploy task) performs the
swap. CLAUDE.md asks for behaviour on types and traits.

**Options.**

- **A. Actor + Command enum (as documented).** Explicit serialization point; easy to log every
  command; natural back-pressure. Costs the triplication above and makes every call site async
  message plumbing.
- **B. `Orchestrator` handle with async methods** *(recommended)*. `Arc<Orchestrator>` holding
  `Mutex<DesiredState>` (short critical sections only), per-stack deploy slots
  (`HashMap<StackId, ActiveDeploy>` behind the same lock), the `ArcSwap<RouteTable>` writer, and
  the store. Axum handlers, the webhook, the poller and the reconciler call
  `orchestrator.deploy(..).await` etc. and get typed results directly. Deploy tasks are spawned by
  the handle and call back into it (`orchestrator.transition(id, event)`), which keeps the single
  writer: only methods on `Orchestrator` touch desired state or publish routes. Tests construct
  the struct with a `FakeRuntime` and call methods; no channels to drive.
- **C. Hybrid.** Handle for queries and deploy requests, a channel only for deploy-task events.
  Probably the shape B converges on anyway for the task → orchestrator direction.

**Recommendation:** B. Same invariants, less code, and the API is the type's methods rather than
an enum mirrored by a reply enum. If B is chosen, "orchestrator actor" in the docs becomes
"orchestrator".

</details>

## OQ-34: Poller bootstrap, advisory DNS check, health default path, `PORT`, host redirects

**Resolved 2026-11-07 → [ADR-0051](adr/0051-poller-bootstrap-advisory-dns-health-default-port.md).** 1: deploy on first sighting if the service never ran. 2: advisory DNS check, own addresses from the admin host (override `acme.public_addresses`). 3: default health path is `/healthz` (Kubernetes convention; declare `health` otherwise). 4: `PORT` injected for single-port routed services. 5: host redirects **not** in v1.

<details><summary>Original discussion</summary>

**1. Poller first sighting (ADR-0043).** A tag's first observed digest is recorded without
deploying, so a freshly applied stack with `track = "main"` and no digest yet is never deployed
by the poller; the first deploy must come from a webhook or the UI.
*Recommendation:* on first sighting, deploy if the service has never run; otherwise record only.
Rollback protection is unaffected because a rolled-back service has run.

**2. DNS check before issuance (ADR-0036).** The check compares A/AAAA records with "this
server's" addresses, which nothing defines. Behind a proxying CDN (Cloudflare orange-cloud) the
records point at the CDN while HTTP-01 still succeeds through it, so a hard gate defers forever.
*Recommendation:* hoster's own addresses are the A/AAAA records of the admin host, resolved at
startup (it must point at the VPS anyway), overridable by `acme.public_addresses` in config. The
check becomes advisory: a mismatch is shown in the UI as the likely cause and issuance is still
attempted under the normal backoff. Rate-limit cost is bounded by the backoff (5 min → 24 h).

**3. Default health probe path (ADR-0038).** `GET /` on the first route's port 404s for a service
mounted under a preserved prefix (`/api` without `strip_prefix`).
*Recommendation:* default path = the first route's prefix (`/api`) when the prefix is preserved,
`/` when it is stripped or the route is at the root. Expected status stays 2xx–3xx.

**4. `PORT` convenience (stack-format §2.2, §2.3).** The app's listen port is fixed by the image
and named in the route's `port`; hoster maps a loopback port onto it, so the app never needs to be
told anything. Many 12-factor images nevertheless read `PORT`, which today means writing both
`env = { PORT = "8080" }` and `port = 8080`.
*Recommendation:* for a routed service with exactly one distinct container port, inject
`PORT=<that port>` unless `env` already defines `PORT`. Generic convention, not an app-specific
feature (ADR-0018). Document next to `port` that the app must bind `0.0.0.0`/`::`, not
`127.0.0.1`, because rootless port forwarding cannot reach a loopback-bound listener.

**5. Host redirects (ADR-0032 edge table).** `example.com` and `www.example.com` to the same
service works with two routes, but there is no way to say "send www to the apex". HTTP → HTTPS is
already handled by the :80 service (308).
*Recommendation:* a stack-level `[[redirects]]` entry, `{ from = "www.example.com", to =
"https://example.com", status = 308 }`, that owns `(from, /)` in the collision check like a route,
gets a certificate like a route, and makes the :443 proxy answer a redirect preserving path and
query. Served from the `RouteTable` as a fourth `Target::Redirect { to, status }`, so it costs no
new mechanism. `status` defaults to 308; 307 allowed. No path rewriting beyond prepending `to`.

</details>

---

## OQ-35: Ambiguous podman object names across stacks

**Resolved 2026-11-11 → [ADR-0063](adr/0063-reject-cross-stack-podman-name-collisions.md).** A + D. Apply rejects a stack whose `<stack>-<service>` or `<stack>-<volume>` equals another stack's, counting the kept volumes of removed stacks. The volume label check (D), already implemented by then, stays. Names are unchanged. B was dropped because podman cannot rename existing volumes.

<details><summary>Original discussion</summary>

**Problem.** Stack, service and volume names may contain `-` (`Name`: `[a-z][a-z0-9-]{0,30}`), and
architecture §8 / ADR-0030 build podman names by joining them with `-`. Different stacks can
therefore produce the same name:

- Volumes: stack `a-b` volume `c` and stack `a` volume `b-c` are both `hoster-a-b-c`. The second
  stack silently mounts the first stack's data, which breaks the stack isolation of ADR-0006 and
  ADR-0046.
- Containers: stack `a-b` service `c` and stack `a` service `b-c` are both `hoster-a-b-c-g<n>`.
  The second deploy fails with a name `Conflict` that the reconciler would read as a leftover of
  its own interrupted deploy.
- Networks (`hoster-<stack>`) are unambiguous, since they carry only the stack name.

Found while reviewing task 3v6 (`VolumeMount::named`, `ServiceGeneration`).

**Options.**

- **A. Reject at apply.** Like route collisions (ADR-0012), apply refuses a stack whose container
  or volume names equal another stack's. Names stay as documented. Needs a cross-stack check in
  apply and still lets the first stack "squat" names.
- **B. Unambiguous separator.** Use a character `Name` cannot contain, e.g.
  `hoster-<stack>_<volume>` and `hoster-<stack>_<service>-g<n>` (podman allows `_`). Collisions
  become impossible; existing docs and examples change.
- **C. Forbid `-` in stack names.** Smallest change, but restricts the most visible name.
- **D. Ownership check only.** Keep names; label volumes `hoster.stack` at creation and refuse to
  mount a volume whose label names another stack. Prevents the data leak, not the container
  conflict.

**Recommendation.** B, together with D as defence in depth (volumes are labelled at creation in
any case, ADR-0030). It removes the ambiguity by construction instead of relying on a cross-stack
check.

</details>

## OQ-36: No file-system sandbox for the user unit on Ubuntu

**Resolved 2026-11-11 → [ADR-0064](adr/0064-accept-no-unit-file-system-sandbox-on-ubuntu.md).** A, accept the gap. The podman socket lets hoster bind-mount anything the `hoster` user owns, so no unit sandbox contains a compromised hoster. The `hoster` user is the boundary. The sandbox would only guard against wrong-path bugs, which doesn't justify relaxing a host-wide default (B) or moving to a system service (D).

<details><summary>Original discussion</summary>

**Problem.** On Ubuntu ≥ 23.10, `kernel.apparmor_restrict_unprivileged_userns = 1` gives the user
manager a user namespace without capabilities. The unit therefore cannot set up a mount
namespace. `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=`, `PrivateTmp=`,
`ProtectKernelTunables=` and `ProtectControlGroups=` are silently skipped, and hoster can write
wherever the `hoster` user can. Only `NoNewPrivileges=` and the seccomp filters apply.

Directives that drop capabilities made the unit fail outright (`218/CAPABILITIES`) and were
removed (ADR-0023 correction). Ubuntu is the most likely VPS image, and with self-update
(ADR-0052) the binary itself is writable by the service user. The file-system sandbox is what
should keep a compromised hoster away from the rest of `~hoster`, the unit and the credential
files.

Found on the first real install (Ubuntu 26.04, v0.1.0). The `unit_sandbox` check in
`hoster doctor` and `scripts/test-systemd-unit.sh` in CI report it.

**Options.**

- **A. Accept it on Ubuntu.** Keep the user unit, document the gap (done) and rely on seccomp,
  `NoNewPrivileges=` and Unix permissions. No work, but the main deployment target gets the
  weakest sandbox.
- **B. Lift the restriction host-wide.** Ship `kernel.apparmor_restrict_unprivileged_userns = 0`
  in `50-hoster.conf`, next to the low-port sysctl. Trivial, and the full sandbox works. It
  undoes an Ubuntu kernel-hardening default for every local user, the same trade-off ADR-0023
  already makes for low ports on a single-purpose VPS.
- **C. An AppArmor profile that grants `userns`.** Attached to `systemd-executor`, it would apply
  to every user unit on the host, so it amounts to B with more moving parts. Attached to the
  hoster binary, it doesn't help: the namespace is created by systemd before `exec`.
- **D. Run hoster as a system service with `User=hoster`.** This revisits the alternative ADR-0023
  rejected. The root system manager sets up the full sandbox without user namespaces, and
  `AmbientCapabilities=CAP_NET_BIND_SERVICE` can replace the low-port sysctl. The costs:
  - ordering against `user@<uid>.service`, plus waiting for the rootless podman socket;
  - root-owned unit and journal;
  - install.sh and the docs change.

**Recommendation.** D. It gives the strongest sandbox on every distribution, it drops the
low-ports-for-everyone sysctl, and it doesn't weaken host defaults. Until then, A, which is the
current state. B is a reasonable stopgap if the owner wants the sandbox on Ubuntu now.

</details>

## OQ-37: Notifications: channel, provider, configuration and which events

**Resolved 2026-11-10 → [ADR-0058](adr/0058-email-notifications.md): Email on problems and recoveries through a provider trait (Resend first); run-time settings in SQLite; a notifier over the event log.**

## OQ-38: Client-supplied forwarding headers at the edge

**Problem.** hoster is the edge: nothing trusted sits in front of it. `Forwarding::apply`
(`src/proxy/router.rs`) overwrites client-supplied `X-Forwarded-Prefix`, `-Proto` and `-Host` for
that reason (ADR-0010, architecture §6.3), but two things still let a client choose what an app
believes about the connection:

- **`X-Forwarded-For` is appended to.** The client's own values are kept and the peer address is
  added last. Every entry but the last is attacker-chosen. An app that reads the leftmost entry
  (a common default) or trusts a fixed number of hops other than one gets a spoofed client
  address for rate limits, allowlists and audit logs.
- **Other forwarding headers pass through untouched.** `Forwarded` (RFC 7239), `X-Real-IP`,
  `X-Forwarded-Port`, `X-Forwarded-Ssl`, `X-Forwarded-Scheme`, `X-Original-URL` and
  `X-Rewrite-URL` are neither set nor removed. Some frameworks prefer `Forwarded` or `X-Real-IP`
  over the `X-Forwarded-*` set hoster does control, and a few honour `X-Original-URL` for
  routing.

Found by reading the proxy code on 2026-11-10. hoster's own admin app reads none of these
headers.

**Options.**

- **A. Keep appending; document it.** The stack-format guide says that only the last
  `X-Forwarded-For` entry is hoster's and that the other headers are unfiltered. No code change;
  every app has to be configured for exactly one trusted hop.
- **B. Replace `X-Forwarded-For` with the peer address and strip the rest.** hoster sends exactly
  one `X-Forwarded-For` value and removes `Forwarded`, `X-Real-IP`, `X-Forwarded-Port`,
  `X-Forwarded-Ssl`, `X-Forwarded-Scheme`, `X-Original-URL` and `X-Rewrite-URL` from the request.
  Consistent with how the other three headers are treated. A host behind a CDN or another proxy
  loses the real client address, which today's append keeps (unauthenticated).
- **C. B, and also set `X-Real-IP` and `Forwarded`.** Apps that read those get a correct value
  instead of none. More headers to keep in step.
- **D. B, plus a `trusted_proxies` setting.** Forwarding headers are kept only when the peer
  address is in a configured list (a CDN's ranges). Covers the CDN case properly; a new
  `hoster.toml` key and a list to maintain.

**Recommendation.** B now. It matches the stated rule that incoming values are untrusted, and a
single-VPS install is normally reached directly. D when someone actually runs hoster behind a
CDN. The decision amends ADR-0010 and architecture §6.3.

## OQ-39: Connection caps and slow-client policy at the edge

**Problem.** The survey of the embedded proxy against hostile clients (task `95p`, architecture
§9.1) found bounds for most cases and set the timeouts whose value was obvious: 60 s for idle
keep-alive and HTTP/2 connections, and client read and write timeouts that follow the route's.
Four cases are left with no bound, and closing each of them is a policy choice:

1. **Connections are not capped**, per address or in total. hoster runs with systemd's default
   soft `RLIMIT_NOFILE` of 1024 (the unit sets no `LimitNOFILE=`). About a thousand idle
   connections, re-opened every 60 s by one client, exhaust it. hoster survives `EMFILE` and
   recovers when the connections close, but while it lasts it accepts nothing on `:80`, `:443`
   or the admin socket, and whatever else needs a descriptor fails (SQLite, the podman socket,
   ACME files, upstream connections). So one cheap client can stop ingress, the admin UI and
   deploys together.
2. **A slow-but-moving client is never cut.** Pingora's timeouts are per read or write. A request
   header or body sent a byte every few seconds lasts as long as the client likes; a body also
   keeps its request in flight on the upstream (a drain is still bounded by `drain_timeout`).
3. **An `h2` connection that never sends the preface is never closed.** Pingora 0.9.1 has no
   timeout around the HTTP/2 handshake, and `h2_idle_timeout` starts after it.
4. **Requests per connection are not limited** (`keepalive_request_limit`). No harm was observed;
   nginx caps it at 1000 to free per-connection memory.

Each connection in 2 and 3 costs one descriptor and a few tens of KiB, so they are the same
problem as 1 unless connections are capped.

**Options.**

- **A. Leave it.** The 1024-descriptor default is an accidental cap that also starves hoster
  itself. No work.
- **B. A total connection cap below the descriptor limit, and a higher `LimitNOFILE=`.** Pingora
  has a `connection_filter` feature (a `ConnectionFilter` asked on every accept) that can refuse
  beyond a count. Set `LimitNOFILE=` in the unit (for example 16384) and cap client connections
  well below it (for example half), so hoster always has descriptors left for upstream
  connections, the admin socket, SQLite and podman. A flood then makes the proxy refuse new
  clients but leaves the admin UI and deploys working. Worst-case memory is the cap times the
  per-connection cost (8192 × 80 KiB ≈ 640 MiB), so the cap has to fit the smallest supported
  VPS; that is the number to choose.
- **C. B plus a per-address cap** in the same filter (for example 100 connections per IPv4
  address or IPv6 /64). Stops a single client from using the whole total. It costs a map of
  counters, and it punishes many users behind one NAT if set too low.
- **D. Request rate limiting** per address (`pingora-limits`). A different problem (request
  floods to apps, not held connections), with more policy: which routes, which limits, what
  answer. It can be decided later on its own.
- **E. A minimum transfer rate.** `Session::set_min_send_rate` exists for HTTP/1.1 responses
  only. For request headers and bodies hoster would have to time them itself: a body in a
  `request_body_filter`, a header not at all (no hook runs before the header is complete). It
  breaks legitimately slow clients (large uploads on bad links) unless set per route.
- **F. Report 3 upstream** and wait for a fix, since no setting reaches it.

**Recommendation.** B and C together, with the numbers as constants first and `hoster.toml` keys
only if somebody needs them, and F. They turn the worst case from "everything stops" into "new
clients are refused", which is what a cap is for. Leave D for a separate decision. Skip E: with
connections capped, a slow client costs one slot and no more, and the per-route rate is hard to
get right. Leave 4 unset.

## OQ-40: Dot segments and other non-normal paths at the edge

**Problem.** The proxy matches and forwards the request path exactly as the client wrote it
(ADR-0009, architecture §6.2). It does not resolve `.` and `..`, merge `//` or decode `%2e` and
`%2f`. The adversarial tests of task `fq5` (`src/proxy/tests/smuggling.rs`) show two consequences:

- **No cross-service confusion.** A route is chosen by the literal leading segments, so no
  spelling reaches a service other than the one those segments name. `/x/../api/y` goes to the
  `/` route, not to `/api`.
- **A prefix is not a boundary inside a service.** `/api/../internal` matches the `/api` route
  and is forwarded with its `..` (as `/../internal` with `strip_prefix`). An app or framework
  that resolves dot segments then serves its own `/internal`, a path the stack file never
  exposed. The same holds for `%2e%2e` in an app that decodes before resolving. Whoever reads
  a route's `prefix` as "only these paths of the service are public" is wrong today, and nothing
  in the stack-format guide says so.

Browsers and HTTP libraries resolve dot segments before sending (RFC 3986 §5.2.4), so such paths
come from hand-written requests only.

**Options.**

- **A. Keep forwarding literally; document it.** One paragraph in the stack-format guide: a
  prefix selects a service, it does not restrict it. No behaviour change.
- **B. Refuse them.** Answer 400 to a path with a `.` or `..` segment, plain or percent-encoded
  (`%2e`, in either case). Legitimate clients never send one. Simple to state and to test, and
  it makes the prefix a real boundary for apps that resolve dot segments. It does not cover an
  app that decodes `%2f` into a separator, or `;`-parameters (`/api/..;/x`, Tomcat).
- **C. Normalise before matching** like nginx: resolve dot segments, merge slashes, then match
  and forward the normalised path. Changes what apps receive (S3-style signed paths depend on
  the exact bytes, ADR-0013), and proxy and app can still disagree about encodings.
- **D. B, plus refuse `//`, encoded slashes and backslashes.** Closes more app-specific
  spellings and breaks more legitimate paths (object keys with `//` or `%2F`).

**Recommendation.** B, with A's paragraph. It removes the common case without rewriting any
path an app sees, and what it refuses no legitimate client sends. C is how the big proxies do
it, but rewriting paths conflicts with ADR-0010's "forward what the client sent" and with
signed URLs.
