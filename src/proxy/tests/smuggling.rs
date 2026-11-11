//! Request smuggling and framing at the edge: hostile bytes in, and a look at both what the
//! client gets and what the upstream receives (the [`Recorder`] behind `raw.example.com`).
//!
//! hoster takes h1 and h2 from clients, speaks h1 to upstreams (ADR-0033) and reuses upstream
//! connections, which is the setup smuggling needs: if the proxy and the upstream disagree
//! about where a request ends, the tail of one client's request becomes the start of the next
//! client's. So for every case the proxy must either refuse the request or forward **one**
//! request an upstream cannot frame differently, and the upstream connection must be clean
//! afterwards. Every exchange checks that (see [`Outcome::check`]) on top of its own expected
//! outcome.
//!
//! The Pingora advisories in this area, all fixed before the 0.9.1 this is pinned to (RustSec
//! advisory database, checked 2026-11-11):
//!
//! | Advisory | What | Fixed in |
//! |---|---|---|
//! | RUSTSEC-2025-0037, CVE-2025-4366 | Request smuggling: body not drained on a cache hit (`pingora-cache`, not used here) | 0.5.0 |
//! | RUSTSEC-2025-0070, CVE-2025-8671 | HTTP/2 "MadeYouReset" denial of service | 0.6.0 |
//! | RUSTSEC-2026-0033, CVE-2026-2833 | Request smuggling: bytes after an `Upgrade` request forwarded before the upstream's 101 | 0.8.0 |
//! | RUSTSEC-2026-0034, CVE-2026-2835 | Request smuggling: close-delimited HTTP/1.0 bodies, several `Transfer-Encoding` values | 0.8.0 |
//! | RUSTSEC-2026-0035, CVE-2026-2836 | Cache poisoning by the default cache key (`pingora-cache`, not used here) | 0.8.0 |
//! | RUSTSEC-2026-0389, CVE-2026-105297 | HTTP/2 memory exhaustion: no default header-list or stream limits | 0.8.1 |
//! | RUSTSEC-2026-0388, CVE-2026-105298 | HTTP/2 `:authority` that disagrees with `Host` | 0.9.0 |
//! | RUSTSEC-2026-0390, CVE-2026-105299 | Headers named by the client's `Connection` forwarded to HTTP/1 upstreams | 0.9.0 |
//!
//! The cases for 0033, 0034, 0388 and 0390 are below; 0070 and 0389 are in `limits.rs`.

use super::*;

/// How long a connection must stay quiet before an exchange counts as over.
const QUIET: Duration = Duration::from_millis(400);

/// A request no case sends on its own. It rides behind the request under test, where a
/// parser that frames that request wrongly would find it.
const SMUGGLED: &str = "GET /smuggled HTTP/1.1\r\nHost: raw.example.com\r\n\r\n";

/// What the proxy must do with one hostile request.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Answer it itself with this status. The upstream gets nothing.
    Refused(u16),
    /// Forward exactly this request line and body, and pass the upstream's 200 on.
    Forwarded(&'static str, &'static str),
    /// Forward the head, find the body malformed, and answer with this status. The upstream
    /// gets no whole request, and its connection is closed.
    Aborted(u16),
}

/// What one exchange did.
#[derive(Debug)]
struct Outcome {
    /// The status of every response the client got, in order.
    statuses: Vec<u16>,
    /// The whole requests the upstream received meanwhile.
    upstream: Vec<Seen>,
    /// How many upstream connections had been cut short before the exchange.
    broken_before: usize,
}

impl Outcome {
    /// The request lines the upstream received.
    fn lines(&self) -> Vec<String> {
        self.upstream.iter().map(Seen::line).collect()
    }

    /// Checks what must hold whatever the request was, then `expect`:
    ///
    /// - the upstream received at most one request, and never the smuggled one;
    /// - what it received is framed one way only ([`Seen::ambiguities`]).
    ///
    /// That the upstream connection is clean afterwards was checked when the exchange ended
    /// ([`Harness::assert_upstream_is_clean`]).
    async fn check(&self, h: &Harness, what: &str, expect: Expect) {
        let lines = self.lines();
        assert!(lines.len() <= 1, "{what}: more than one request: {self:?}");
        assert!(
            !lines.iter().any(|line| line.contains("/smuggled")),
            "{what}: the smuggled request reached the upstream: {self:?}"
        );
        for seen in &self.upstream {
            assert_eq!(seen.ambiguities(), [""; 0], "{what}: {seen:?}");
        }
        match expect {
            Expect::Refused(status) => {
                assert_eq!(self.statuses, [status], "{what}: {self:?}");
                assert!(self.upstream.is_empty(), "{what}: {self:?}");
            }
            Expect::Forwarded(line, body) => {
                assert_eq!(self.statuses, [200], "{what}: {self:?}");
                assert_eq!(lines, [line], "{what}");
                assert_eq!(
                    String::from_utf8_lossy(&self.upstream[0].body),
                    body,
                    "{what}"
                );
            }
            Expect::Aborted(status) => {
                assert_eq!(self.statuses, [status], "{what}: {self:?}");
                assert!(self.upstream.is_empty(), "{what}: {self:?}");
                h.recorder.broken_after(self.broken_before).await;
            }
        }
    }
}

impl Harness {
    /// Two plain requests through fresh client connections, which the proxy sends over its
    /// pooled upstream connections: each must get its own answer. On a connection with
    /// smuggled bytes left on it, the answer would be to those bytes instead.
    async fn assert_upstream_is_clean(&self, what: &str) {
        for i in 0..2 {
            let reply = self.get("raw.example.com", &format!("/after/{i}")).await;
            assert_eq!(reply.status, 200, "{what}");
            assert_eq!(
                reply.text(),
                format!("GET /after/{i} HTTP/1.1\n"),
                "{what}: an upstream connection was left poisoned"
            );
        }
        // Nothing else arrived with them. (A request the proxy reset on HTTP/2 may still
        // have reached the upstream a moment late; that is its own request line.)
        let lines: Vec<String> = self.recorder.take().iter().map(Seen::line).collect();
        let after = lines.iter().filter(|line| line.starts_with("GET /after/"));
        assert_eq!(after.count(), 2, "{what}: {lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("/smuggled")),
            "{what}: {lines:?}"
        );
    }

    /// Writes `bytes` on one HTTP/1.1 connection for the SNI name `sni` and reads responses
    /// until the proxy closes it or goes quiet.
    async fn raw_exchange(&self, what: &str, sni: &str, bytes: &[u8]) -> Outcome {
        self.recorder.take();
        let broken_before = self.recorder.broken();
        let mut client = self.connect(sni).await;
        // The proxy may answer and close before it has read everything.
        let stream = client.0.get_mut();
        let _ = stream.write_all(bytes).await;
        let _ = stream.flush().await;
        let mut received = Vec::new();
        let mut buf = [0; 4096];
        loop {
            match tokio::time::timeout(QUIET, client.0.read(&mut buf)).await {
                Err(_) | Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            }
        }
        let upstream = self.recorder.take();
        self.assert_upstream_is_clean(what).await;
        Outcome {
            statuses: statuses(&received),
            upstream,
            broken_before,
        }
    }

    /// One HTTP/1.1 exchange with `raw.example.com`, checked against `expect`.
    async fn attack(&self, what: &str, bytes: impl AsRef<[u8]>, expect: Expect) {
        self.raw_exchange(what, "raw.example.com", bytes.as_ref())
            .await
            .check(self, what, expect)
            .await;
    }

    /// Sends `frames` on one HTTP/2 connection to `raw.example.com` and reads until stream 1
    /// ends or the connection does.
    async fn h2_exchange(&self, what: &str, frames: &[Frame]) -> (Received, Outcome) {
        self.recorder.take();
        let broken_before = self.recorder.broken();
        let mut client = H2Client::connect(self, "raw.example.com").await;
        let _ = client.send(frames).await;
        let received = client
            .read_until(Duration::from_secs(5), |frame| {
                frame.ends_stream() || frame.is(Frame::GOAWAY)
            })
            .await;
        // What the proxy still had to tell the upstream (the end of a body, or nothing).
        tokio::time::sleep(QUIET / 2).await;
        let upstream = self.recorder.take();
        self.assert_upstream_is_clean(what).await;
        let outcome = Outcome {
            statuses: received.status(1).into_iter().collect(),
            upstream,
            broken_before,
        };
        (received, outcome)
    }
}

/// The status of each response in `bytes`. Every response on these connections has a
/// `Content-Length`.
fn statuses(bytes: &[u8]) -> Vec<u16> {
    let mut statuses = Vec::new();
    let mut rest = bytes;
    while let Some(end) = rest.windows(4).position(|w| w == b"\r\n\r\n") {
        let head = String::from_utf8_lossy(&rest[..end]).into_owned();
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok());
        statuses.push(status.unwrap_or_else(|| panic!("not a response: {head:?}")));
        let len: usize = head
            .split("\r\n")
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        rest = &rest[(end + 4 + len).min(rest.len())..];
    }
    statuses
}

/// A `POST /a` with `headers` after `Host`, and `body` (and whatever follows it).
fn post(headers: &str, body: &str) -> String {
    format!("POST /a HTTP/1.1\r\nHost: raw.example.com\r\n{headers}\r\n{body}")
}

/// A request with `request_line` and `headers`, no `Host` of its own.
fn request(request_line: &str, headers: &str) -> String {
    format!("{request_line}\r\n{headers}\r\n")
}

const HOST: &str = "Host: raw.example.com\r\n";
const POSTED: Expect = Expect::Forwarded("POST /a HTTP/1.1", "hello");

// ---------------------------------------------------------------------------------------------
// HTTP/1 framing

#[tokio::test]
async fn a_message_has_one_length() {
    let h = Harness::start().await;
    let hello = format!("hello{SMUGGLED}");
    let empty_chunked = format!("0\r\n\r\n{SMUGGLED}");
    let cases = [
        (
            "Content-Length",
            post("Content-Length: 5\r\n", &hello),
            POSTED,
        ),
        // Both: chunked wins, and only it goes upstream (RFC 9112 §6.3 rule 3).
        (
            "Content-Length, then Transfer-Encoding",
            post(
                "Content-Length: 4\r\nTransfer-Encoding: chunked\r\n",
                &empty_chunked,
            ),
            Expect::Forwarded("POST /a HTTP/1.1", ""),
        ),
        (
            "Transfer-Encoding, then Content-Length",
            post(
                "Transfer-Encoding: chunked\r\nContent-Length: 6\r\n",
                &empty_chunked,
            ),
            Expect::Forwarded("POST /a HTTP/1.1", ""),
        ),
        // The same length twice is one length, and the upstream is told it once.
        (
            "the same Content-Length twice",
            post("Content-Length: 5\r\nContent-Length: 5\r\n", &hello),
            POSTED,
        ),
        (
            "a list of the same length",
            post("Content-Length: 5, 5\r\n", &hello),
            POSTED,
        ),
        (
            "a leading zero",
            post("Content-Length: 05\r\n", &hello),
            POSTED,
        ),
        (
            "whitespace after the length",
            post("Content-Length: 5\t\r\n", &hello),
            POSTED,
        ),
        (
            "two different Content-Lengths",
            post("Content-Length: 5\r\nContent-Length: 6\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "a list of different lengths",
            post("Content-Length: 5,6\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "a signed length",
            post("Content-Length: +5\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "a negative length",
            post("Content-Length: -5\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "a hexadecimal length",
            post("Content-Length: 0x5\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "two numbers",
            post("Content-Length: 5 5\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "an empty length",
            post("Content-Length: \r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "a length beyond 64 bits",
            post("Content-Length: 99999999999999999999999\r\n", &hello),
            Expect::Refused(400),
        ),
        (
            "a space before the colon",
            post("Content-Length : 5\r\n", &hello),
            Expect::Refused(400),
        ),
        // A body on a GET is a body.
        (
            "a GET with a body",
            format!("GET /a HTTP/1.1\r\n{HOST}Content-Length: 5\r\n\r\n{hello}"),
            Expect::Forwarded("GET /a HTTP/1.1", "hello"),
        ),
    ];
    for (what, bytes, expect) in cases {
        h.attack(what, bytes, expect).await;
    }
}

/// Only `chunked` as the final coding frames a body. Anything else Pingora either refuses or
/// reads as chunked and re-encodes, so the upstream always gets a plain
/// `Transfer-Encoding: chunked`.
#[tokio::test]
async fn transfer_encoding_is_chunked_or_refused() {
    let h = Harness::start().await;
    let body = format!("5\r\nhello\r\n0\r\n\r\n{SMUGGLED}");
    let cases = [
        ("Transfer-Encoding: chunked\r\n", POSTED),
        ("tRaNsFeR-eNcOdInG: ChUnKeD\r\n", POSTED),
        ("Transfer-Encoding:\t\tchunked\r\n", POSTED),
        ("Transfer-Encoding: chunked \r\n", POSTED),
        ("Transfer-Encoding: gzip, chunked\r\n", POSTED),
        (
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
            POSTED,
        ),
        (
            "Transfer-Encoding: identity\r\nTransfer-Encoding: chunked\r\n",
            POSTED,
        ),
        ("Transfer-Encoding: xchunked\r\n", Expect::Refused(400)),
        ("Transfer-Encoding: identity\r\n", Expect::Refused(400)),
        (
            "Transfer-Encoding: chunked, identity\r\n",
            Expect::Refused(400),
        ),
        (
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\n",
            Expect::Refused(400),
        ),
        ("Transfer-Encoding: \"chunked\"\r\n", Expect::Refused(400)),
        ("Transfer-Encoding:\x0bchunked\r\n", Expect::Refused(400)),
        ("Transfer-Encoding\t: chunked\r\n", Expect::Refused(400)),
        ("Transfer-Encoding : chunked\r\n", Expect::Refused(400)),
    ];
    for (headers, expect) in cases {
        h.attack(headers.trim_end(), post(headers, &body), expect)
            .await;
    }
}

/// Chunks are decoded by the proxy and encoded again for the upstream, so a chunk the proxy
/// accepts loosely (an extension, whitespace after the size) reaches the upstream in plain
/// form, and one it cannot read ends the request before the upstream got a whole one.
#[tokio::test]
async fn malformed_chunks_never_reach_the_upstream() {
    let h = Harness::start().await;
    let cases = [
        ("an extension", "5;ext=1\r\nhello\r\n0\r\n\r\n", POSTED),
        (
            "a line feed in an extension",
            "5;a\nb\r\nhello\r\n0\r\n\r\n",
            POSTED,
        ),
        ("a space after the size", "5 \r\nhello\r\n0\r\n\r\n", POSTED),
        (
            "trailer fields",
            "5\r\nhello\r\n0\r\nX-T: 1\r\n\r\n",
            POSTED,
        ),
        (
            "a 0x size",
            "0x5\r\nhello\r\n0\r\n\r\n",
            Expect::Aborted(400),
        ),
        (
            "a signed size",
            "+5\r\nhello\r\n0\r\n\r\n",
            Expect::Aborted(400),
        ),
        (
            "a negative size",
            "-1\r\nhello\r\n0\r\n\r\n",
            Expect::Aborted(400),
        ),
        (
            "a size beyond 64 bits",
            "FFFFFFFFFFFFFFFFF\r\nhello\r\n0\r\n\r\n",
            Expect::Aborted(400),
        ),
        // 2^64 + 5: a parser that wraps would read five bytes.
        (
            "a size that wraps to 5",
            "10000000000000005\r\nhello\r\n0\r\n\r\n",
            Expect::Aborted(400),
        ),
        ("bare line feeds", "5\nhello\n0\n\n", Expect::Aborted(400)),
        (
            "no CRLF after the data",
            "5\r\nhelloXX0\r\n\r\n",
            Expect::Aborted(400),
        ),
    ];
    for (what, chunks, expect) in cases {
        let body = format!("{chunks}{SMUGGLED}");
        h.attack(what, post("Transfer-Encoding: chunked\r\n", &body), expect)
            .await;
    }
}

/// Line endings and header syntax. Pingora's parser takes a bare LF for a line end, so what
/// follows one is a header of its own for the proxy too, and goes upstream with CRLF.
#[tokio::test]
async fn request_heads_are_refused_or_rewritten_cleanly() {
    let h = Harness::start().await;
    let chunked = format!("5\r\nhello\r\n0\r\n\r\n{SMUGGLED}");
    let got = Expect::Forwarded("GET /a HTTP/1.1", "");
    let cases: Vec<(&str, Vec<u8>, Expect)> = vec![
        (
            "bare LF line ends",
            format!("GET /a HTTP/1.1\nHost: raw.example.com\n\n{SMUGGLED}").into(),
            got,
        ),
        (
            "a bare LF inside a header value",
            format!("GET /a HTTP/1.1\r\n{HOST}X-A: b\nTransfer-Encoding: chunked\r\n\r\n{chunked}")
                .into(),
            Expect::Forwarded("GET /a HTTP/1.1", "hello"),
        ),
        (
            "an empty line before the request",
            format!("\r\nGET /a HTTP/1.1\r\n{HOST}\r\n").into(),
            got,
        ),
        (
            "bare CR line ends",
            format!("GET /a HTTP/1.1\rHost: raw.example.com\r\r{SMUGGLED}").into(),
            Expect::Refused(400),
        ),
        (
            "a bare CR inside a header value",
            format!("GET /a HTTP/1.1\r\n{HOST}X-A: b\rTransfer-Encoding: chunked\r\n\r\n{chunked}")
                .into(),
            Expect::Refused(400),
        ),
        (
            "a folded header",
            format!("GET /a HTTP/1.1\r\n{HOST}X-A: b\r\n c\r\n\r\n{SMUGGLED}").into(),
            Expect::Refused(400),
        ),
        (
            "Transfer-Encoding in a folded line",
            format!(
                "POST /a HTTP/1.1\r\n{HOST}X-A: b\r\n Transfer-Encoding: chunked\r\n\r\n{chunked}"
            )
            .into(),
            Expect::Refused(400),
        ),
        (
            "whitespace before the first header",
            format!("GET /a HTTP/1.1\r\n {HOST}{HOST}\r\n{SMUGGLED}").into(),
            Expect::Refused(400),
        ),
        (
            "a NUL in a header value",
            format!("GET /a HTTP/1.1\r\n{HOST}X-A: b\0c\r\n\r\n{SMUGGLED}").into(),
            Expect::Refused(400),
        ),
        (
            "a space in a header name",
            format!("GET /a HTTP/1.1\r\n{HOST}X A: b\r\n\r\n{SMUGGLED}").into(),
            Expect::Refused(400),
        ),
        (
            "a colon in the method",
            format!("G:T /a HTTP/1.1\r\n{HOST}\r\n").into(),
            Expect::Refused(400),
        ),
        (
            "text after the version",
            format!("GET /a HTTP/1.1 X\r\n{HOST}\r\n").into(),
            Expect::Refused(400),
        ),
        ("no version", b"GET /a\r\n\r\n".into(), Expect::Refused(400)),
        (
            "HTTP/2.0 in the request line",
            format!("GET /a HTTP/2.0\r\n{HOST}\r\n").into(),
            Expect::Refused(400),
        ),
        (
            "the HTTP/2 preface",
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".into(),
            Expect::Refused(400),
        ),
        (
            "bytes that are not UTF-8 in the target",
            [b"GET /a\xff\xfe HTTP/1.1\r\n", HOST.as_bytes(), b"\r\n"].concat(),
            Expect::Refused(400),
        ),
        // Opaque to the proxy, and no framing depends on it.
        (
            "bytes that are not UTF-8 in a header value",
            [
                b"GET /a HTTP/1.1\r\n",
                HOST.as_bytes(),
                b"X-A: \xff\xfe\r\n\r\n",
            ]
            .concat(),
            got,
        ),
        // Pingora's request line parser is loose about spaces. What it makes of them is
        // what it routes and forwards, as one well-formed request line.
        (
            "a space in the target",
            format!("GET /a b HTTP/1.1\r\n{HOST}\r\n").into(),
            Expect::Forwarded("GET /a%20b HTTP/1.1", ""),
        ),
        (
            "a tab in the target",
            format!("GET /a\tb HTTP/1.1\r\n{HOST}\r\n").into(),
            Expect::Forwarded("GET /a%09b HTTP/1.1", ""),
        ),
        (
            "two spaces before the version",
            format!("GET /a  HTTP/1.1\r\n{HOST}\r\n").into(),
            Expect::Forwarded("GET /a%20 HTTP/1.1", ""),
        ),
        (
            "two spaces before the target",
            format!("GET  /a HTTP/1.1\r\n{HOST}\r\n").into(),
            Expect::Forwarded("GET / HTTP/1.1", ""),
        ),
        (
            "a lowercase method",
            format!("get /a HTTP/1.1\r\n{HOST}\r\n").into(),
            Expect::Forwarded("get /a HTTP/1.1", ""),
        ),
    ];
    for (what, bytes, expect) in cases {
        h.attack(what, bytes, expect).await;
    }
}

/// HTTP/1.0 (RUSTSEC-2026-0034): a body needs a `Content-Length`. One without is not read to
/// the end of the connection, and chunked does not exist in 1.0. The upstream always gets
/// HTTP/1.1.
#[tokio::test]
async fn http_1_0_bodies_need_a_content_length() {
    let h = Harness::start().await;
    let hello = format!("hello{SMUGGLED}");
    let cases = [
        (
            "Content-Length",
            format!("POST /a HTTP/1.0\r\n{HOST}Content-Length: 5\r\n\r\n{hello}"),
            POSTED,
        ),
        (
            "Content-Length and keep-alive",
            format!(
                "POST /a HTTP/1.0\r\n{HOST}Connection: keep-alive\r\nContent-Length: 5\r\n\r\n{hello}"
            ),
            POSTED,
        ),
        (
            "no length",
            format!("POST /a HTTP/1.0\r\n{HOST}\r\n{hello}"),
            Expect::Forwarded("POST /a HTTP/1.1", ""),
        ),
        (
            "chunked",
            format!(
                "POST /a HTTP/1.0\r\n{HOST}Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n{SMUGGLED}"
            ),
            Expect::Refused(400),
        ),
    ];
    for (what, bytes, expect) in cases {
        h.attack(what, bytes, expect).await;
    }
}

/// An `Upgrade` the upstream does not accept (the recorder answers 200, not 101) leaves an
/// ordinary request (RUSTSEC-2026-0033): the bytes behind it are not tunnelled, and a body is
/// framed as usual.
#[tokio::test]
async fn a_refused_upgrade_is_not_a_tunnel() {
    let h = Harness::start().await;
    let upgrade = "Upgrade: websocket\r\nConnection: Upgrade\r\n";
    let got = Expect::Forwarded("GET /a HTTP/1.1", "");
    let cases = [
        (
            "bytes behind the request",
            format!("GET /a HTTP/1.1\r\n{HOST}{upgrade}\r\n{SMUGGLED}{SMUGGLED}"),
            got,
        ),
        (
            "a body with a length",
            format!("POST /a HTTP/1.1\r\n{HOST}{upgrade}Content-Length: 5\r\n\r\nhello{SMUGGLED}"),
            POSTED,
        ),
        (
            "a chunked body",
            format!(
                "POST /a HTTP/1.1\r\n{HOST}{upgrade}Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n{SMUGGLED}"
            ),
            POSTED,
        ),
        // h2c is not offered (ADR-0033); the upgrade headers are dropped.
        (
            "an h2c upgrade",
            format!(
                "GET /a HTTP/1.1\r\n{HOST}Upgrade: h2c\r\nConnection: Upgrade, HTTP2-Settings\r\n\
                 HTTP2-Settings: AAMAAABkAAQAoAAAAAIAAAAA\r\n\r\n"
            ),
            got,
        ),
    ];
    for (what, bytes, expect) in cases {
        h.attack(what, bytes, expect).await;
    }
}

/// Pingora does not pipeline: what a client sends behind a request, before that request's
/// response, is dropped and the connection closed after the response. So in every case
/// above the smuggled request ends up nowhere, and here a perfectly good second request does
/// too.
#[tokio::test]
async fn pipelined_requests_are_dropped() {
    let h = Harness::start().await;
    let two = format!("GET /a HTTP/1.1\r\n{HOST}\r\nGET /b HTTP/1.1\r\n{HOST}\r\n");
    h.attack(
        "two requests at once",
        two,
        Expect::Forwarded("GET /a HTTP/1.1", ""),
    )
    .await;
    let expecting = format!(
        "GET /a HTTP/1.1\r\n{HOST}Expect: 100-continue\r\nContent-Length: 5\r\n\r\nhello{SMUGGLED}"
    );
    h.attack(
        "Expect: 100-continue",
        expecting,
        Expect::Forwarded("GET /a HTTP/1.1", "hello"),
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// Which host a request is for

/// A request names its host once, and that name is the connection's SNI name (ADR-0032).
#[tokio::test]
async fn a_request_names_one_host() {
    let h = Harness::start().await;
    let got = Expect::Forwarded("GET /a HTTP/1.1", "");
    let cases = [
        // An absolute-form target goes upstream in origin form.
        ("GET http://raw.example.com/a HTTP/1.1", HOST, got),
        ("GET /a HTTP/1.1", "Host: raw.example.com:1234\r\n", got),
        ("GET /a HTTP/1.1", "Host: RAW.example.com.\r\n", got),
        // The target's authority and `Host` disagree, either way round.
        (
            "GET http://raw.example.com/a HTTP/1.1",
            "Host: app.example.com\r\n",
            Expect::Refused(400),
        ),
        (
            "GET http://app.example.com/a HTTP/1.1",
            HOST,
            Expect::Refused(400),
        ),
        (
            "GET http://app.example.com@raw.example.com/a HTTP/1.1",
            HOST,
            Expect::Refused(400),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: raw.example.com\r\nHost: raw.example.com\r\n",
            Expect::Refused(400),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: raw.example.com\r\nHost: app.example.com\r\n",
            Expect::Refused(400),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: app.example.com\r\nHost: raw.example.com\r\n",
            Expect::Refused(400),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: app.example.com@raw.example.com\r\n",
            Expect::Refused(400),
        ),
        // Not the SNI name.
        (
            "GET /a HTTP/1.1",
            "Host: app.example.com\r\n",
            Expect::Refused(421),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: raw.example.com, app.example.com\r\n",
            Expect::Refused(421),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: raw.example.com app.example.com\r\n",
            Expect::Refused(421),
        ),
        (
            "GET /a HTTP/1.1",
            "Host: raw.example.com/x\r\n",
            Expect::Refused(421),
        ),
        // No host at all matches no route.
        ("GET /a HTTP/1.1", "", Expect::Refused(404)),
        ("GET /a HTTP/1.1", "Host: \r\n", Expect::Refused(404)),
        (
            "CONNECT raw.example.com:443 HTTP/1.1",
            HOST,
            Expect::Refused(405),
        ),
    ];
    for (line, headers, expect) in cases {
        let what = format!("{line} / {}", headers.trim_end());
        h.attack(&what, request(line, headers), expect).await;
    }
    // The other way round: the right `Host` on a connection for another name.
    h.raw_exchange(
        "another SNI name",
        "app.example.com",
        request("GET /a HTTP/1.1", HOST).as_bytes(),
    )
    .await
    .check(&h, "another SNI name", Expect::Refused(421))
    .await;
}

/// The client's `Connection` header cannot take headers away from the upstream that hoster
/// or the app rely on (RUSTSEC-2026-0390), and hop-by-hop headers stop at the proxy.
#[tokio::test]
async fn connection_cannot_strip_what_the_upstream_relies_on() {
    let h = Harness::start().await;
    let head = |headers: &str| format!("GET /a HTTP/1.1\r\n{HOST}{headers}\r\n");
    for nominated in ["X-Forwarded-For", "Host", "close, X-Forwarded-Host"] {
        let what = format!("Connection: {nominated}");
        h.attack(&what, head(&format!("{what}\r\n")), Expect::Refused(400))
            .await;
    }

    // Other headers the client names are its to drop.
    let dropped = head("Connection: keep-alive, Cookie, X-Foo\r\nCookie: a=b\r\nX-Foo: 1\r\n");
    let outcome = h
        .raw_exchange("named headers", "raw.example.com", dropped.as_bytes())
        .await;
    outcome
        .check(
            &h,
            "named headers",
            Expect::Forwarded("GET /a HTTP/1.1", ""),
        )
        .await;
    let hop_by_hop = head(
        "Keep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nTE: trailers\r\nTrailer: X\r\n",
    );
    let passed = h
        .raw_exchange("hop-by-hop", "raw.example.com", hop_by_hop.as_bytes())
        .await;
    passed
        .check(&h, "hop-by-hop", Expect::Forwarded("GET /a HTTP/1.1", ""))
        .await;
    for seen in outcome.upstream.iter().chain(&passed.upstream) {
        for name in [
            "connection",
            "cookie",
            "x-foo",
            "keep-alive",
            "proxy-connection",
            "te",
        ] {
            assert_eq!(seen.header(name), [""; 0], "{name}: {seen:?}");
        }
        assert_eq!(seen.header("host"), ["raw.example.com"]);
        assert_eq!(seen.header("x-forwarded-for"), ["127.0.0.1"]);
    }

    // Naming Content-Length drops it; the body then goes upstream chunked, still whole.
    let body = format!(
        "POST /a HTTP/1.1\r\n{HOST}Connection: Content-Length\r\nContent-Length: 5\r\n\r\nhello"
    );
    h.attack("Connection: Content-Length", body, POSTED).await;
}

// ---------------------------------------------------------------------------------------------
// HTTP/2, forwarded as HTTP/1.1

/// What the proxy must do with one hostile HTTP/2 request.
#[derive(Debug, Clone, Copy)]
enum H2Expect {
    /// Forward exactly this request line and body, and pass the upstream's 200 on.
    Forwarded(&'static str, &'static str),
    /// Answer the stream with this status. The upstream gets nothing.
    Refused(u16),
    /// Reset the stream with `PROTOCOL_ERROR`. The upstream gets no body: nothing, a head on
    /// a connection that is then closed, or (a declared length of zero) a bodyless request.
    Reset,
    /// End the connection with `GOAWAY PROTOCOL_ERROR`. The upstream gets nothing.
    GoAway,
}

impl Harness {
    async fn h2_attack(&self, what: &str, frames: &[Frame], expect: H2Expect) {
        let (received, outcome) = self.h2_exchange(what, frames).await;
        match expect {
            H2Expect::Forwarded(line, body) => {
                outcome
                    .check(self, what, Expect::Forwarded(line, body))
                    .await;
            }
            H2Expect::Refused(status) => outcome.check(self, what, Expect::Refused(status)).await,
            H2Expect::Reset => {
                let reset = received.of(Frame::RST_STREAM).next();
                assert_eq!(
                    reset.and_then(Frame::error_code),
                    Some(Frame::PROTOCOL_ERROR),
                    "{what}: {received:?}"
                );
                for seen in &outcome.upstream {
                    assert!(seen.body.is_empty(), "{what}: {seen:?}");
                    assert!(!seen.line().contains("/smuggled"), "{what}: {seen:?}");
                }
            }
            H2Expect::GoAway => {
                assert_eq!(
                    received.goaway().map(|(code, _)| code),
                    Some(Frame::PROTOCOL_ERROR),
                    "{what}: {received:?}"
                );
                assert!(outcome.upstream.is_empty(), "{what}: {outcome:?}");
            }
        }
    }
}

/// The pseudo-headers of a request to `raw.example.com`, then `extra`.
fn h2_head<'a>(
    method: &'a str,
    path: &'a str,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        (":method", method),
        (":scheme", "https"),
        (":authority", "raw.example.com"),
        (":path", path),
    ];
    fields.extend_from_slice(extra);
    fields
}

/// An HTTP/2 body is as long as its DATA frames. A `content-length` that says otherwise is
/// a protocol error, not a second opinion for the upstream.
#[tokio::test]
async fn h2_bodies_are_as_long_as_their_data() {
    let h = Harness::start().await;
    let post = |extra: &[(&'static str, &'static str)], data: &[u8]| {
        vec![
            Frame::headers(1, false, &h2_head("POST", "/a", extra)),
            Frame::data(1, true, data),
        ]
    };
    let smuggling = [b"hello", SMUGGLED.as_bytes()].concat();
    let chunked = format!("0\r\n\r\n{SMUGGLED}");
    let cases = [
        (
            "a matching length",
            post(&[("content-length", "5")], b"hello"),
            POSTED_H2,
        ),
        // Without a length the upstream gets the body chunked.
        ("no length", post(&[], b"hello"), POSTED_H2),
        (
            "the same length twice",
            post(
                &[("content-length", "5"), ("content-length", "5")],
                b"hello",
            ),
            POSTED_H2,
        ),
        (
            "more data than the length",
            post(&[("content-length", "5")], &smuggling),
            H2Expect::Reset,
        ),
        (
            "less data than the length",
            post(&[("content-length", "50")], b"hello"),
            H2Expect::Reset,
        ),
        (
            "data after a zero length",
            post(&[("content-length", "0")], SMUGGLED.as_bytes()),
            H2Expect::Reset,
        ),
        (
            "a list of lengths",
            post(&[("content-length", "5, 5")], b"hello"),
            H2Expect::Reset,
        ),
        (
            "a signed length",
            post(&[("content-length", "+5")], b"hello"),
            H2Expect::Reset,
        ),
        (
            "a length on a request without a body",
            vec![Frame::headers(
                1,
                true,
                &h2_head("GET", "/a", &[("content-length", "5")]),
            )],
            H2Expect::Reset,
        ),
        // HTTP/2 has no chunked coding; a client that sends the header anyway is refused
        // rather than have the upstream decode the DATA as chunks.
        (
            "transfer-encoding: chunked",
            post(&[("transfer-encoding", "chunked")], chunked.as_bytes()),
            H2Expect::Reset,
        ),
        (
            "transfer-encoding with a length",
            post(
                &[("transfer-encoding", "chunked"), ("content-length", "5")],
                chunked.as_bytes(),
            ),
            H2Expect::Reset,
        ),
    ];
    for (what, frames, expect) in cases {
        h.h2_attack(what, &frames, expect).await;
    }

    // Trailers end the body; they are not forwarded as part of it.
    let trailers = [
        Frame::headers(1, false, &h2_head("POST", "/a", &[])),
        Frame::data(1, false, b"hello"),
        Frame::headers(1, true, &[("x-trailer", "1")]),
    ];
    h.h2_attack("trailers", &trailers, POSTED_H2).await;
}

const POSTED_H2: H2Expect = H2Expect::Forwarded("POST /a HTTP/1.1", "hello");

/// What HTTP/2 forbids in a header block (RFC 9113 §8.2) would become HTTP/1.1 syntax when
/// the request is written out for the upstream: a line break in a value is a new header, one
/// in `:path` a new request. h2 and Pingora refuse all of it before hoster sees a request.
#[tokio::test]
async fn h2_headers_cannot_carry_http_1_syntax() {
    let h = Harness::start().await;
    let get = |extra: &[(&'static str, &'static str)]| {
        vec![Frame::headers(1, true, &h2_head("GET", "/a", extra))]
    };
    let path = |path: &'static str| vec![Frame::headers(1, true, &h2_head("GET", path, &[]))];
    let got = H2Expect::Forwarded("GET /a HTTP/1.1", "");
    let cases = [
        ("a plain request", get(&[]), got),
        ("te: trailers", get(&[("te", "trailers")]), got),
        // Connection-specific headers.
        (
            "connection: keep-alive",
            get(&[("connection", "keep-alive")]),
            H2Expect::Reset,
        ),
        (
            "connection: close",
            get(&[("connection", "close")]),
            H2Expect::Reset,
        ),
        (
            "keep-alive",
            get(&[("keep-alive", "timeout=5")]),
            H2Expect::Reset,
        ),
        (
            "proxy-connection",
            get(&[("proxy-connection", "keep-alive")]),
            H2Expect::Reset,
        ),
        ("upgrade", get(&[("upgrade", "websocket")]), H2Expect::Reset),
        ("te: gzip", get(&[("te", "gzip")]), H2Expect::Reset),
        // Line breaks and NUL in values, and anything but a lowercase token as a name.
        (
            "CRLF in a value",
            get(&[("x-a", "b\r\ntransfer-encoding: chunked")]),
            H2Expect::GoAway,
        ),
        (
            "LF in a value",
            get(&[("x-a", "b\nx-b: c")]),
            H2Expect::GoAway,
        ),
        (
            "CR in a value",
            get(&[("x-a", "b\rx-b: c")]),
            H2Expect::GoAway,
        ),
        ("NUL in a value", get(&[("x-a", "b\0c")]), H2Expect::GoAway),
        ("a space in a name", get(&[("x a", "b")]), H2Expect::GoAway),
        (
            "a colon and CRLF in a name",
            get(&[("x-a: b\r\nx-c", "d")]),
            H2Expect::GoAway,
        ),
        (
            "CRLF in a name",
            get(&[("x-a\r\nx-b", "d")]),
            H2Expect::GoAway,
        ),
        (
            "an uppercase name",
            get(&[("X-Upper", "b")]),
            H2Expect::GoAway,
        ),
        (
            "an unknown pseudo-header",
            get(&[(":foo", "bar")]),
            H2Expect::GoAway,
        ),
        // The request line's parts.
        (
            "a request in :path",
            path("/a HTTP/1.1\r\nx-s: 1\r\n\r\nGET /smuggled"),
            H2Expect::Reset,
        ),
        ("a space in :path", path("/a b"), H2Expect::Reset),
        ("a :path without a slash", path("a"), H2Expect::Reset),
        ("an empty :path", path(""), H2Expect::Reset),
        (
            "a URL as :path",
            path("http://app.example.com/x"),
            H2Expect::Reset,
        ),
        (
            "a request in :method",
            vec![Frame::headers(
                1,
                true,
                &h2_head("GET /smuggled HTTP/1.1\r\nx:", "/a", &[]),
            )],
            H2Expect::GoAway,
        ),
        (
            ":path twice",
            vec![Frame::headers(
                1,
                true,
                &h2_head("GET", "/a", &[(":path", "/b")]),
            )],
            H2Expect::Reset,
        ),
        (
            "a pseudo-header after a header",
            vec![Frame::headers(
                1,
                true,
                &[
                    (":method", "GET"),
                    (":scheme", "https"),
                    (":authority", "raw.example.com"),
                    ("x-a", "b"),
                    (":path", "/a"),
                ],
            )],
            H2Expect::Reset,
        ),
    ];
    for (what, frames, expect) in cases {
        h.h2_attack(what, &frames, expect).await;
    }
}

/// `:authority` is the request's host: it must be the SNI name (ADR-0032), and a `host`
/// header may not say anything else (RUSTSEC-2026-0388).
#[tokio::test]
async fn h2_requests_name_one_host() {
    let h = Harness::start().await;
    let head = |authority: Option<&'static str>, hosts: &[&'static str]| {
        let mut fields = vec![(":method", "GET"), (":scheme", "https")];
        fields.extend(authority.map(|authority| (":authority", authority)));
        fields.push((":path", "/a"));
        fields.extend(hosts.iter().map(|host| ("host", *host)));
        vec![Frame::headers(1, true, &fields)]
    };
    let raw = "raw.example.com";
    let got = H2Expect::Forwarded("GET /a HTTP/1.1", "");
    let cases = [
        (head(Some(raw), &[]), got),
        (head(Some(raw), &[raw]), got),
        (head(None, &[raw]), got),
        (
            head(Some(raw), &["app.example.com"]),
            H2Expect::Refused(400),
        ),
        (
            head(None, &[raw, "app.example.com"]),
            H2Expect::Refused(400),
        ),
        (head(None, &[]), H2Expect::Refused(400)),
        (
            head(Some("app.example.com@raw.example.com"), &[]),
            H2Expect::Refused(400),
        ),
        (head(Some("app.example.com"), &[]), H2Expect::Refused(421)),
        (head(None, &["app.example.com"]), H2Expect::Refused(421)),
        (
            head(Some("raw.example.com\r\nx-s: 1"), &[]),
            H2Expect::Reset,
        ),
    ];
    for (frames, expect) in cases {
        let what = format!("{:?}", String::from_utf8_lossy(&frames[0].payload));
        h.h2_attack(&what, &frames, expect).await;
    }
    for seen in h.h2_exchange("host", &head(None, &[raw])).await.1.upstream {
        assert_eq!(seen.header("host"), [raw]);
    }
}

// ---------------------------------------------------------------------------------------------
// Routing

/// Paths are matched and forwarded as the client wrote them (ADR-0009): the proxy does not
/// decode, merge slashes or resolve dot segments, before matching or after. So no spelling
/// reaches a *different* service than its literal segments name, and `strip_prefix` removes
/// exactly the matched prefix. Left of each arrow is the request target on
/// `raw.example.com`, which has `/` and a stripped `/api`; right of it what the upstream
/// gets, and the prefix it is told.
///
/// What this does not give: a prefix is not a boundary inside one service. `/api/../x`
/// matches `/api` and reaches an upstream that resolves dot segments as its `/x` (OQ-40).
#[tokio::test]
async fn paths_are_matched_and_forwarded_literally() {
    let h = Harness::start().await;
    let api = Some("/api");
    let cases = [
        ("/api/x", "/x", api),
        ("/api", "/", api),
        ("/api/x?a=b#c", "/x?a=b", api),
        ("/api?/../x", "/?/../x", api),
        ("/api#/x", "/", api),
        // Dot segments stay where they are.
        ("/api/../secret", "/../secret", api),
        ("/api/..", "/..", api),
        ("/api/./x", "/./x", api),
        ("/api/%2e%2e/x", "/%2e%2e/x", api),
        ("/api/..%2fsecret", "/..%2fsecret", api),
        ("/api/..;/x", "/..;/x", api),
        ("/api//x", "//x", api),
        ("/api/%0d%0aX:%20y", "/%0d%0aX:%20y", api),
        ("/../api/x", "/../api/x", None),
        ("/x/../api/y", "/x/../api/y", None),
        ("/./api/x", "/./api/x", None),
        ("//api/x", "//api/x", None),
        // Not the `api` segment: encoded, another case, or with something attached.
        ("/api%2fx", "/api%2fx", None),
        ("/%61pi/x", "/%61pi/x", None),
        ("/API/x", "/API/x", None),
        ("/apix", "/apix", None),
        ("/api.", "/api.", None),
        ("/api;p/x", "/api;p/x", None),
        ("/api\\x", "/api\\x", None),
        ("/api%00/x", "/api%00/x", None),
        ("/api%20/x", "/api%20/x", None),
    ];
    for (target, upstream, prefix) in cases {
        let head = format!("GET {target} HTTP/1.1\r\n{HOST}Connection: close\r\n\r\n");
        let outcome = h
            .raw_exchange(target, "raw.example.com", head.as_bytes())
            .await;
        assert_eq!(
            outcome.lines(),
            [format!("GET {upstream} HTTP/1.1")],
            "{target}"
        );
        let told = outcome.upstream[0].header("x-forwarded-prefix");
        assert_eq!(told, Vec::from_iter(prefix), "{target}");
    }
}

/// What a client says about itself in forwarding headers. `X-Forwarded-Proto`, `-Host` and
/// `-Prefix` are replaced (ADR-0010). Still open in OQ-38, and pinned here as it is today:
/// `X-Forwarded-For` is appended to, and `Forwarded` and `X-Real-IP` pass through.
#[tokio::test]
async fn client_forwarding_headers_are_replaced_except_where_oq_38_is_open() {
    let h = Harness::start().await;
    let sent = [
        ("x-forwarded-for", "1.2.3.4"),
        ("x-forwarded-proto", "http"),
        ("x-forwarded-host", "evil.example.com"),
        ("x-forwarded-prefix", "/evil"),
        ("forwarded", "for=1.2.3.4"),
        ("x-real-ip", "1.2.3.4"),
    ];
    let headers: String = sent
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let h1 = h
        .raw_exchange(
            "h1",
            "raw.example.com",
            format!("GET /api/x HTTP/1.1\r\n{HOST}{headers}\r\n").as_bytes(),
        )
        .await;
    let (_, h2) = h
        .h2_exchange(
            "h2",
            &[Frame::headers(1, true, &h2_head("GET", "/api/x", &sent))],
        )
        .await;
    for outcome in [h1, h2] {
        let [seen] = outcome.upstream.as_slice() else {
            panic!("{outcome:?}");
        };
        assert_eq!(seen.ambiguities(), [""; 0], "{seen:?}");
        assert_eq!(seen.header("x-forwarded-proto"), ["https"]);
        assert_eq!(seen.header("x-forwarded-host"), ["raw.example.com"]);
        assert_eq!(seen.header("x-forwarded-prefix"), ["/api"]);
        assert_eq!(seen.header("x-forwarded-for"), ["1.2.3.4, 127.0.0.1"]);
        assert_eq!(seen.header("forwarded"), ["for=1.2.3.4"]);
        assert_eq!(seen.header("x-real-ip"), ["1.2.3.4"]);
    }
}
