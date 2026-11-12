# ADR-0066: A background sampler reads container CPU/memory and host CPU, memory and disk every 10 s and keeps an hour in memory

- Status: Accepted (amends ADR-0045; fills the resource usage ADR-0054 left out)
- Date: 2026-11-12

## Context

Every screen in `docs/design/` has a **HOST** card at the bottom of the sidebar (cpu, mem and
disk, each with a meter), and the stack overview's services table has CPU and Memory columns
(`StackDetail.dc.html`). ADR-0054 left both out because hoster had no source for them. Since
ADR-0059 a service can set `limits.memory` and `limits.cpus`, and the services card already shows
them, so usage can now be shown against them.

The sidebar is rendered on every signed-in page. Calling podman from there would put one or
more socket round trips on every page view, and a slow or missing podman would slow down or
break pages that have nothing to do with containers.

The owner decided the points marked *(owner)* below on 2026-11-12 (epic `qmw`). The rest is the
design that follows from them.

**What podman reports** (checked on 2026-11-12 against rootless podman 6.1.4, cgroup v2, through
the compat API at `/v1.44`, with throwaway `alpine:3.22` containers, `curl` on the socket and the
container's own cgroup files):

- `GET /containers/{id}/stats?stream=false&one-shot=true` answers in 3–5 ms. Without `one-shot`
  the answer has the same shape and is as fast. Two of about a dozen calls took 70–90 ms; the
  cause was not looked into. Podman does not wait for a second sample either way:
  `precpu_stats` is all zeros and `preread` is `0001-01-01T00:00:00Z`. A CPU rate therefore
  needs two calls.
- `cpu_stats.cpu_usage.total_usage` is cumulative CPU time in nanoseconds, equal to the cgroup's
  `cpu.stat` `usage_usec` × 1000. With one busy-looping thread, its delta over 3 s of wall time
  came to 99.3 % of one core, and `podman stats` showed 99.75 %.
- `memory_stats` has only `usage` and `limit`. There is no `stats` map (Docker's `memory.stat`
  copy) and no `max_usage`. `usage` is the cgroup's `memory.current` minus `inactive_file`, to
  within a few hundred KiB (the reads are not atomic). After writing a 200 MiB file in the
  container, `memory.current` rose to 205 MiB and `usage` stayed at 5.6 MiB. After writing 50 MiB
  to `/dev/shm`, `usage` rose by 50 MiB: tmpfs and shm pages count. `limit` is `memory.max`, or
  the host's `MemTotal` when the container has no limit, so an unset limit cannot be told apart
  from the reported number.
- For a container that exists but is not running, the endpoint answers HTTP **200** with an
  error body (`{"cause":"container is stopped","message":"container is stopped","response":500}`).
  bollard 0.21.1's `ContainerStatsResponse` has only optional fields and no
  `deny_unknown_fields`, so it would likely decode that body as an empty reading without an
  error. That was read from the types, not run through bollard. A missing container is a 404.
- Compat `/info` `DockerRootDir` is podman's storage root (`~/.local/share/containers/storage`).
  It is the same path as libpod's `store.graphRoot`, and named volumes live under it by default
  (`store.volumePath`). bollard exposes it as `SystemInfo::docker_root_dir`.

## Decision

**1. One background sampler** *(owner)*. A task in `src/stats/` ticks every **10 s**. Each tick
reads the stats of every running managed container and the host's CPU, memory and disk usage.
Pages read only its in-memory cache, never podman or `/proc`. The interval and the history
length are constants, not settings, as in ADR-0065.

**2. Container stats come from podman's compat stats endpoint through bollard** (ADR-0002), one
`stream=false`, `one_shot=true` call per running managed container. hoster does not read the
cgroup files itself. One tick makes one container list call (the `hoster.managed=true` label,
as the reconciler uses) and one stats call per running container. The stats calls run
concurrently, at most 8 at a time. The whole tick must finish within **5 s**, half the interval.
A call still open at that point counts as unknown. The ticker uses
`MissedTickBehavior::Skip`, so a tick that overruns is skipped, not queued. A reading with no
CPU or memory figure (the stopped-container body above) and a 404 are both "not running": the
container gets no figures for that tick, and nothing is logged.

**3. The memory figure is podman's `usage`**: the cgroup's memory minus inactive page cache.
That is what `podman stats` and `docker stats` show. It includes anonymous memory, tmpfs and
shm, and active page cache. It leaves out cache the kernel would reclaim first, before any OOM
kill, so it is the better measure of how close a container is to its `limits.memory`.
`memory.current` would let a container that just wrote a large file look full. Stats' `limit`
is not used. The limit shown is the service's `limits.memory` from its revision (ADR-0059),
because podman reports the host's `MemTotal` when there is no limit.

**4. Container CPU %** is the delta of `total_usage` between a container's two latest readings,
divided by the wall time between them (the sampler's monotonic clock when each answer arrived).
It is a **percent of one core**: `150%` is 1.5 cores, as `podman stats` and `top` show it. Where
the service sets `limits.cpus` (ADR-0059), the column shows it as `/ 150%` and a meter fills to
usage over that limit. Where no limit is set, there is no meter.

**5. Host figures** come from `Machine` (`src/machine.rs`):

- **CPU** is the busy fraction of all cores between two readings of `/proc/stat`'s aggregate
  `cpu` line, as a percent (`100%` = every core busy). Busy is everything but idle and iowait,
  so steal counts as busy: on a VPS, stolen time was not available to work. Guest time is
  already part of user and nice, so it is not counted twice.
- **Memory** is `MemTotal − MemAvailable` from `/proc/meminfo`, out of `MemTotal`.
- **Disk** *(owner: the filesystem of podman's storage root)* is `statvfs` on the storage root.
  That path comes from the engine's info (compat `DockerRootDir`, `EngineInfo`), read when the
  sampler starts and read again on later ticks while it is unknown. Used = size − space
  available to an unprivileged user (`f_blocks − f_bavail`, times `f_frsize`), so the blocks
  reserved for root count as used, since hoster cannot write to them. The size is `f_blocks`.
  The System page may also show the state dir's filesystem (`state_dir`) when it is a different
  one (a different `st_dev`).

**6. History** *(owner: about an hour, in memory, lost on restart)*. Each container and the host
have a fixed ring of **360 samples** (1 h at 10 s). Each sample holds its time and its figures,
and each figure may be unknown. A container's ring is keyed by container id and holds its stack
and service from the labels. It is kept while the container is listed, so a stopped container
gets unknown samples, and it is dropped once the container is no longer listed. A new
generation is a new container, so its line starts empty after a deploy. A sample is about
32 bytes, so a container's ring is about 11.5 KiB, and 50 containers plus the host take
under 600 KiB. Nothing is written to SQLite and there is no migration.

**7. Unknown values.** A container's first reading has no CPU figure, because there is no
earlier reading to take a delta from. A reading whose counter went backwards (a restarted
container gets a new cgroup) has none either. The host's first tick likewise has no CPU figure.
Before the first tick, and for any figure podman or `/proc` did not give, the value is
unknown: the UI shows `—` and an empty meter. When the runtime fails, the host figures are still
recorded and every container's are unknown. A failing runtime or engine info is logged once per
change of state (failing, recovered), not every tick. The sampler never fails a page.

**8. Change signal.** After each tick the sampler bumps a `tokio::sync::watch` counter, which
live pages can await.

**9. Live updates** *(owner)*:

- **Stack overview.** The page's existing SSE stream (`StackFeed`, `/stacks/{name}/events`) also
  wakes on the change signal. It then sends `<hx-partial>`s that replace the CPU and memory
  cells of the stack's containers, keyed by container id, as the dashboard stream already does
  for its events card. They are rendered from the sampler's cache alone. A tick never lists
  containers from podman, reads the store or re-sends the events card. Rows that appear or go
  away (a deploy) still change only when the page is reloaded, as they do today.
- **Sidebar.** The HOST card is its own fragment, served at a small authenticated GET endpoint.
  It polls that endpoint with HTMX every 10 s (`hx-trigger="every 10s"`) and swaps itself. Each
  open tab costs one cache read and one render every 10 s, with no second SSE connection.
  The poll needs a live session like any admin route but does not count as activity for the
  idle timeout (ADR-0031): it never refreshes the session's idle timer.

**10. Tones.** For a meter with a maximum (memory against `limits.memory`, CPU against
`limits.cpus`, host CPU, memory and disk): `Ok` below 80 %, `Warn` from 80 %, `Fail` from 95 %.
Figures without a maximum are `Neutral`. One 10 s sample decides the tone, so short host CPU
spikes will show as warn or bad for one interval. That is accepted, because the sparkline next
to the meter shows whether the load lasts.

**11. UI under the CSP** (ADR-0054 unchanged). Meters are the server-drawn `Meter`
(`src/web/ui.rs`), and the sparklines next to the host's cpu and mem and in the table's columns
are `Sparkline` (`src/web/charts.rs`). There are no inline styles, and new CSS goes in
`assets/hoster.css` with the existing tokens. `Sparkline` takes integers, so CPU is passed in
tenths of a percent and memory in bytes. Unknown samples are left out, so a gap closes up
rather than showing as a break. The line scales to its own maximum: it shows the shape of the
hour, and the meter shows the level.

## Alternatives considered

- **Sampling on page load** (a stats call per container on each render): the sidebar is on
  every page, so every page view would pay podman round trips and go slow when podman does.
  There would also be no CPU rate without a second call.
- **Sampling only while a page is open**: saves a few calls on an idle server, but the first
  view after a quiet period has no CPU figure and no history. A tick costs a handful of
  millisecond calls, so there is little to save.
- **5 s**: twice the calls for figures that change little. **30 s**: the meters lag
  noticeably, and an hour is only 120 points.
- **Latest value only**: no sparklines, which the owner wants next to the meters.
- **Persisted minute buckets like ADR-0062**: survive restarts, but need tables, a migration,
  a retention sweep and write load for something glanced at, not analysed. Persisted history
  would be a later proposal tied to traffic history.
- **Streaming `stats?stream=true`**: one long-lived connection per container, which has to be
  opened and closed as containers come and go, and podman then pushes a sample every second.
  One-shot calls cost a few ms each and need no connection bookkeeping.
- **Reading the cgroup files directly** (`cpu.stat`, `memory.current`, `memory.stat` under
  `State.CgroupPath`): no socket call, but it depends on the cgroup layout podman chooses
  (`libpod-<id>.scope` under `user@<uid>.service`), duplicates podman's accounting, and cannot be
  covered by `FakeRuntime`. Kept in reserve if the per-container calls ever cost too much.
- **libpod's all-container stats endpoint** (`/libpod/containers/stats?containers=…`): one call
  per tick instead of N, but bollard has no call for it, so hoster would need a raw libpod
  request (ADR-0002's escape hatch). Its `CPU` field is a percentage podman computes itself,
  not a counter. Not needed at the measured per-call cost.
- **The `sysinfo` crate for host figures**: a large dependency for three small `/proc` and
  `statvfs` reads that `Machine` already does in the same style, with fixture tests.
- **`memory.current` (cache included) as the memory figure**: overstates use and closeness to
  the limit, and differs from `podman stats`.
- **Stats' `limit` as the memory limit**: when no limit is set it is the host's `MemTotal`,
  which cannot be told apart from a real limit.
- **Host CPU without steal, or with iowait as busy**: hiding steal would make a starved VPS look
  idle. iowait is time spent waiting, not working.
- **The fullest of two filesystems, or two disk meters** (storage root and state dir): on a
  typical VPS both are on one filesystem. Where they are not, podman's images and volumes are
  what fill a disk. The System page can show the second one.
- **Figures computed on load only, without live updates**: the meters would freeze on any page
  left open, and the dashboard and stack pages are live everywhere else.
- **A metrics endpoint** (Prometheus): ADR-0045 rejected it, and it does not give the UI its
  figures.

## Consequences

- Every signed-in page shows host figures at most 10 s old without touching podman. A podman
  outage shows as `—` instead of a slow or broken page.
- A server with N running managed containers makes N + 1 podman calls every 10 s. Measured at
  3–5 ms per stats call on podman 6.1.4, with up to 8 in parallel, a few dozen containers stay
  well under the 5 s deadline.
- History is lost on restart and self-update (ADR-0052): sparklines start empty, and the first
  CPU figure appears after the second tick. ADR-0045's "statistics reset when hoster restarts"
  still holds for resource usage.
- A container's sparkline starts over at each deploy, because the ring follows the container,
  not the service.
- `ContainerRuntime` gains a stats call and `EngineInfo` the storage root. `Machine` gains CPU
  times, available memory and filesystem usage.
- Memory shown against a limit is podman's figure, not what the kernel compares with
  `memory.max`. It still includes active page cache, which the kernel can also reclaim, so a
  container with a busy file cache can show close to its limit without being at risk of an OOM
  kill. `podman stats` shows the same figure.
