# Spike: systemd socket activation for :80/:443 across hoster restarts

- Task: Bears `f9t`, the follow-up named in ADR-0034
- Date: 2026-11-08
- Prototype: [`examples/socket_activation.rs`](../../examples/socket_activation.rs) (not wired into
  hoster; run it with `cargo run --example socket_activation -- --help`)
- **Recommendation: adopt**, as a small post-v1 change. This note is input for the owner's
  decision, not a decision. Adopting it needs a new ADR that amends ADR-0034.

## Question

ADR-0034 accepts a gap of 1–2 s in ingress while hoster restarts, during which clients get
`connection refused`. Could a `hoster.socket` user unit with `ListenStream=80`/`443` keep the
listening sockets open across restarts, so that clients wait in the kernel backlog instead? That
only works if Pingora 0.9 can serve on file descriptors it inherits instead of binding its own.

## Findings

### Pingora can adopt inherited fds, using only its public API

This is how pingora-core 0.9.1 handles listening sockets:

- `Server::run` passes every service a `ListenFds` table (`Arc<Mutex<Fds>>`, a map from bind
  string to `RawFd`). The table comes from the server's private `Bootstrap`. During a graceful
  upgrade, `Bootstrap::load_fds` fills it from the upgrade socket (`transfer_fd`).
- `ListenerEndpointBuilder::listen` (`listeners/l4.rs`) looks up the address string from
  `add_tcp`/`add_tls` in that table:
  - **On a hit** it takes the fd as is (`from_raw_fd`). It calls `listen(65535)` again, which on
    Linux only resizes the backlog.
  - **On a miss** it binds a new socket and adds it to the table.
- There is no public setter for the server's table. hoster does not need one. The `Service`
  trait's `start_service(fds: Option<ListenFds>, …)` is public. A wrapper service can therefore
  add the inherited fds to the table it is given, then delegate to the real `listening::Service`.
  The prototype's `Inherited<S>` does this in about 30 lines. Because the fds go into the server's
  own table, a later Pingora graceful upgrade would hand them on as well.

There are two pitfalls, and the prototype handles both:

1. **Blocking mode.** systemd passes sockets in blocking mode unless the unit sets
   `NonBlocking=yes`. Pingora's fd path assumes non-blocking sockets, because its upgrade path
   only ever moves sockets it created itself. tokio requires non-blocking sockets, and a blocking
   listener would stall a runtime worker inside `accept`. hoster must call
   `set_nonblocking(true)` on every inherited fd, or the unit must set `NonBlocking=yes` (or
   both).
2. **Key matching.** The table key must be byte-for-byte the string passed to `add_tcp`. The
   prototype reads each inherited socket's `local_addr()` and uses that string for both. A
   mismatch (say, `0.0.0.0:80` in config against a dual-stack `[::]:80` from `ListenStream=80`)
   would make Pingora bind its own socket and fail with `EADDRINUSE`.

### Measured client-visible behaviour

Setup:

- The prototype's `serve` runs as a transient user service with `Restart=always`, using
  `systemd-run --user`, on systemd 262 and Linux 7.2. It is plain HTTP on 127.0.0.1, one Pingora
  thread, and `grace_period_seconds = 0`.
- Startup is modelled by sleeping before `Server::run`: 1000 ms (ADR-0034's figure) unless noted.
- `probe` opens a fresh connection every 10 ms for 6 s and sends a `GET`. The restart or kill
  happens 1.5 s in.

```sh
# without socket activation (what hoster does today)
systemd-run --user --unit=hoster-spike-plain -p Restart=always \
  target/debug/examples/socket_activation serve --listen 127.0.0.1:18080 --startup-delay-ms 1000
# with socket activation
systemd-run --user --unit=hoster-spike-sa -p Restart=always \
  --socket-property=ListenStream=127.0.0.1:18081 \
  target/debug/examples/socket_activation serve --startup-delay-ms 1000
target/debug/examples/socket_activation probe 127.0.0.1:1808x --seconds 6 &
systemctl --user restart hoster-spike-…service      # or: systemctl --user kill -s KILL …
```

| Scenario | Startup | Refused | Other errors | Longest outage | Worst successful latency |
|---|---|---|---|---|---|
| Plain, `systemctl restart` | 1 s | 101 | 0 | 1017 ms | 0 ms |
| Plain, `SIGKILL` + `Restart=always` | 1 s | 123 | 0 | 1244 ms | 2 ms |
| Socket-activated, `systemctl restart` | 1 s | **0** | 0 | **0 ms** | 1018 ms |
| Socket-activated, `SIGKILL` + `Restart=always` | 1 s | **0** | 0 | **0 ms** | 1357 ms |
| Socket-activated, `systemctl restart` | 3 s | **0** | 0 | **0 ms** | 3019 ms |

Without socket activation, every connection made during the gap is refused. That is the
ADR-0034 gap, plus systemd's default `RestartSec=100ms` after a crash. With socket activation,
no connection fails. Connections made during the gap complete their TCP handshake in the kernel
and are answered once the new process accepts them, so the gap shows up only as latency equal to
the startup time.

What the measurement does not cover:

- **Ports 80/443 themselves.** This machine has `ip_unprivileged_port_start=1024`. With
  socket activation the *user* systemd instance binds the ports, so the ADR-0023 sysctl is still
  needed and still enough. Nothing else changes for low ports.
- **TLS.** It is not measured separately. The handshake starts only after `accept`, so :443
  queues the same way.
- **In-flight requests.** A request already being served when the old process exits is still cut,
  as before. Socket activation only covers connections that are not yet accepted, so the ADR-0034
  consequence "in-flight requests are cut" stands.
- **Backlog limits.** The kernel caps the backlog at `net.core.somaxconn` (4096 here; systemd's
  `Backlog=` defaults to it). Beyond that, SYNs are dropped and clients retransmit them after
  about 1 s, still without a refusal. A small VPS will not hit this in a 1–2 s gap.

## What adopting it would take

1. **proxy module** (the only code change of substance):
   - Read the inherited fds once at startup, before any threads exist, with
     `sd_notify::listen_fds`. `sd-notify` is already a dependency.
   - Set them non-blocking.
   - Match them to the `:80` and `:443` services by the **port** of each socket's `local_addr()`,
     and pass that `local_addr()` string (not the configured listen address) to `add_tcp` and the
     fd table, as the prototype does. Matching the full address against config would hit pitfall 2
     with the defaults: `http_listen` defaults to `0.0.0.0:80`, while the `hoster.socket` below
     (`ListenStream=80`) yields a dual-stack `[::]:80`. When the inherited address differs from
     the configured one, log it (and let `hoster doctor` report it).
     `FileDescriptorName=` names every fd of a socket unit the same, so names would need two
     socket units; matching by port needs only one.
   - Wrap both listening services as the prototype's `Inherited<S>` does.
   - Without `LISTEN_FDS`, behave exactly as today. That keeps tests, `cargo run` and installs
     without the socket unit working.
2. **`Proxy::spawn` pre-checks:**
   - Skip the `TcpListener::bind` busy-port pre-check for inherited addresses. Otherwise it fails
     with `EADDRINUSE` against systemd's socket.
   - Change `ProxyEvents::ready`. Its connect probe is meaningless with an inherited socket,
     because the kernel completes the handshake before Pingora listens. Inherited fds cannot fail
     to bind, so `ExecutionPhase::Running` is enough, or the wrapper can report once it has
     seeded the table.
3. **Units and install docs** (ADR-0023, `docs/install.md`):
   - A new `hoster.socket`:

     ```ini
     [Socket]
     ListenStream=80
     ListenStream=443
     NonBlocking=yes
     BindIPv6Only=both

     [Install]
     WantedBy=sockets.target
     ```

   - In `hoster.service`: `Requires=hoster.socket` and `After=hoster.socket`.
   - Operators must learn that `systemctl --user stop hoster` alone no longer stops ingress. The
     next connection re-activates the service, so a full stop is
     `systemctl --user stop hoster.socket hoster.service`.
4. **`hoster doctor`:** report whether hoster.socket is active, and whether hoster got its
   listeners from systemd or bound them itself.

## Side benefit

ADR-0023 lists a residual risk: another local process could grab :80/:443 while hoster
restarts. With socket activation that goes away, because systemd holds the sockets the whole
time.

## Recommendation

**Adopt it after v1.** Reasons:

- The client-visible gap goes from refused connections to delayed ones.
- It needs only public Pingora API, no new dependency and roughly 100 lines in the proxy module.
- It falls back to today's behaviour when the socket unit is absent.

It is not needed for v1, which ADR-0034 already accepts. The prototype stays an example and is
not merged into the proxy. Wiring it in waits for the owner's approval and a new ADR.
