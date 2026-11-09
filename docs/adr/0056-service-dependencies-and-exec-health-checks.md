# ADR-0056: Services declare `depends_on`; `health.exec` runs a check inside the container

- Status: Accepted (amends ADR-0028, ADR-0038 and ADR-0048)
- Date: 2026-11-09

## Context

Stacks of third-party images do not start reliably. A first deploy of an app and a stop-first db
probes the app before the db exists (ADR-0048); after a reboot, a dependent that crashes on a
missing database burns crash-loop budget (ADR-0028). Such images rarely serve `/healthz`
(ADR-0051) and often show readiness only from inside (`pg_isready`), out of ADR-0038's reach.
Since hoster never sets `Healthcheck`, podman runs docker-format images' own checks on systemd
timers, which ADR-0038 rejected (spike: architecture §5.5).

## Decision

**1. `depends_on`.** `depends_on = ["db", "cache"]` names services of the same stack that must
pass their check before this one starts; there are no compose `condition` variants. A self
reference, an unknown name, a duplicate or a cycle is a validation error. It is not in the config
hash: changing it reorders starts and never recreates a container.

**2. Deploys (amends ADR-0048; architecture §5.1, §5.4).** States, events and edges stay:

1. The **start phase** is the changed start-first services plus the changed stop-first services
   without a previous generation (no gap). Held-back services (3) aside, `Starting` starts those
   with no in-phase dependency and `HealthChecking` starts each other one once those pass.
2. The **replace phase** (the other changed stop-first services) goes one at a time, in
   dependency order with name as tie-break.
3. A start-phase service that depends, even transitively, on a replace-phase service is **held
   back**: `Replacing` starts and probes it once that dependency has passed. Its old generation
   serves until `Switching`, but gaps close only there, so its start and probe add to the 503 gap
   of every replace-phase service already stopped.
4. An unchanged dependency must be running at the start and before each held-back or replaced
   dependent starts, or the deploy fails ("db (unchanged) is not running"); its monitor status is
   logged, not awaited. An unchanged service that ran at the start and exited while a replaced
   dependency was down is started again once that replacement passes, without a restart record,
   and probed within its `start_period` before any changed service depending on it starts; a
   failed probe aborts the deploy, like a dependency that is not running.
5. An edge to a never-run service outside the deployment is ignored, as in the reconciler, and
   logged ("api has never run; starting web without it"). There is no deploy-wide timeout.
6. `Aborting` restarts the containers it stopped in dependency order only: it ignores edges to
   services without a pointer, never waits for health, and leaves the rest to the reconciler.

**3. Reconciler and monitor (amends ADR-0028 and ADR-0038).** Outside a deployment, dependencies
and checks come from the last committed deployment's revision. A stopped current container starts
once each dependency's current container runs and has passed its check since hoster (re)started
it or first saw it running; a dependency without a pointer is ignored. One that is not running
holds its dependents without a bound, at no crash-loop cost (its own restarts are the visible
events). A running one that has not passed within min(`start_period`, 5 min) stops holding: the
dependent starts as it would have without the hold, and one reconciler-restart event per hold
records it ("started app although db has not passed its check"). `recover()` starts only ready
containers; the periodic pass starts the rest as startup starts (no restart record or backoff),
past their hold too. A dependency going down never stops its dependents ("waiting for db" in the
UI). A (re)start resets the monitor status to `Unknown`; failures within min(`start_period`,
5 min) write no event, and the first pass wakes the reconciler. `start_period` thus also bounds
the monitor's grace and the reconciler's hold (5 s for the settle check).

**4. `health.exec` (amends ADR-0038).** `exec = ["pg_isready", "-U", "app"]` is a non-empty argv
run through podman's exec API without a shell, as the container's user with its environment. At
most one of `http`, `tcp` and `exec` is set; `exec` works on any service and is the only probe a
non-routed service may declare. Its default `timeout` is 5 s; the other defaults are those of
http and tcp. Exit 0 passes. The check **cannot run** only when podman could not execute argv[0]:
a deploy aborts at once with podman's error, and the monitor reports `Unhealthy`. Any other exit,
a shell's 126/127 included, is an ordinary failure. On a routed service an exec check also needs
a TCP connect to the first route's loopback port. A probe fails at `timeout` whether or not the
exec ends; hoster then kills or abandons the exec (at most three per container, architecture
§5.5). The output tail, secrets redacted, goes to deployment events only.

**5. Create and hash.** Containers are created with `Healthcheck {Test: ["NONE"]}`, so podman never
runs image checks on its own timers. `exec` is hashed only when set; no existing hash changes.

## Alternatives considered

- **The image HEALTHCHECK as the default check:** podman drops it from OCI-format images
  (`vaultwarden/server` declares one; `podman image inspect` shows `HealthCheck: null`), so it
  needs a registry config-blob fetch, per-digest storage, offline-rollback special cases and a
  behaviour change for existing stacks. A later, separate, advisory feature may show it in the
  apply preview as a suggested `health.exec`, off the deploy path. Letting podman run it needs
  systemd user timers and sees nothing in OCI images.
- **Compose `condition` variants:** more to explain, and `service_started` waits for nothing.
- **Readiness by app retries only:** no code, but many images exit instead and burn budget.
- **Probing held-back dependents against the old dependency:** a shorter gap, but images that
  exit on a lost connection fail every such deploy.
- **An unbounded hold on a running dependency:** a stuck check strands dependents silently.
- **Strict waves** (start a level, probe all of it): easier to log, never faster.
- **Exec alone on routed services:** blind to the published port.
- **Requiring an unchanged dependency to be `Healthy`:** the monitor lags up to 30 s.
- **Holding dependents of never-run services:** `Plan` keeps no pending digest, so it is dropped.
- **One abandoned exec per container:** a hung check blocks probes until the container restarts.
- **A deploy-wide timeout:** fails deploys that would pass.
- **A podman 4.9 minimum for exec session removal:** rules out Debian 12; podman cleans up anyway.

## Consequences

- ADR-0028, ADR-0038 and ADR-0048 carry a status note; their text is otherwise unchanged.
  Containers created before this keep podman's timer check until they are recreated.
- ADR-0048's "the stop-first gap is the same length" and "a broken start-first image never causes
  a stop-first outage" no longer hold for held-back services: their start and probe add to the
  503 gap of every replace-phase service already stopped, and a broken one aborts after it.
- ADR-0055's decisions stand: a status kept at `Unknown` is no health change, the past-hold start
  is a reconciler-restart event, and `failed` names the dependents never started.
- `ContainerRuntime` gains `exec` and a raw libpod session removal; no podman minimum (§5.5).
- After a reboot each dependency level costs about its dependency's time to pass.
