---
id: "8vv"
title: "Flaky proxy::tests::websockets_pass_through: Pingora upgrade race (waiting for upstream)"
status: blocked
priority: P2
created: "2026-11-10T12:39:13.089690765Z"
updated: "2026-11-10T14:48:48.117486090Z"
tags:
  - proxy
  - tests
  - flaky
  - pingora
  - upstream
attempts: 1
---

`proxy::tests::websockets_pass_through` (src/proxy/tests.rs) failed once in the v0.4.0 release workflow (run 42803527823, x86_64-musl job, attempt 1) with `UnexpectedEof: early eof` at the `read_exact` of the echoed frame: the connection closed after the 101, before the echo.

## Status (2026-11-10)

Root cause found; it is a bug in Pingora, not in hoster or the test harness. **Nothing was changed in hoster.**

Owner's initial leaning: **wait for upstream**. Maybe open an issue on cloudflare/pingora and later submit a PR there, but not now. Until then the test can fail rarely under CPU contention; a rerun passes.

## Root cause

Pingora 0.9.1, `pingora-proxy/src/proxy_h1.rs` (same code on `main` as of 2026-11-10):

- l.122: `proxy_1to1` writes the request header upstream; the upstream can answer from here.
- l.152: `try_join!` polls the downstream half first, then the upstream half.
- l.486: the downstream half sees the empty request body of the upgrade `GET` and queues `HttpTask::Body(None, true)` for the upstream half.
- l.267: the upstream half's `tokio::select!` picks at random among ready branches.
- l.268-278: reading the 101 sets `upgraded = true` and resets `request_done = false`.
- l.326-333: taking the queued task sets `request_done = true`, and `if request_done && client_session.was_upgraded() { response_done = true; }` ends the loop.

Normal order (end-of-body, then 101) works. If the 101 is already readable on the first poll of the upstream half (proxy thread descheduled after l.122 while the backend answers on another core) and `select!` picks it first, the stale pre-upgrade end-of-body is then taken as the end of the upgraded stream and both connections are closed: the client sees 101, then EOF.

It is a real proxy bug for WebSocket upgrades: in production a rare immediate drop of a new WebSocket to a fast local upstream on a busy host. hoster cannot work around it: no `ProxyHttp` hook runs in the upstream half or can stop or reorder that initial task.

## Evidence

The one test run directly from the test binary, 24 at a time, pinned to one CPU beside two busy loops (`taskset -c 0`):

- stock Pingora 0.9.1: 2 / 600 failed
- instrumented at l.331, unfixed: 10 / 700 failed, the race marker logged in exactly those 10
- same build with the fix below: 0 / 700 failed, race marker logged 7 times

Unpinned (16 parallel + 8 busy loops on 4 cores): 400 / 400 passed. `--stress-count 200` alone passes.

## Fix (in Pingora)

```rust
body = rx.recv(), if !request_done => {
    let pre_upgrade_end = matches!(body, Some(HttpTask::Body(..)));
    match send_body_to1(client_session, body).await {
        Ok(send_done) => {
            request_done = send_done;
            if request_done && client_session.was_upgraded() {
                if pre_upgrade_end {
                    // The end of the HTTP request body, queued before the 101 was read:
                    // keep receiving the upgraded body from downstream.
                    request_done = false;
                } else {
                    // An upgraded request is terminated when either side is done
                    response_done = true;
                }
            }
        },
```

## Options if not waiting

1. Vendor patched `pingora-proxy` 0.9.1 in the repo with a `[patch.crates-io]` path entry (was the recommendation), removed once upstream ships a fix.
2. Fork cloudflare/pingora and pin `[patch.crates-io]` to the fixed rev.

Either needs an ADR / open question (dependency policy). With a patched Pingora, add a regression test that forces the bad order: a raw upstream that writes its 101 at accept, before reading the request (fails about half the time unpatched).

## To do when picked up again

- Check whether a newer Pingora release fixes it (look at the `rx.recv()` branch of `proxy_handle_upstream`); if so `cargo update` and close this.
- Otherwise file the upstream issue (draft below), and possibly a PR with the fix above.

## Upstream issue draft

**Title:** H1 upgrade: a freshly upgraded connection is closed when the 101 is read before the pre-upgrade end-of-body task

**Describe the bug**

With an HTTP/1.1 upstream, a WebSocket (or any `Upgrade`) request is occasionally closed right after the `101 Switching Protocols` has been forwarded to the client, before any upgraded bytes pass. The client sees the 101 and then EOF.

It is an ordering race in `proxy_handle_upstream` (`pingora-proxy/src/proxy_h1.rs`):

1. `proxy_1to1` writes the request header upstream, then starts both halves with `try_join!`.
2. On its first poll `proxy_handle_downstream` sees an empty request body and queues `HttpTask::Body(None, true)` for the upstream half.
3. `proxy_handle_upstream` then enters its `tokio::select!`. If the upstream has already answered (a fast upstream on loopback, and the proxy thread was descheduled between the header write and this poll), both branches are ready and `select!` picks one at random.
4. If it picks `read_response_task()` first, the 101 sets `upgraded = true` and is sent downstream. The next iteration takes the queued `Body(None, true)`: `send_body_to1` returns `true`, so `request_done = true`, and because `client_session.was_upgraded()` is now true, `response_done = true` as well. The loop exits and the upgraded connection is torn down. The task that ended it was the end of the *pre-upgrade* HTTP request body, not the end of the upgraded stream.

In the other order (`Body(None, true)` first, then the 101) the 101 branch resets `request_done = false` and everything works, which is why it is rare.

**Pingora info**

Pingora version: 0.9.1 (the same code is on `main`). OS: Linux x86_64.

**Steps to reproduce**

A proxy in front of a local WebSocket echo server; a client that upgrades, sends one frame and reads the echo. Run many copies pinned to one CPU beside a busy loop (`taskset -c 0`), so the proxy thread is preempted between writing the request header and first polling the upstream half. In our test suite that fails about 1 run in 70; under normal conditions it is far rarer (seen once in CI).

**Expected results**

The upgraded connection stays open until either side closes it.

**Observed results**

The proxy closes both connections right after forwarding the 101.

**Proposed fix**

Only an end of the *upgraded* request stream (an `UpgradedBody` end, or the pipe closing) should finish the response; a plain `Body` end consumed after the upgrade is the stale pre-upgrade end and should leave the request side open (code under "Fix" above). With this change the race still occurs under the same load (logged 7 times in 700 runs) and no run fails; without it 10 of 700 fail, each with the race logged.

References: src/proxy/tests.rs (`websockets_pass_through`), src/proxy/router.rs.