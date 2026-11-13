# Spike: how a container reaches a hoster listener under rootless podman

- Task: Bears `h4q`, for OQ-42 question 2 (decision task `yjp`, epic `xv9`)
- Date: 2026-11-13
- Run on: Arch Linux, kernel 7.2.14, podman 6.1.4, netavark, pasta 2026_10_02, rootless. One
  host only; other versions are from release notes and package indexes, marked "not run".
- Nothing was built. Throwaway Python listeners on the host, and `wget`/`curl` from
  `alpine:3.22` containers on a network and container created through the compat API the way
  `src/runtime/podman.rs` creates them (bridge driver, `hoster.managed` label, network mode set
  to the stack network, read-only root file system).
- **Recommendation: option A′**, the receiver as a route on the proxy's existing `:80`, matched
  by `Host: host.containers.internal` and a peer address that is the host's own. It needs no
  host-wide change, no new port, no TLS and no firewall rule. This note is input for `yjp`,
  not a decision.

## Results in short

| Option | Works | Host-wide change | What it costs |
|---|---|---|---|
| A. Own listener reached as `host.containers.internal:<port>` | Yes, on a wildcard or interface-address bind; **not on loopback** | None; a firewall rule to close the port from outside | A new port open on the public address, guarded by a peer check and the token |
| A′. The same name on the proxy's `:80`, as a route for local peers only | Yes (stand-in listener, not the real proxy) | None | A special case in the proxy's routing, caps and request log |
| B. Route that hairpins through the proxy, by public DNS | **No** on a host whose public address is on its interface | — | — |
| B′. The same, with the hostname pinned to `169.254.1.2` per container | Yes | None | A hostname and certificate, a CA bundle in every image, a TLS handshake per connection, the caps of ADR-0065 shared by all containers |
| C. Unix socket mounted into the container | On paper: no SDK takes one from the environment | None | A bind mount (ADR-0019), code in every app |

## How a container reaches the host at all

Every bridge network of a rootless user shares one network namespace, connected to the host by
one `pasta` process. Podman starts it as:

```text
pasta --config-net --map-guest-addr 169.254.1.2 --map-guest-addr fc00::2 --address fc00::3
      --gateway fc00::1 --dns-forward 169.254.1.1 -t none -u none -T none -U none --no-map-gw …
```

pasta copies the host's interface address into that namespace (here `ens18 10.10.1.50/24`,
seen with `podman unshare --rootless-netns ip -br addr`). Two things follow:

- **The host's own address is not the host from inside a container.** A connection to
  `10.10.1.50` ends in the namespace, where nothing listens, and is refused.
- **`169.254.1.2` is the host.** `--map-guest-addr` makes pasta open the connection from the
  host's namespace to the host's interface address. Podman writes
  `169.254.1.2 host.containers.internal host.docker.internal` into each container's
  `/etc/hosts`. The name is not in aardvark's DNS (`nslookup` says NXDOMAIN), so it resolves
  only through the hosts file, which every libc reads but a resolver that skips it would not.

## Option A: a listener reached as `host.containers.internal`

Listeners on the host, probed from a container on the stack-style network:

| Listener bound to | `host.containers.internal` (`169.254.1.2`) | Host's address `10.10.1.50` | Network gateway `10.89.3.1` |
|---|---|---|---|
| `127.0.0.1` | refused | refused | refused |
| `0.0.0.0` | **reached** | refused | refused |
| `10.10.1.50` | **reached** | refused | refused |
| `0.0.0.0`, `SO_BINDTODEVICE=lo` | refused | refused | — |
| `10.10.1.50`, `SO_BINDTODEVICE=lo` | refused | refused | — |

Answers to the task's questions:

- **Address the name resolves to:** `169.254.1.2`, a constant in podman, the same on every
  network and in a container created through the compat API.
- **Address hoster must bind:** the wildcard, or the interface address pasta copied (the one
  on the default route). **Loopback does not work.** The listener sees the connection arrive
  on the host's interface address with that same address as the peer (`peer 10.10.1.50 local
  10.10.1.50`), so it cannot tell containers or stacks apart by address; the token of OQ-42
  question 3 has to.
- **Binding to the loopback device does not help.** A socket bound with `SO_BINDTODEVICE=lo`
  (allowed without privileges since Linux 5.7) accepts `127.0.0.1` only. A local connection to
  the host's own interface address is refused too, although it travels over `lo`, because the
  kernel attributes it to the interface that owns the address (`net/ipv4/route.c`). So there
  is no bind that takes container traffic and is closed to the outside.
- **What keeps outsiders out, then:** the listener can refuse any peer whose address is not
  one of the host's own. A packet from outside that claims a local source address is dropped
  by the kernel as a martian. This check was reasoned, **not tested from a second machine**
  (none was available, and no root). An outside client still sees the port open and then
  closed. Any process on the host, and any container of any user on it, passes the check.
- **Other host services:** already reachable, with or without this option. From the test
  container, `host.containers.internal:22` (sshd on `0.0.0.0`), `:5432` (a Postgres on `*`)
  and `:8080` (bound to the interface address) were open. Services on `127.0.0.1` were not,
  which includes every upstream port hoster publishes (ADR-0007), so stacks stay isolated
  from each other. Option A adds no path; it uses one that exists. OQ-42's "containers have no
  path to the host" was wrong.
- **Reaching loopback instead** needs pasta's `--map-host-loopback`, set through
  `pasta_options` in `containers.conf`. That is a change for every container of the user, and
  it would open every `127.0.0.1` port to every container, upstream ports of other stacks
  included. Not tried, and not an option.
- **Podman versions (not run, from release notes and source):** `--map-guest-addr` is the
  default since podman 5.3.0 and covers rootless bridge networks. It needs passt 2024_08_14
  or later; with an older pasta podman retries without the option and the name points at an
  unreachable address or is missing, silently. Every distribution with podman 5.4 or later
  (ADR-0057) checked ships a newer passt: Debian 13 (podman 5.4.2, passt 2025_05_03), Ubuntu
  25.04 (5.4.1, 2025_02_17), 25.10 (5.4.2, 2025_05_03) and 26.04 (5.7.0, 2026_01_20).
  `host_containers_internal_ip` in `containers.conf` can override or remove the entry, so a
  doctor check that the name resolves to `169.254.1.2` in a container would be cheap.
- **IPv6:** stack networks have IPv6 off, so `fc00::2` is unreachable from them. The path is
  IPv4 only; a host with no IPv4 address was not tested.

Cost per export: none beyond a local TCP connection. A 20 kB `POST` took 0.8 ms on a new
connection and 0.3 ms on a reused one.

**Closing a dedicated port from outside** is possible, but not by hoster, which has no root.
Container traffic is a local connection on the host, so a default-deny inbound firewall that
accepts loopback traffic, or the provider's firewall, should block the port from outside and
leave containers working. Not tested (no root); it would be a step in `install.sh` or a
documented requirement.

## Option A′: the same name, as a route on the proxy's `:80`

Raised by the owner after the first results: let the proxy carry it, open to stacks only.
The proxy already listens on `0.0.0.0:80`, which containers reach as
`host.containers.internal` like any wildcard listener. The receiver becomes a route that
matches on two things: `Host: host.containers.internal`, and a peer address that is one of the
host's own. Anything else on that Host gets the 404 of an unknown host (ADR-0032).

Tried with a stand-in listener that makes that check, on `0.0.0.0` and on dual-stack `[::]`
(port 80 itself could not be bound on the test host, where `ip_unprivileged_port_start` is
1024, so the `Host` header carried a port that the check stripped):

| Request | Peer the listener saw | Answer |
|---|---|---|
| Container, `POST http://host.containers.internal:<port>/v1/traces` | `10.10.1.50` (`::ffff:10.10.1.50` on `[::]`) | 200 |
| Container, another `Host` to `169.254.1.2` | same | 404 |
| Host process, `Host` set, to `10.10.1.50` or `127.0.0.1` | `10.10.1.50`, `127.0.0.1` | 200 |

- **Endpoint:** `http://host.containers.internal`, port 80, so no port in the URL or the
  `Host` header. No extra-hosts entry, no certificate, no CA bundle, no public hostname.
- **Outside clients** cannot pass the peer check: a packet from outside with a local source
  address is dropped by the kernel as a martian. As for option A this was reasoned, **not
  tested from a second machine**.
- **The peer check is not identity.** Every container of every stack, every process on the
  host and other users' containers pass it. The token decides which service is speaking.
- **On a dual-stack listener** the peer is an IPv4-mapped IPv6 address and must be unmapped
  before it is compared.
- **The route must be served on `:80`,** not redirected to HTTPS as other known hosts are
  (ADR-0015), and handed to the receiver's Axum handlers as the admin host is (ADR-0031).
- **The caps of ADR-0065** count every container under the host's own address, so local
  peers need an exemption or a count of their own, or 256 connections are shared by all.
- **The request log** (ADR-0067) should leave these requests out.
- **Not run against the real proxy.** Whether Pingora's request filter can do this before the
  redirect is for the build task to show.

## Option B: a route that hairpins through the proxy

**By public DNS it does not work here.** The hostname resolves to the host's public address,
which inside the namespace is the namespace itself: `10.10.1.50:<port>` was refused for a
listener on `0.0.0.0`. That is every host whose public address is configured on its interface,
which is most VPS providers. Where the provider maps the public address 1:1 onto a private one
(AWS, GCP, Azure), the connection leaves the host and depends on the provider routing it back;
not tested.

This contradicts ADR-0046, which says stack A reaches stack B through B's public routes. It
is filed as proposed task `kf3` rather than fixed here.

**B′: pin the hostname to `169.254.1.2`.** `ExtraHosts: ["otel.hoster.test:host-gateway"]` on
the compat create call wrote `169.254.1.2 otel.hoster.test` into `/etc/hosts`, and the name
then reached a listener on `0.0.0.0`, as the proxy's `:443` is (`[::]:443`, dual-stack). No
host-wide change; one more field in the create request. Its costs:

- **TLS.** 4 ms to finish a handshake against a throwaway P-256 certificate, then 0.4–0.7 ms
  per `POST` on the kept connection against 0.3 ms plain. SDK exporters keep connections, so
  the handshake is paid once per connection, not per export.
- **A CA bundle in the image.** The exporter must trust the certificate. `scratch` and some
  minimal images have no roots, and the export then fails where plain HTTP would not.
- **A hostname.** Either one more hoster-owned host with its own certificate, or a path on the
  admin host (ADR-0031). The endpoint is then reachable from the internet like any route, and
  the token is its only guard unless the proxy also checks the peer.
- **The caps of ADR-0065.** Every container arrives from the host's own address, so all
  exports of all stacks share one 256-connection allowance and count against the 4096 total.
- **The request log** (ADR-0067) gets a row per export unless the route is excluded.

## Option C: a Unix socket mounted into the container (on paper)

- **No standard way to configure it.** The OTLP exporter specification requires an `http` or
  `https` URL for OTLP/HTTP, so `OTEL_EXPORTER_OTLP_ENDPOINT` cannot name a socket. The owner's
  answer to question 3, variables injected into every service, would not work with this option.
- **With code in the app**, the HTTP exporters of Go, Python, .NET, Rust and PHP take a custom
  HTTP client that could dial a socket; no primary source shows it done. Java and Ruby have no
  such hook. JS could not be confirmed either way. gRPC exporters accept `unix:` targets in
  some SDKs (Go), but OQ-42 question 4 chose HTTP only.
- **It needs a bind mount,** which ADR-0019 pins to none today, of a directory rather than the
  socket file so the socket survives a hoster restart, writable by whatever user the image
  runs as.

It gives the best isolation, a path to hoster and nothing else, and is unusable for
third-party images and for SDKs configured by environment.

## Recommendation

**Option A′.** Like A it works with the injected `OTEL_*` variables, in any image, without TLS
and without a host-wide change; unlike A it opens no port:

- Endpoint: `http://host.containers.internal`, on the proxy's `:80`.
- The proxy serves the receiver only for that `Host` from a peer that is one of the host's
  own addresses, and answers 404 otherwise. Authenticate with the per-service token
  regardless, since the peer check passes every local process and container.
- Local peers get their own treatment in the connection caps and stay out of the request log.
- Add a doctor check that a container resolves `host.containers.internal` to `169.254.1.2`.

Against the owner's two conditions for A (OQ-42, answers of 2026-11-12): it needs no host-wide
change, and it exposes no host service that containers cannot already reach. The owner
preferred A′ over a dedicated port when it came up (2026-11-13). Plain A is the fallback if the
proxy cannot make the check where it has to; it then wants a firewall rule for its port.

For `yjp` and `5up`: test the peer check from a second machine before relying on it, and
confirm on the real proxy that the route is matched before the HTTPS redirect.
