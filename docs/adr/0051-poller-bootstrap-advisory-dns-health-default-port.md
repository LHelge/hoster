# ADR-0051: Poller deploys on first sighting of a never-run service; DNS check is advisory; default health path is `/healthz`; `PORT` is injected; no host redirects in v1

- Status: Accepted (resolves OQ-34; amends ADR-0043, ADR-0036 and ADR-0038); `OTEL_*` variables are injected beside `PORT` since ADR-0069
- Date: 2026-11-07

## Context

The pre-implementation review found four small gaps in accepted decisions and one feature request
(www → apex redirects). They are grouped into one record.

## Decision

**1. Poller first sighting (amends ADR-0043).** When the poller sees a tracked tag for the first
time, it deploys the observed digest **if the service has never run**; otherwise it records the
digest without deploying, as before. A rolled-back service has run, so rollback protection is
unchanged.

**2. DNS check before issuance (amends ADR-0036).** The check is **advisory**:

- hoster's own public addresses are the A/AAAA records of the admin host, resolved at startup and
  re-resolved hourly. `acme.public_addresses` in `hoster.toml` overrides them.
- A host whose records do not match is **still issued**, under the normal per-host backoff
  (5 min → 24 h, ADR-0035 #5). The mismatch is shown on the certificates page and in the stack's
  "TLS pending" status as the likely cause.
- Proxied CDNs therefore work without configuration, and a DNS typo costs a few failed
  validations rather than an indefinite deferral.

**3. Default health probe (amends ADR-0038).** For a routed service without `health`, the default
is HTTP `GET /healthz` on the first route's container port, expecting 2xx–3xx. Services whose health
endpoint lives elsewhere, including services mounted under a preserved prefix, declare `health`
explicitly. Non-routed services keep "running and not restarted for 5 s".

**4. `PORT` injection (stack format).** For a routed service with exactly one distinct container
port across its routes, hoster sets `PORT=<that port>` in the container environment unless `env`
already defines `PORT`. Services with several container ports or no routes get nothing. This is
documented next to `port`, together with the requirement that the app bind `0.0.0.0` or `::`.

**5. Host redirects: not in v1.** `www.example.com → https://example.com` is not expressible in a
stack file. Apps redirect themselves or declare both hosts as routes. HTTP → HTTPS redirects on :80
remain (ADR-0015, ADR-0032). The `Target::Redirect` sketch from OQ-34 is kept in the open-questions
history for later.

## Alternatives considered

- Keep ADR-0043's record-only first sighting: forces the first deploy of every poller-tracked
  service through a webhook or the UI.
- Hard DNS gate with addresses from config: rate-limit safe, but wrong behind CDNs and one more
  required setting.
- Default health path following the route prefix: convenient, but implicit; the owner preferred a
  fixed convention (`/healthz`, the Kubernetes probe convention) and explicit configuration otherwise.
- Not injecting `PORT`: zero magic, two places to keep in sync for every 12-factor image.
- `[[redirects]]` as a fourth `RouteTable` target: cheap, but no present need.

## Consequences

- `ADR-0043`, `ADR-0036` and `ADR-0038` carry a status note pointing here; their text is otherwise
  unchanged.
- `RouteTable` keeps three target kinds (architecture §6.1).
- The example stack in stack-format.md sets `health` explicitly for `web` (`/`) and `api`
  (`/api/healthz`), so it is unaffected by the new default.
- `hoster doctor` reports the resolved public addresses so an operator can see what the advisory
  check compares against.
