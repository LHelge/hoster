//! What the proxy does with clients that stall, flood or send too much (architecture §9.1).
//!
//! Each test pins rows of that section's table against the real proxy, so a Pingora or h2
//! upgrade that changes a bound fails here. Where the bound is hoster's own
//! ([`DownstreamLimits`], the route's timeouts) the tests shorten it; Pingora's fixed 60 s
//! waits are all observed in one slow test.

use std::net::{IpAddr, Ipv4Addr};

use super::*;
use crate::proxy::FileLimit;

/// Client limits short enough to wait for.
const SHORT: DownstreamLimits = DownstreamLimits {
    keepalive: Duration::from_secs(1),
    h2_idle: Duration::from_secs(1),
};

/// How much later than its timeout a bound may take effect on a loaded machine.
const SLACK: Duration = Duration::from_secs(5);

const KEEP_ALIVE_GET: &[u8] = b"GET /x HTTP/1.1\r\nHost: app.example.com\r\n\r\n";
const PARTIAL_HEAD: &[u8] = b"GET /x HTTP/1.1\r\nHost: app.example.com\r\n";
const WEBSOCKET_UPGRADE: &[u8] = b"GET /ws HTTP/1.1\r\nHost: stream.example.com\r\n\
    Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
    Sec-WebSocket-Version: 13\r\n\r\n";

/// How long until the server closes `stream`, or `None` if it is still open after `wait`.
/// Whatever the server sends first is dropped.
async fn closed_within<S: AsyncRead + Unpin>(stream: &mut S, wait: Duration) -> Option<Duration> {
    let started = Instant::now();
    let mut buf = [0; 4096];
    loop {
        let left = wait.checked_sub(started.elapsed())?;
        match tokio::time::timeout(left, stream.read(&mut buf)).await {
            Err(_) => return None,
            Ok(Ok(0) | Err(_)) => return Some(started.elapsed()),
            Ok(Ok(_)) => {}
        }
    }
}

/// Asserts that the server closed `stream` once `timeout` had passed, and not before.
async fn assert_closed_after<S: AsyncRead + Unpin>(what: &str, stream: &mut S, timeout: Duration) {
    let took = closed_within(stream, timeout + SLACK)
        .await
        .unwrap_or_else(|| panic!("{what}: still open {SLACK:?} after its {timeout:?} timeout"));
    assert!(
        took >= timeout.mul_f32(0.9),
        "{what}: closed after {took:?}, before its {timeout:?} timeout"
    );
}

/// Waits until `upstream` has `count` requests in flight.
async fn in_flight(upstream: &Upstream, count: u64) {
    let deadline = Instant::now() + SLACK;
    while upstream.inflight() != count {
        assert!(
            Instant::now() < deadline,
            "{} requests in flight, expected {count}",
            upstream.inflight()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One masked WebSocket text frame out, its unmasked echo back.
async fn websocket_echo<S: AsyncRead + AsyncWrite + Unpin>(client: &mut Client<S>) {
    let payload = b"hello";
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    client.write(&frame).await;
    let mut echo = [0; 7];
    tokio::time::timeout(SLACK, client.0.read_exact(&mut echo))
        .await
        .expect("echo frame")
        .unwrap();
    assert_eq!(&echo[2..], payload);
}

// ---------------------------------------------------------------------------------------------
// Idle and stalled connections

#[tokio::test]
async fn kept_alive_connections_are_closed_after_the_keepalive_timeout() {
    let h = Harness::start_limited(Timeouts::default(), SHORT).await;
    let h = &h;

    // Within the timeout the connection is reused.
    let mut reused = h.connect("app.example.com").await;
    for _ in 0..2 {
        reused.write(KEEP_ALIVE_GET).await;
        assert_eq!(reused.reply().await.status, 200);
        tokio::time::sleep(SHORT.keepalive / 4).await;
    }

    let idle = async {
        let mut client = h.connect("app.example.com").await;
        client.write(KEEP_ALIVE_GET).await;
        assert_eq!(client.reply().await.status, 200);
        assert_closed_after("idle after a request", &mut client.0, SHORT.keepalive).await;
    };
    // The next request header counts as idle time until it is complete.
    let partial = async {
        let mut client = h.connect("app.example.com").await;
        client.write(KEEP_ALIVE_GET).await;
        assert_eq!(client.reply().await.status, 200);
        client.write(PARTIAL_HEAD).await;
        assert_closed_after("half a second request", &mut client.0, SHORT.keepalive).await;
    };
    // The client does not get to pick the timeout.
    let asked = async {
        let mut client = h.connect("app.example.com").await;
        client
            .write(
                b"GET /x HTTP/1.1\r\nHost: app.example.com\r\nConnection: keep-alive\r\n\
                  Keep-Alive: timeout=3600\r\n\r\n",
            )
            .await;
        assert_eq!(client.reply().await.status, 200);
        assert_closed_after("Keep-Alive: timeout=3600", &mut client.0, SHORT.keepalive).await;
    };
    let plain = async {
        let mut client = h.connect_http().await;
        client.write(KEEP_ALIVE_GET).await;
        assert_eq!(client.reply().await.status, 308);
        assert_closed_after(":80 after a redirect", &mut client.0, SHORT.keepalive).await;
    };
    tokio::join!(idle, partial, asked, plain);
}

#[tokio::test]
async fn idle_h2_connections_are_closed_but_not_ones_with_a_stream() {
    let h = Harness::start_limited(Timeouts::default(), SHORT).await;

    let mut idle = H2Client::connect(&h, "app.example.com").await;
    let started = Instant::now();
    let received = idle.read_to_end(SHORT.h2_idle + SLACK).await;
    assert!(received.closed, "{received:?}");
    assert!(
        started.elapsed() >= SHORT.h2_idle.mul_f32(0.9),
        "{received:?}"
    );
    // Pingora drops the connection without a GOAWAY.
    assert_eq!(received.goaway(), None, "{received:?}");

    // A request slower than the idle timeout is answered: the connection is not idle.
    let mut busy = H2Client::connect(&h, "app.example.com").await;
    let slow = SHORT.h2_idle.as_millis() * 2;
    busy.send(&[Frame::get(1, "app.example.com", &format!("/slow/{slow}"))])
        .await
        .unwrap();
    let received = busy.read_until(SLACK, Frame::ends_stream).await;
    assert_eq!(received.status(1), Some(200), "{received:?}");
    assert_eq!(received.goaway(), None, "{received:?}");
}

/// A client that asks for a large response and never reads it is dropped after the route's
/// write timeout, which also takes its request off the upstream's in-flight count (a deploy's
/// drain waits for that count).
#[tokio::test]
async fn a_client_that_stops_reading_is_dropped_and_releases_its_upstream() {
    let timeouts = Timeouts {
        write: Duration::from_secs(1),
        ..Timeouts::default()
    };
    let h = Harness::start_limited(timeouts, DownstreamLimits::default()).await;

    let mut h1 = h.connect("app.example.com").await;
    h1.write(b"GET /download/268435456 HTTP/1.1\r\nHost: app.example.com\r\n\r\n")
        .await;
    in_flight(&h.web, 1).await;
    in_flight(&h.web, 0).await;
    // Reading now finds what was buffered and then the end, far short of the whole body.
    let mut left = Vec::new();
    tokio::time::timeout(SLACK, h1.0.read_to_end(&mut left))
        .await
        .expect("the connection was closed")
        .ok();
    assert!(left.len() < 64 * 1024 * 1024, "{} bytes", left.len());

    // HTTP/2: the client never widens its flow-control window.
    let mut h2 = H2Client::connect(&h, "app.example.com").await;
    h2.send(&[Frame::get(1, "app.example.com", "/download/268435456")])
        .await
        .unwrap();
    in_flight(&h.web, 1).await;
    in_flight(&h.web, 0).await;
    let received = h2
        .read_until(SLACK, |frame| frame.is(Frame::RST_STREAM))
        .await;
    let reset = received.of(Frame::RST_STREAM).next();
    assert_eq!(
        reset.and_then(Frame::error_code),
        Some(Frame::CANCEL),
        "{received:?}"
    );
}

/// A request body that stops arriving ends the request after the route's read timeout: on
/// HTTP/1.1 the client's stall (400) and the upstream's wait for the body (504) run out
/// together, on HTTP/2 Pingora has no body timeout and only the upstream's ends it.
#[tokio::test]
async fn a_request_body_that_stalls_ends_the_request() {
    let timeouts = Timeouts {
        read: Duration::from_secs(1),
        ..Timeouts::default()
    };
    let h = Harness::start_limited(timeouts, DownstreamLimits::default()).await;

    let mut h1 = h.connect("app.example.com").await;
    h1.write(b"POST /upload HTTP/1.1\r\nHost: app.example.com\r\nContent-Length: 100\r\n\r\nx")
        .await;
    in_flight(&h.web, 1).await;
    let reply = tokio::time::timeout(timeouts.read + SLACK, h1.reply())
        .await
        .expect("a reply once the read timeout has passed");
    assert!(matches!(reply.status, 400 | 504), "{reply:?}");
    assert_eq!(reply.header("connection"), Some("close"));
    assert!(closed_within(&mut h1.0, SLACK).await.is_some());
    in_flight(&h.web, 0).await;

    let mut h2 = H2Client::connect(&h, "app.example.com").await;
    let head = Frame::headers(
        1,
        false,
        &[
            (":method", "POST"),
            (":scheme", "https"),
            (":authority", "app.example.com"),
            (":path", "/upload"),
            ("content-length", "100"),
        ],
    );
    h2.send(&[head, Frame::data(1, false, b"x")]).await.unwrap();
    in_flight(&h.web, 1).await;
    let received = h2
        .read_until(timeouts.read + SLACK, Frame::ends_stream)
        .await;
    assert_eq!(received.status(1), Some(504), "{received:?}");
    in_flight(&h.web, 0).await;
}

/// The client-side timeouts follow the route's, so a route that allows long streams keeps a
/// quiet WebSocket open past the proxy-wide defaults.
#[tokio::test]
async fn a_routes_own_timeouts_keep_a_quiet_stream_open() {
    let timeouts = Timeouts {
        read: Duration::from_secs(1),
        write: Duration::from_secs(1),
        ..Timeouts::default()
    };
    let h = Harness::start_limited(timeouts, SHORT).await;
    let mut client = h.connect("stream.example.com").await;
    client.write(WEBSOCKET_UPGRADE).await;
    let (status, headers) = client.head().await;
    assert_eq!(status, 101, "{headers:?}");
    tokio::time::sleep(timeouts.read * 3).await;
    websocket_echo(&mut client).await;
}

/// Pingora's fixed waits, and hoster's defaults, observed at full length: a client that says
/// nothing is dropped after 60 s wherever it stops, and a kept-alive or HTTP/2 connection
/// after 60 s idle.
///
/// Two cases have no bound but the connection cap (ADR-0065), and are asserted so that a change
/// shows up here: a client that keeps sending a byte now and then, and one that negotiates `h2`
/// and never sends the connection preface.
#[tokio::test]
async fn a_silent_client_is_dropped_after_sixty_seconds() {
    const MINUTE: Duration = Duration::from_secs(60);
    assert_eq!(DownstreamLimits::default().keepalive, MINUTE);
    assert_eq!(DownstreamLimits::default().h2_idle, MINUTE);
    let h = Harness::start().await;
    let h = &h;

    let no_handshake = async {
        let mut tcp = TcpStream::connect(h.proxy).await.unwrap();
        assert_closed_after("TCP only on :443", &mut tcp, MINUTE).await;
    };
    let half_a_handshake = async {
        let mut tcp = TcpStream::connect(h.proxy).await.unwrap();
        // The start of a TLS handshake record that never completes.
        tcp.write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01])
            .await
            .unwrap();
        assert_closed_after("half a ClientHello", &mut tcp, MINUTE).await;
    };
    let no_request = async {
        let mut client = h.connect("app.example.com").await;
        assert_closed_after("TLS and no request", &mut client.0, MINUTE).await;
    };
    let no_request_plain = async {
        let mut tcp = TcpStream::connect(h.http).await.unwrap();
        assert_closed_after("TCP only on :80", &mut tcp, MINUTE).await;
    };
    let half_a_header = async {
        let mut client = h.connect("app.example.com").await;
        client.write(PARTIAL_HEAD).await;
        assert_closed_after("half a request header", &mut client.0, MINUTE).await;
    };
    let half_a_header_plain = async {
        let mut client = h.connect_http().await;
        client.write(PARTIAL_HEAD).await;
        assert_closed_after("half a request header on :80", &mut client.0, MINUTE).await;
    };
    let kept_alive = async {
        let mut client = h.connect("app.example.com").await;
        client.write(KEEP_ALIVE_GET).await;
        assert_eq!(client.reply().await.status, 200);
        assert_closed_after("idle after a request", &mut client.0, MINUTE).await;
    };
    let idle_h2 = async {
        let mut client = H2Client::connect(h, "app.example.com").await;
        assert_closed_after("idle HTTP/2", &mut client.0, MINUTE).await;
    };
    // A route with longer timeouts keeps a quiet WebSocket past the minute.
    let quiet_stream = async {
        let mut client = h.connect("stream.example.com").await;
        client.write(WEBSOCKET_UPGRADE).await;
        assert_eq!(client.head().await.0, 101);
        assert_eq!(closed_within(&mut client.0, MINUTE + SLACK).await, None);
        websocket_echo(&mut client).await;
    };

    // Not bounded: each byte restarts the wait.
    let dribbled_header = async {
        let mut client = h.connect("app.example.com").await;
        client.write(PARTIAL_HEAD).await;
        for _ in 0..4 {
            assert_eq!(closed_within(&mut client.0, MINUTE / 3).await, None);
            client.write(b"X-Slow: 1\r\n").await;
        }
    };
    let dribbled_body = async {
        let mut client = h.connect("app.example.com").await;
        client
            .write(b"POST /upload HTTP/1.1\r\nHost: app.example.com\r\nContent-Length: 100\r\n\r\n")
            .await;
        for _ in 0..4 {
            client.write(b"x").await;
            assert_eq!(closed_within(&mut client.0, MINUTE / 3).await, None);
        }
    };
    // Not bounded: the HTTP/2 idle timeout starts only once the preface has arrived.
    let no_preface = async {
        let mut client = H2Client::handshake(h, "app.example.com").await;
        assert_eq!(closed_within(&mut client.0, MINUTE + SLACK).await, None);
    };

    tokio::join!(
        no_handshake,
        half_a_handshake,
        no_request,
        no_request_plain,
        half_a_header,
        half_a_header_plain,
        kept_alive,
        idle_h2,
        quiet_stream,
        dribbled_header,
        dribbled_body,
        no_preface,
    );
}

// ---------------------------------------------------------------------------------------------
// Connection caps

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// Another client address: the whole of 127/8 is loopback on Linux.
const NEIGHBOUR: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

/// A TCP connection to `addr` from the client address `from`.
async fn connect_from(from: IpAddr, addr: SocketAddr) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(from, 0)).unwrap();
    socket.connect(addr).await.unwrap()
}

/// Whether `:80` takes a new connection from `from` on: it answers a request on it, or closes
/// it at once.
async fn plain_accepted_from(h: &Harness, from: IpAddr) -> Option<TcpStream> {
    let mut tcp = connect_from(from, h.http).await;
    // A refused connection may be closed before the request is written.
    let _ = tcp.write_all(KEEP_ALIVE_GET).await;
    let mut buf = [0; 64];
    match tokio::time::timeout(SLACK, tcp.read(&mut buf)).await {
        Ok(Ok(0) | Err(_)) => None,
        Ok(Ok(_)) => Some(tcp),
        Err(_) => panic!("neither answered nor closed"),
    }
}

async fn plain_accepted(h: &Harness) -> Option<TcpStream> {
    plain_accepted_from(h, LOCALHOST).await
}

/// Whether `:443` completes a TLS handshake for a new connection from `from`.
async fn tls_accepted_from(h: &Harness, from: IpAddr) -> bool {
    let tcp = connect_from(from, h.proxy).await;
    let name = ServerName::try_from("app.example.com").unwrap();
    tokio::time::timeout(SLACK, h.tls.connect(name, tcp))
        .await
        .expect("a handshake or a close")
        .is_ok()
}

/// Waits until `ip` holds `open` slots under the per-address cap, and fails if it does not
/// within [`SLACK`]. At zero no address is left in the map.
async fn slots_settle(h: &Harness, ip: IpAddr, open: usize) {
    let deadline = Instant::now() + SLACK;
    while h.admission.open_from_ip(ip) != open {
        assert!(
            Instant::now() < deadline,
            "{} connections counted from {ip}, expected {open}",
            h.admission.open_from_ip(ip)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if open == 0 {
        assert_eq!(
            h.admission.clients(),
            0,
            "an address stayed in the map at zero"
        );
    }
}

/// At the descriptor cap (ADR-0065) new clients are refused on both listeners, while the
/// connections already open, the admin host's among them, keep working; once connections
/// close, new ones are taken on again.
#[tokio::test]
async fn new_clients_are_refused_at_the_descriptor_cap() {
    const HEADROOM: usize = 40;
    let admission = Admission::new(usize::MAX);
    let h =
        Harness::start_admitting(Timeouts::default(), DownstreamLimits::default(), admission).await;
    let mut app = h.connect("app.example.com").await;
    app.write(KEEP_ALIVE_GET).await;
    assert_eq!(app.reply().await.status, 200);
    let mut admin = h.connect("hoster.example.com").await;
    let healthz = b"GET /healthz HTTP/1.1\r\nHost: hoster.example.com\r\n\r\n";
    admin.write(healthz).await;
    assert_eq!(admin.reply().await.status, 200);

    let open = h.admission.usage().open.expect("/proc/self/fd is readable");
    h.admission.set_cap(open + HEADROOM);
    // The next accept counts afresh.
    tokio::time::sleep(Admission::REFRESH).await;

    // Each connection costs this process two descriptors, the client's and the proxy's.
    let mut held = Vec::new();
    while let Some(tcp) = plain_accepted(&h).await {
        held.push(tcp);
        assert!(held.len() <= HEADROOM, "still accepting past the cap");
    }
    assert!(held.len() >= HEADROOM / 4, "refused after {}", held.len());
    assert_eq!(h.admission.refused(), 1);
    // `:443` refuses before the TLS handshake.
    assert!(h.handshake(&h.tls, "app.example.com").await.is_err());
    assert_eq!(h.admission.refused(), 2);

    // What is open keeps working.
    app.write(KEEP_ALIVE_GET).await;
    assert_eq!(app.reply().await.status, 200);
    admin.write(healthz).await;
    assert_eq!(admin.reply().await.status, 200);

    drop(held);
    let deadline = Instant::now() + SLACK;
    while plain_accepted(&h).await.is_none() {
        assert!(
            Instant::now() < deadline,
            "still refused after the connections closed"
        );
        tokio::time::sleep(Admission::REFRESH).await;
    }
    assert_eq!(h.get("app.example.com", "/after").await.status, 200);
}

/// One address may hold [`Admission::PER_ADDRESS`] connections (ADR-0065): the next one is
/// refused on both listeners, while another address still connects and what is open keeps
/// working; closing one lets the address in again.
#[tokio::test]
async fn the_connection_beyond_the_per_address_cap_is_refused() {
    let _ = FileLimit::raise();
    let h = Harness::start_admitting(
        Timeouts::default(),
        DownstreamLimits::default(),
        Admission::new(usize::MAX),
    )
    .await;
    slots_settle(&h, LOCALHOST, 0).await;
    let mut app = h.connect("app.example.com").await;
    app.write(KEEP_ALIVE_GET).await;
    assert_eq!(app.reply().await.status, 200);

    let mut held = Vec::new();
    for _ in 1..Admission::PER_ADDRESS {
        held.push(plain_accepted(&h).await.expect("under the cap"));
    }
    slots_settle(&h, LOCALHOST, Admission::PER_ADDRESS).await;
    assert!(
        plain_accepted(&h).await.is_none(),
        "the connection beyond the cap"
    );
    assert!(
        !tls_accepted_from(&h, LOCALHOST).await,
        "refused before TLS too"
    );
    assert_eq!(h.admission.refused(), 2);

    // Another address is not affected, nor is what is open.
    assert!(plain_accepted_from(&h, NEIGHBOUR).await.is_some());
    assert!(tls_accepted_from(&h, NEIGHBOUR).await);
    app.write(KEEP_ALIVE_GET).await;
    assert_eq!(app.reply().await.status, 200);

    held.pop();
    slots_settle(&h, LOCALHOST, Admission::PER_ADDRESS - 1).await;
    assert!(
        plain_accepted(&h).await.is_some(),
        "a freed slot is taken again"
    );
    assert_eq!(h.admission.refused(), 2);
}

/// A connection holds one slot however many requests it carries: HTTP/1.1 keep-alive on
/// both listeners, HTTP/2 with many streams at once.
#[tokio::test]
async fn a_connection_holds_one_slot_however_it_is_used() {
    let h = Harness::start().await;
    slots_settle(&h, LOCALHOST, 0).await;
    let mut tls = h.connect("app.example.com").await;
    let mut plain = h.connect_http().await;
    for _ in 0..3 {
        tls.write(KEEP_ALIVE_GET).await;
        assert_eq!(tls.reply().await.status, 200);
        plain.write(KEEP_ALIVE_GET).await;
        assert_eq!(plain.reply().await.status, 308);
        slots_settle(&h, LOCALHOST, 2).await;
    }

    let mut h2 = H2Client::connect(&h, "app.example.com").await;
    let streams: Vec<Frame> = (0..50)
        .map(|i| Frame::get(1 + 2 * i, "app.example.com", "/slow/300"))
        .collect();
    h2.send(&streams).await.unwrap();
    in_flight(&h.web, 50).await;
    slots_settle(&h, LOCALHOST, 3).await;
    let mut ended = 0;
    let received = h2
        .read_until(SLACK, |frame| {
            frame.ends_stream() && {
                ended += 1;
                ended == 50
            }
        })
        .await;
    let answered = received
        .of(Frame::HEADERS)
        .filter(|f| f.status() == Some(200));
    assert_eq!(answered.count(), 50, "{received:?}");
    slots_settle(&h, LOCALHOST, 3).await;

    drop((tls, plain, h2));
    slots_settle(&h, LOCALHOST, 0).await;
}

/// A slot is given back however the connection ends: the client leaves mid-header, or while
/// its response is pending, or its TLS handshake stalls or fails. A handshake holds a slot
/// while it lasts, since the slot is taken before it.
#[tokio::test]
async fn a_slot_is_freed_however_the_connection_ends() {
    let h = Harness::start().await;
    slots_settle(&h, LOCALHOST, 0).await;
    let mut tls = h.connect("app.example.com").await;
    tls.write(PARTIAL_HEAD).await;
    let mut plain = h.connect_http().await;
    plain.write(b"GET /x HTTP/1.1\r\nHost: app").await;
    let mut pending = h.connect("app.example.com").await;
    pending
        .write(b"GET /slow/2000 HTTP/1.1\r\nHost: app.example.com\r\n\r\n")
        .await;
    let mut h2 = H2Client::connect(&h, "app.example.com").await;
    h2.send(&[Frame::get(1, "app.example.com", "/slow/2000")])
        .await
        .unwrap();
    in_flight(&h.web, 2).await;
    slots_settle(&h, LOCALHOST, 4).await;
    drop((tls, plain, pending, h2));
    // The pending ones are noticed once their answer is written.
    slots_settle(&h, LOCALHOST, 0).await;

    // Half a ClientHello, then nothing.
    let mut stalled = TcpStream::connect(h.proxy).await.unwrap();
    stalled
        .write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01])
        .await
        .unwrap();
    // Not TLS at all.
    let mut garbage = TcpStream::connect(h.proxy).await.unwrap();
    garbage.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    // A client that does not trust the certificate aborts the handshake.
    let untrusting = TestCa::new().connector(&[b"http/1.1"]);
    let tcp = TcpStream::connect(h.proxy).await.unwrap();
    let name = ServerName::try_from("app.example.com").unwrap();
    assert!(untrusting.connect(name, tcp).await.is_err());
    slots_settle(&h, LOCALHOST, 1).await;
    drop(stalled);
    slots_settle(&h, LOCALHOST, 0).await;
    drop(garbage);
}

/// Stopping the proxy, gracefully or fast, gives back every slot, also of connections that
/// are idle, mid-request or mid-handshake then.
#[tokio::test]
async fn stopping_the_proxy_frees_every_slot() {
    for signal in [
        ShutdownSignal::GracefulTerminate,
        ShutdownSignal::FastShutdown,
    ] {
        let name = format!("{signal:?}");
        let mut h = Harness::start().await;
        slots_settle(&h, LOCALHOST, 0).await;
        let mut kept = h.connect("app.example.com").await;
        kept.write(KEEP_ALIVE_GET).await;
        assert_eq!(kept.reply().await.status, 200);
        let mut busy = H2Client::connect(&h, "app.example.com").await;
        busy.send(&[Frame::get(1, "app.example.com", "/slow/10000")])
            .await
            .unwrap();
        let idle = H2Client::connect(&h, "app.example.com").await;
        let mut plain = h.connect_http().await;
        plain.write(b"GET /x HTTP/1.1\r\n").await;
        let mut stalled = TcpStream::connect(h.proxy).await.unwrap();
        stalled.write_all(&[0x16, 0x03, 0x01]).await.unwrap();
        slots_settle(&h, LOCALHOST, 5).await;

        h.shutdown.send_replace(Some(signal));
        let thread = h.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .unwrap()
            .expect("the proxy thread exits");
        assert_eq!(h.admission.open_from_ip(LOCALHOST), 0, "{name}");
        assert_eq!(h.admission.clients(), 0, "{name}");
        drop((kept, busy, idle, plain, stalled));
    }
}

/// The filter checks the count and the slot is taken a moment later, in the connection's own
/// task, so a burst can get a few connections past the cap before they are counted (the spike
/// measured at most three). The total cap bounds whatever gets past.
#[tokio::test]
async fn a_burst_from_one_address_overshoots_its_cap_by_little() {
    const BURST: usize = 1000;
    let _ = FileLimit::raise();
    let h = Harness::start_admitting(
        Timeouts::default(),
        DownstreamLimits::default(),
        Admission::new(usize::MAX),
    )
    .await;
    slots_settle(&h, LOCALHOST, 0).await;
    // Raw connections, all completed in the kernel's backlog before the proxy accepts any.
    let mut held = Vec::with_capacity(BURST);
    for i in 0..BURST {
        let addr = if i % 2 == 0 { h.http } else { h.proxy };
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    let deadline = Instant::now() + SLACK;
    while h.admission.refused() + (h.admission.open_from_ip(LOCALHOST) as u64) < BURST as u64 {
        assert!(Instant::now() < deadline, "the burst was not all accepted");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let open = h.admission.open_from_ip(LOCALHOST);
    assert!(
        (Admission::PER_ADDRESS..=Admission::PER_ADDRESS + 16).contains(&open),
        "{open} connections counted"
    );
    drop(held);
    slots_settle(&h, LOCALHOST, 0).await;
}

// ---------------------------------------------------------------------------------------------
// Size limits

/// The first line of the answer to `head` on `:443` or `:80`; the request may be refused
/// before it was written in full.
async fn status_of(h: &Harness, tls: bool, head: &str) -> u16 {
    let reply = async {
        if tls {
            let mut client = h.connect("app.example.com").await;
            let stream = client.0.get_mut();
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.flush().await;
            client.head().await.0
        } else {
            let mut client = h.connect_http().await;
            let _ = client.0.get_mut().write_all(head.as_bytes()).await;
            client.head().await.0
        }
    };
    tokio::time::timeout(SLACK, reply).await.expect("an answer")
}

#[tokio::test]
async fn oversized_request_heads_are_refused_on_both_listeners() {
    let h = Harness::start().await;
    let get = |path: &str, headers: &str| {
        format!(
            "GET /{path} HTTP/1.1\r\nHost: app.example.com\r\nConnection: close\r\n{headers}\r\n"
        )
    };
    let field = |len: usize| format!("X-Big: {}\r\n", "a".repeat(len));
    // (what, request, refused)
    let cases = [
        // A request target is at most 65 535 bytes (the `http` crate's URI limit).
        ("60 KiB target", get(&"a".repeat(60 * 1024), ""), false),
        ("70 KiB target", get(&"a".repeat(70 * 1024), ""), true),
        // The whole head is at most 1 MiB.
        ("60 KiB header", get("x", &field(60 * 1024)), false),
        ("1 MiB header", get("x", &field(1024 * 1024)), true),
        // At most 256 header fields; the request has `Host` and `Connection` too.
        ("256 headers", get("x", &"X-A: b\r\n".repeat(254)), false),
        ("257 headers", get("x", &"X-A: b\r\n".repeat(255)), true),
    ];
    for (what, head, refused) in &cases {
        for tls in [true, false] {
            let status = status_of(&h, tls, head).await;
            // What is let through is answered by hoster on :80 and by the upstream on :443,
            // whose own limits are tighter (431) for the larger ones.
            let expected: &[u16] = match (refused, tls) {
                (true, _) => &[400],
                (false, false) => &[308],
                (false, true) => &[200, 431],
            };
            assert!(expected.contains(&status), "{what}, tls {tls}: {status}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP/2

#[tokio::test]
async fn h2_streams_beyond_one_hundred_at_once_are_refused() {
    let h = Harness::start().await;
    let mut client = H2Client::connect(&h, "app.example.com").await;
    let streams: Vec<Frame> = (0..150)
        .map(|i| Frame::get(1 + 2 * i, "app.example.com", "/slow/1000"))
        .collect();
    client.send(&streams).await.unwrap();
    let mut ended = 0;
    let received = client
        .read_until(SLACK * 2, |frame| {
            frame.ends_stream() && {
                ended += 1;
                ended == 150
            }
        })
        .await;
    let refused: Vec<_> = received.of(Frame::RST_STREAM).collect();
    assert_eq!(refused.len(), 50, "{received:?}");
    assert!(
        refused
            .iter()
            .all(|frame| frame.error_code() == Some(Frame::REFUSED_STREAM)),
        "{refused:?}"
    );
    let answered = received
        .of(Frame::HEADERS)
        .filter(|frame| frame.status() == Some(200))
        .count();
    assert_eq!(answered, 100, "{received:?}");
    assert_eq!(received.goaway(), None);
}

/// The rapid reset pattern of CVE-2023-44487: h2 ends the connection once too many streams
/// were reset before the proxy took them up, and nothing stays in flight upstream.
#[tokio::test]
async fn h2_rapid_resets_end_the_connection() {
    let h = Harness::start().await;
    let mut client = H2Client::connect(&h, "app.example.com").await;
    let mut sent = 0;
    'flood: for batch in 0..100 {
        let frames: Vec<Frame> = (0..100)
            .flat_map(|i| {
                let stream = 1 + 2 * (batch * 100 + i);
                [
                    Frame::get(stream, "app.example.com", "/slow/1000"),
                    Frame::rst_stream(stream, Frame::CANCEL),
                ]
            })
            .collect();
        if client.send(&frames).await.is_err() {
            break 'flood;
        }
        sent += 100;
    }
    let received = client.read_to_end(SLACK).await;
    assert!(received.closed, "after {sent} resets: {received:?}");
    assert_eq!(
        received.goaway(),
        Some((Frame::ENHANCE_YOUR_CALM, "too_many_resets".to_owned())),
        "after {sent} resets"
    );
    assert!(sent < 10_000, "the whole flood was accepted");
    in_flight(&h.web, 0).await;
}

#[tokio::test]
async fn h2_continuation_floods_end_the_connection() {
    let h = Harness::start().await;
    let mut client = H2Client::connect(&h, "app.example.com").await;
    let mut head = Frame::get(1, "app.example.com", "/x");
    head.flags = Frame::END_STREAM;
    client.send(&[head]).await.unwrap();
    let filler = Frame::block(&[("x-fill", &"a".repeat(16_000))]);
    let mut sent = 0;
    while sent < 64 * 1024 * 1024 {
        let frame = Frame::new(Frame::CONTINUATION, 0, 1, filler.clone());
        if client.send(&[frame]).await.is_err() {
            break;
        }
        sent += filler.len();
    }
    let received = client.read_to_end(SLACK).await;
    assert!(received.closed, "after {sent} bytes: {received:?}");
    assert_eq!(
        received.goaway(),
        Some((
            Frame::ENHANCE_YOUR_CALM,
            "too_many_continuations".to_owned()
        )),
        "after {sent} bytes"
    );
    assert!(sent < 1024 * 1024, "{sent} bytes of header block were read");
}

#[tokio::test]
async fn h2_header_lists_over_64_kib_are_refused() {
    let h = Harness::start().await;
    for (size, expected) in [(48 * 1024, 200), (70 * 1024, 431)] {
        let mut client = H2Client::connect(&h, "app.example.com").await;
        let mut block = Frame::get(1, "app.example.com", "/x").payload;
        let filler = "a".repeat(4096);
        for _ in 0..size / filler.len() {
            block.extend(Frame::block(&[("x-fill", &filler)]));
        }
        let mut chunks = block.chunks(16_000).peekable();
        let mut frames = vec![Frame::new(
            Frame::HEADERS,
            Frame::END_STREAM,
            1,
            chunks.next().unwrap(),
        )];
        frames.extend(chunks.map(|chunk| Frame::new(Frame::CONTINUATION, 0, 1, chunk)));
        frames.last_mut().unwrap().flags |= Frame::END_HEADERS;
        client.send(&frames).await.unwrap();
        let received = client.read_until(SLACK, Frame::ends_stream).await;
        assert_eq!(received.status(1), Some(expected), "{size}: {received:?}");
        assert_eq!(received.goaway(), None, "{size}");
    }
}

/// SETTINGS, PING and PRIORITY frames cost the proxy nothing it keeps: each SETTINGS and
/// PING is answered, and when the client does not read the answers the proxy stops reading
/// too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_control_frame_floods_do_not_grow_memory() {
    const FRAMES: usize = 300_000;
    let h = Harness::start().await;
    let floods = [
        Frame::new(Frame::SETTINGS, 0, 0, []),
        Frame::new(Frame::PING, 0, 0, [0; 8]),
        Frame::new(Frame::PRIORITY, 0, 1, [0; 5]),
    ];
    let watch = RssWatch::start();
    for frame in &floods {
        let mut client = H2Client::connect(&h, "app.example.com").await;
        let batch = frame.bytes().repeat(1000);
        for _ in 0..FRAMES / 1000 {
            // Backpressure: the proxy stopped reading because its answers are not read.
            match tokio::time::timeout(Duration::from_secs(1), client.0.write_all(&batch)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }
    let growth = watch.growth();
    assert!(
        growth < 64 * 1024 * 1024,
        "memory grew by {growth} bytes under control frame floods"
    );
    assert_eq!(h.get("app.example.com", "/after").await.status, 200);
}
