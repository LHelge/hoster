# ADR-0065: Cap client connections at the edge, in total and per address; raise the descriptor limit

- Status: Accepted (resolves OQ-39; amends ADR-0008, ADR-0032)
- Date: 2026-11-11

## Context

The survey of the embedded proxy against hostile clients (architecture §9.1) left client
connections unbounded except by the process's file-descriptor limit. The unit sets no
`LimitNOFILE=`, so systemd's soft default of 1024 applies. About a thousand idle connections from
one client, re-opened every 60 s, exhaust it. hoster survives `EMFILE`, but while it lasts it
accepts nothing on `:80` or `:443`, and the admin UI comes in through the same listeners
(ADR-0031). File and socket work elsewhere also fails (SQLite, the podman socket, ACME files,
upstream connections). A slow-but-moving request, or an `h2` connection that never sends the
preface (Pingora 0.9.1 has no timeout for the HTTP/2 handshake), costs one descriptor each, so
they are the same problem as long as connections are uncapped.

Pingora 0.9.1's `connection_filter` feature asks a `ConnectionFilter::should_accept(addr)` on
every accept, before TLS. It has no callback when a connection closes.

## Decision

- **Descriptor limit.** At startup hoster raises its soft `RLIMIT_NOFILE` to 16384, or to the
  hard limit if that is lower (systemd's default hard limit is 524288). A self-updated binary
  therefore gets it without a new unit. The unit also sets `LimitNOFILE=16384`.
- **Total cap.** The proxy refuses (closes at accept) a new client connection while the process
  has 4096 or more descriptors open, as `/proc/self/fd` counts them. If the hard limit kept the
  limit lower, the cap is a quarter of it. It is implemented in a `ConnectionFilter` on every
  proxy listener. The cap counts what actually runs out, so it needs no close hook, and the rest
  of the process always has about 12k descriptors left. Worst-case memory is about
  4096 × 80 KiB ≈ 320 MiB.
- **Per-address cap.** At most 256 open connections per client IPv4 address or IPv6 /64. A
  connection beyond it is refused at accept like one beyond the total. Counting needs to know when
  a connection ends. That can be a wrapper around Pingora's `ServerApp` that owns the keep-alive
  loop (count on entry, release on exit). A spike confirms that design, or an equivalent, before
  it is built. 256 leaves room for many users behind one carrier NAT.
- **Constants first.** The three numbers are constants. They become `hoster.toml` keys only if an
  operator needs to change them.
- A refused connection is counted, and the count is reported (log line at most once per
  interval, and on the System page), so a flood is visible.
- **Upstream report.** The missing HTTP/2 handshake timeout is reported to Pingora. Until it is
  fixed, such a connection costs one slot under the caps.
- **Not now:** request rate limiting (a separate decision if request floods to apps become a
  problem), a minimum transfer rate for request headers and bodies (Pingora only offers it for
  HTTP/1.1 responses, and it breaks slow uploads unless it is set per route), and a limit on
  requests per kept-alive connection (no harm observed).

## Alternatives considered

- **Leave it.** The 1024 default is an accidental cap that starves hoster itself. Rejected.
- **A total cap only.** One address could take every slot, the admin UI included. Rejected.
- **Counting connections in the filter.** That isn't possible without a close callback. Counting
  descriptors gives the total without one.
- **A minimum transfer rate.** With connections capped, a slow client costs one slot and no
  more, so it is not needed. Skipped.

## Consequences

- Under a flood, new clients are refused while existing connections, deploys, SQLite and podman
  keep working. A single address cannot lock out others or the admin UI.
- Architecture §9.1's "Many connections" and "never sends the preface" rows change from
  "nothing" to the caps, with tests in `src/proxy/tests/limits.rs`.
- The unit's `LimitNOFILE=` reaches existing installs when `install.sh` is re-run (ADR-0052).
  Until then, the startup raise covers them.
