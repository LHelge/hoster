# ADR-0059: Services may set `shm_size`, `tmpfs`, `init` and `limits`; OOM kills are named

- Status: Accepted (amends ADR-0019, ADR-0023, ADR-0028, ADR-0038 and ADR-0052)
- Date: 2026-11-10

## Context

ADR-0038 left resource limits out of v1. General third-party images (owner, 2026-11-09) need more:
Postgres outgrows the 62.5 MiB `/dev/shm`, `read_only` images need scratch space, some need an
init, and one runaway container can starve the rest. Spikes checked each field (architecture §1.8).

## Decision

**1. Runtime options (service keys).**
- `shm_size = "256MiB"` sizes `/dev/shm`. Unset keeps podman's 65536000 bytes.
- `tmpfs = { "/tmp" = "64MiB", "/var/cache/nginx" = "16MiB" }` maps a path to a size, mounted
  `rw,nosuid,nodev,mode=1777,size=<bytes>` (so non-root images can write), with no `noexec` and no
  per-mount options. A path that is not absolute and normalised, is `/` or `/dev/shm`, lies under
  `/proc`, `/sys` or `/dev`, or overlaps any other mount target of the service (volumes and any
  other declared mount) is an error.
- `init = true` runs podman's init (catatonit) as PID 1; `false` is unset. Nothing checks for the
  binary: a missing one fails at create, which aborts the deploy.

**2. Limits (`[services.<svc>.limits]`).** `memory = "512MiB"` is a hard limit with no swap,
`pids = 512` caps processes and threads, `cpus = 1.5` caps CPU time. No defaults: unset keeps
podman's (no memory or CPU cap, 2048 pids). hoster reserves no headroom for itself; the docs advise
leaving some. Validation warns when `shm_size` plus the tmpfs sizes reach `memory`, since that RAM
is charged to the limit. Start-first runs old and new together, so a service can use twice its
limits in the overlap; the stack-format reference says so. Errors: sizes below 1 MiB, `memory`
below 8 MiB (typo guards), `pids` below 1, `cpus` not finite or outside 0.01 (the kernel's
smallest quota) to 1024.

**3. `ByteSize`.** Every size is one `ByteSize(u64)` with `FromStr` and `Display`: an integer and
`B`, `KB`/`MB`/`GB` (10³, 10⁶, 10⁹) or `KiB`/`MiB`/`GiB` (2¹⁰, 2²⁰, 2³⁰), case-sensitive, no
fraction or space. A bare integer, Docker's `512m` or more than i64::MAX bytes (podman's field
type) is an error; the first two suggest `"512MiB"`. It displays in the largest unit that divides
it exactly, binary first.

**4. Create and hash (architecture §8).** `memory` sets `Memory` and `MemorySwap` to the same
bytes, `cpus` sets `NanoCpus` = round(cpus × 10⁹), the rest set `PidsLimit`, `ShmSize`, `Tmpfs` and
`Init: true`. Unset options stay unset; `Privileged` and `PublishAllPorts` stay pinned to `false`.
A change recreates the container through a normal deploy (ADR-0049). A key is hashed only when set,
so no existing hash changes (ADR-0056): sizes as bytes, `cpus` as nano-CPUs, `tmpfs` as a sorted
map; `init = false`, an empty `tmpfs` and an empty `limits` table count as unset.

**5. Cgroup delegation.**
- **Check.** A limit whose controller (`memory`, `pids`, `cpu`) is missing from doctor's file,
  `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/cgroup.controllers`, rejects the
  stack at apply (after the route check, before image resolution) and in a deployment's
  `Validating`, so CI deploys and rollbacks are covered. An unreadable file rejects only stacks
  that set a limit. `cpus` above the host's CPU count or `memory` above `MemTotal` is a warning.
- **One fix.** The error, doctor's `cgroup_delegation` hint and install.sh all name
  `/etc/systemd/system/user@<uid>.service.d/hoster-delegate.conf`, `[Service]`
  `Delegate=cpu memory pids`: hoster's user only, and only what limits use.
- **install.sh (amends ADR-0023 and ADR-0052).** After enabling linger, it writes that drop-in if
  the user manager lacks a controller (never over an existing file) and reloads systemd. A reload
  does not apply `Delegate=`, so if `hoster.service` is neither enabled nor active and no
  `hoster-*` container runs, it restarts `user@<uid>.service` before enabling `podman.socket`;
  otherwise it prints that the change applies when that unit restarts (stopping hoster and its
  containers) or at reboot. It installs `catatonit` where missing, by apt-get or dnf as podman;
  for other distributions the docs name it.

**6. OOM.** `State.OOMKilled` stays true across `start`, so hoster keeps the flag it last saw per
container (at create, before each start, when first seen running). On an exit hoster did not cause
(before a reconciler restart, ADR-0028, or while a deploy probes it), 137 with the flag true reads
"OOM-killed (limit 512MiB)", as an unsent SIGKILL is almost always the OOM killer; any other exit
where the flag turned true reads "exited <code> after an OOM kill (limit 512MiB)", or "(no memory
limit)". The restart event's message carries it, the restart record keeps the last one for the
crash-loop detail and warnings (amends ADR-0028), and a deploy aborts with it (amends ADR-0038).

## Alternatives considered

- **Defaults or required limits:** no one value suits both Postgres and a static site; required
  limits would invalidate every existing stack.
- **cgroup v1 support:** hoster already requires cgroup v2, and rootless v1 cannot set limits.
- **A wider allowlist:** `stop_signal`, `cap_add`, `cap_drop`, `sysctls`, `devices`, `privileged`,
  `ulimits`, `security_opt`, `ipc`/`pid` modes, OOM-killer tuning, cpuset and io limits,
  reservations and CPU shares stay unknown fields. Most reach host or kernel policy (ADR-0019);
  each can be added alone when an image needs it (`stop_signal` first).
- **`noexec` tmpfs, fixed or with an `exec = true` opt-in** (the task's recommendation): it blocks
  write-then-run payloads, but breaks images that unpack native libraries into `/tmp` (JVM), and
  with `read_only` podman's own `/run` and `/var/tmp` and any volume stay exec-allowed anyway.
- **A start-first peak note** (the task's info diagnostic): noise on every apply of a limited
  start-first service; the reference documents the overlap once.
- **Podman's default swap** (2 × `memory`): a limit that swaps is not a hard limit.
- **Docker's `512m`/`1g`:** ambiguous between decimal and binary units.
- **Live `podman update`:** bypasses the deploy's health gate and rollback, and the config hash
  would no longer describe the running container.
- **libpod `/info` `host.cgroupControllers` for the check:** it lists podman's own cgroup's
  controllers (`[]` from a cgroup without any, while a container started from there got all three
  limits). Compat `/info` reports `CpuCfsQuota` and `SwapLimit` false where both work.
- **A doctor hint for catatonit, no package:** `init` would fail on hosts install.sh set up.
- **OOM from podman events, `OOMKilled` alone, or recreating after an OOM:** podman emits no `oom`
  event; the sticky flag blames every later exit; recreating duplicates the deploy's create path.

## Consequences

- Stacks without the new keys keep their hash and podman's 2048 pids. `read_only` keeps podman's
  unsized tmpfs on `/tmp`, `/run` and `/var/tmp` except where `tmpfs` declares a path. All of it
  counts against `memory`; init and exec checks count against `pids`.
- A child OOM-killed in a running container shows only at exit, and an OOM in a run hoster did not
  see start is named only if it exits 137. ADR-0055's "OOM if podman reports it" narrows to these
  rules; the event's `meta` is unchanged.
- A controller undelegated after apply makes starts fail (HTTP 500, container left `created`); the
  reconciler records the start error as the reason, and doctor names the gap.
- `init` makes podman bind-mount its own init binary, a path podman picks and the stack file cannot
  name: the one exception to ADR-0019. No field names a host path, device or privilege.
