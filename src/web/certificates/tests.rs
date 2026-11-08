//! The certificates page through the whole app against a fake [`CertificateControl`], its
//! templates on their own, and the production [`CertificateAccess`] against a real orchestrator
//! (fake runtime) and state store.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Mutex;

use tempfile::TempDir;

use super::*;
use crate::acme::{PublicAddresses, Resolve};
use crate::config::PortRange;
use crate::ports::PortAllocator;
use crate::registry::{Anonymous, RegistryClient};
use crate::runtime::FakeRuntime;
use crate::secrets::{SecretKey, SecretStore};
use crate::store::Store;
use crate::web::auth::tests::TestApp;
use crate::web::stacks::tests::{FORM, body, encoded, post};

const HOUR: Duration = Duration::from_secs(60 * 60);

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// A canned overview; records the actions asked for.
#[derive(Default)]
pub struct FakeCertificates {
    pub overview: Mutex<Option<CertOverview>>,
    /// What "Check DNS now" finds, by host; other hosts are unknown.
    pub dns: Mutex<HashMap<String, DnsVerdict>>,
    /// Hosts backing off, with the end of their backoff.
    pub backing_off: Mutex<HashMap<String, SystemTime>>,
    pub retried: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl CertificateControl for FakeCertificates {
    async fn overview(&self) -> CertOverview {
        self.overview
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(CertOverview {
                source: "the A/AAAA records of admin.example.com".into(),
                public: BTreeSet::new(),
                hosts: Vec::new(),
            })
    }

    async fn check_dns(&self, host: &str) -> Result<DnsVerdict, CertActionError> {
        self.dns
            .lock()
            .unwrap()
            .get(host)
            .cloned()
            .ok_or_else(|| CertActionError::UnknownHost(host.to_owned()))
    }

    async fn retry(&self, host: &str) -> Result<(), CertActionError> {
        if let Some(retry_at) = self.backing_off.lock().unwrap().get(host) {
            return Err(CertActionError::BackingOff {
                host: host.to_owned(),
                retry_at: *retry_at,
                left: retry_at
                    .duration_since(SystemTime::now())
                    .unwrap_or_default(),
            });
        }
        self.retried.lock().unwrap().push(host.to_owned());
        Ok(())
    }
}

fn mismatch() -> DnsMismatch {
    DnsMismatch {
        host: "notes.example.com".into(),
        observed: vec![ip("203.0.113.9")],
        expected: vec![ip("192.0.2.1"), ip("2001:db8::1")],
    }
}

/// A status with some fields set (its private bookkeeping stays at the default).
fn status(set: impl FnOnce(&mut CertStatus)) -> CertStatus {
    let mut status = CertStatus::default();
    set(&mut status);
    status
}

/// An issued admin host, a host failing with a DNS mismatch, and one not attempted yet.
fn overview(now: SystemTime) -> CertOverview {
    CertOverview {
        source: "the A/AAAA records of admin.example.com".into(),
        public: BTreeSet::from([ip("192.0.2.1"), ip("2001:db8::1")]),
        hosts: vec![
            CertRow {
                host: "admin.example.com".into(),
                issuer: Some("Let's Encrypt R11".into()),
                validity: Some(Validity {
                    not_before: now - 30 * 24 * HOUR,
                    not_after: now + 60 * 24 * HOUR,
                }),
                status: status(|s| s.dns_checked = true),
                backoff: None,
            },
            CertRow {
                host: "notes.example.com".into(),
                issuer: None,
                validity: None,
                status: status(|s| {
                    s.error = Some("validation failed: <connection refused>".into());
                    s.retry_at = Some(now + HOUR);
                    s.dns_mismatch = Some(mismatch());
                    s.dns_checked = true;
                }),
                backoff: Some(Backoff {
                    failures: 3,
                    retry_at: now + HOUR,
                }),
            },
            CertRow {
                host: "new.example.com".into(),
                issuer: None,
                validity: None,
                status: CertStatus::default(),
                backoff: None,
            },
        ],
    }
}

fn page(now: SystemTime) -> String {
    CertificatesPage::new(
        Layout::new(
            Section::Certificates,
            &Session::for_tests("octocat", "c0ffee"),
        ),
        &overview(now),
        now,
    )
    .render()
    .unwrap()
}

/// The `<tr>` of `host` in `html`.
fn row<'a>(html: &'a str, host: &str) -> &'a str {
    let at = html
        .find(&format!("<td><code>{host}</code></td>"))
        .unwrap_or_else(|| panic!("no row for {host}: {html}"));
    let end = html[at..].find("</tr>").unwrap();
    &html[at..at + end]
}

#[test]
fn the_page_shows_the_public_addresses_and_one_row_per_host() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let html = page(now);
    assert!(
        html.contains("<code>192.0.2.1, 2001:db8::1</code>"),
        "{html}"
    );
    assert!(
        html.contains("(from the A/AAAA records of admin.example.com)"),
        "{html}"
    );

    let admin = row(&html, "admin.example.com");
    assert!(admin.contains("Let&#39;s Encrypt R11"), "{admin}");
    assert!(
        admin.contains(&humantime::format_rfc3339_seconds(now + 60 * 24 * HOUR).to_string()),
        "{admin}"
    );
    assert!(
        admin.contains(r#"<span class="badge badge-ok">ok</span>"#),
        "{admin}"
    );
    assert!(
        admin.contains(&format!(
            "renewal from {}",
            humantime::format_rfc3339_seconds(now + 30 * 24 * HOUR)
        )),
        "{admin}"
    );

    // The mismatch row: observed vs expected, the escaped error, the time to the retry.
    let notes = row(&html, "notes.example.com");
    assert!(notes.contains("TLS pending"), "{notes}");
    assert!(
        notes.contains(r#"<span class="badge badge-error">mismatch</span>"#),
        "{notes}"
    );
    assert!(
        notes.contains(
            "resolves to <code>203.0.113.9</code>; hoster is <code>192.0.2.1, 2001:db8::1</code>"
        ),
        "{notes}"
    );
    assert!(
        notes.contains("validation failed: &#60;connection refused&#62;"),
        "{notes}"
    );
    assert!(notes.contains("in 1h ("), "{notes}");
    assert!(
        notes.contains(r##"hx-post="/certificates/dns" hx-target="#dns-1""##),
        "{notes}"
    );
    assert!(notes.contains(r#"<td id="dns-1">"#), "{notes}");
    assert!(
        notes.contains(r#"hx-post="/certificates/retry""#),
        "{notes}"
    );

    let new = row(&html, "new.example.com");
    assert!(new.contains("not checked yet"), "{new}");
    assert!(new.contains("<td>pending</td>"), "{new}");
}

#[test]
fn an_attempt_without_known_public_addresses_is_not_shown_as_ok() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let mut overview = overview(now);
    overview.public.clear();
    let html = CertificatesPage::new(
        Layout::new(
            Section::Certificates,
            &Session::for_tests("octocat", "c0ffee"),
        ),
        &overview,
        now,
    )
    .render()
    .unwrap();
    assert!(html.contains("<strong>unknown</strong>"), "{html}");
    let admin = row(&html, "admin.example.com");
    assert!(!admin.contains("badge-ok"), "{admin}");
    assert!(admin.contains("public addresses are unknown"), "{admin}");
    // A mismatch recorded earlier still names the addresses it was compared with.
    let notes = row(&html, "notes.example.com");
    assert!(notes.contains("badge-error\">mismatch"), "{notes}");
}

#[test]
fn unknown_hosts_and_unknown_public_addresses_are_said_so() {
    let html = Certificates::refused(
        &CertActionError::UnknownHost("x.example.com".into()),
        SystemTime::now(),
    )
    .render()
    .unwrap();
    assert!(
        html.contains("Hoster manages no certificate for x.example.com."),
        "{html}"
    );

    let cell = DnsCell {
        dns: DnsView::from(&DnsVerdict::Unknown),
    }
    .render()
    .unwrap();
    assert!(cell.contains("public addresses are unknown"), "{cell}");
}

#[tokio::test]
async fn the_page_is_in_the_navigation() {
    let app = TestApp::new().await;
    *app.certificates.overview.lock().unwrap() = Some(overview(SystemTime::now()));
    let browser = app.sign_in().await;
    let response = app.get("/certificates", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(r#"<a href="/certificates" aria-current="page">Certificates</a>"#),
        "{html}"
    );
    assert!(html.contains("<code>notes.example.com</code>"), "{html}");
}

#[tokio::test]
async fn check_dns_now_answers_with_the_hosts_dns_cell() {
    let app = TestApp::new().await;
    app.certificates.dns.lock().unwrap().extend([
        (
            "notes.example.com".to_owned(),
            DnsVerdict::Mismatch(mismatch()),
        ),
        ("admin.example.com".to_owned(), DnsVerdict::Matches),
    ]);
    let browser = app.sign_in().await;
    let check = |host: &str| {
        post(
            "/certificates/dns",
            &browser,
            FORM,
            encoded(&[("host", host)]),
        )
    };

    let response = app.send(check("notes.example.com")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("mismatch"), "{html}");
    assert!(html.contains("<code>203.0.113.9</code>"), "{html}");
    assert!(!html.contains("<html"), "a partial, not a page: {html}");

    let html = body(app.send(check("admin.example.com")).await).await;
    assert_eq!(html, r#"<span class="badge badge-ok">ok</span>"#);

    let response = app.send(check("other.example.com")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retry_now_is_refused_with_the_time_left_while_backing_off() {
    let app = TestApp::new().await;
    let retry_at = SystemTime::now() + Duration::from_secs(4 * 60 + 30);
    app.certificates
        .backing_off
        .lock()
        .unwrap()
        .insert("notes.example.com".into(), retry_at);
    let browser = app.sign_in().await;
    let retry = |host: &str| {
        post(
            "/certificates/retry",
            &browser,
            FORM,
            encoded(&[("host", host)]),
        )
    };

    let response = app.send(retry("notes.example.com")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let html = body(response).await;
    assert!(html.contains("Not retrying notes.example.com"), "{html}");
    assert!(html.contains("4m 2") || html.contains("4m 30s"), "{html}");
    assert!(
        html.contains(&humantime::format_rfc3339_seconds(retry_at).to_string()),
        "{html}"
    );
    assert!(app.certificates.retried.lock().unwrap().is_empty());

    let response = app.send(retry("new.example.com")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("Asked the ACME task to try new.example.com now"),
        "{html}"
    );
    assert_eq!(
        *app.certificates.retried.lock().unwrap(),
        ["new.example.com"]
    );
}

// ---------------------------------------------------------------------------------------------
// The production control
// ---------------------------------------------------------------------------------------------

struct Records(HashMap<&'static str, Vec<IpAddr>>);

#[async_trait::async_trait]
impl Resolve for Records {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.0
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "NXDOMAIN"))
    }
}

struct Production {
    _dir: TempDir,
    store: Store,
    orchestrator: Arc<Orchestrator<FakeRuntime>>,
    access: CertificateAccess<FakeRuntime>,
}

impl Production {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        let orchestrator = Orchestrator::new(
            store.clone(),
            Arc::new(FakeRuntime::new()),
            Arc::new(RegistryClient::new(Arc::new(Anonymous)).unwrap()),
            PortAllocator::new(store.clone(), PortRange::try_from(20000..=20015).unwrap()),
            SecretStore::new(store.clone(), &SecretKey::generate()),
            Default::default(),
            Arc::default(),
        );
        let (certs, _) = CertMap::open(&dir.path().join("state"), None).unwrap();
        let dns = DnsCheck::new(
            PublicAddresses::Configured(BTreeSet::from([ip("192.0.2.1")])),
            Arc::new(Records(HashMap::from([(
                "admin.example.com",
                vec![ip("203.0.113.9")],
            )]))),
        );
        let access = CertificateAccess {
            orchestrator: Arc::clone(&orchestrator),
            certs,
            dns,
            backoffs: Arc::new(store.clone()),
            admin_host: "admin.example.com".into(),
        };
        Self {
            _dir: dir,
            store,
            orchestrator,
            access,
        }
    }
}

#[tokio::test]
async fn the_admin_host_is_always_listed_with_the_configured_addresses() {
    let p = Production::new().await;
    let overview = p.access.overview().await;
    assert_eq!(overview.source, "acme.public_addresses in hoster.toml");
    assert_eq!(overview.public, BTreeSet::from([ip("192.0.2.1")]));
    assert_eq!(
        overview.hosts,
        [CertRow {
            host: "admin.example.com".into(),
            issuer: None,
            validity: None,
            status: CertStatus::default(),
            backoff: None,
        }]
    );

    // Checking DNS records the outcome in the orchestrator's status.
    let verdict = p.access.check_dns("Admin.example.com").await.unwrap();
    assert!(matches!(verdict, DnsVerdict::Mismatch(_)), "{verdict:?}");
    let status = &p.orchestrator.certificates()["admin.example.com"];
    assert!(status.dns_checked);
    assert_eq!(
        status.dns_mismatch.as_ref().unwrap().observed,
        [ip("203.0.113.9")]
    );
    assert!(matches!(
        p.access.check_dns("other.example.com").await,
        Err(CertActionError::UnknownHost(_))
    ));
}

#[tokio::test]
async fn retry_is_gated_on_the_persisted_backoff() {
    let p = Production::new().await;
    assert!(matches!(
        p.access.retry("admin.example.com").await,
        Err(CertActionError::AcmeStopped)
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    p.orchestrator.connect_acme(tx);

    let retry_at = SystemTime::now() + HOUR;
    let backoffs = BTreeMap::from([(
        "admin.example.com".to_owned(),
        Backoff {
            failures: 2,
            retry_at,
        },
    )]);
    BackoffStore::save(&p.store, &backoffs).await.unwrap();
    match p.access.retry("admin.example.com").await {
        Err(CertActionError::BackingOff {
            left, retry_at: at, ..
        }) => {
            assert_eq!(at, retry_at);
            assert!(
                left > HOUR - Duration::from_secs(60) && left <= HOUR,
                "{left:?}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        p.access.overview().await.hosts[0].backoff,
        backoffs.get("admin.example.com").copied()
    );
    assert!(rx.try_recv().is_err(), "nothing was sent while backing off");

    let elapsed = BTreeMap::from([(
        "admin.example.com".to_owned(),
        Backoff {
            failures: 2,
            retry_at: SystemTime::now() - HOUR,
        },
    )]);
    BackoffStore::save(&p.store, &elapsed).await.unwrap();
    p.access.retry("admin.example.com").await.unwrap();
    assert_eq!(
        rx.try_recv().unwrap(),
        crate::acme::AcmeCommand::Retry {
            host: "admin.example.com".into()
        }
    );
    assert!(matches!(
        p.access.retry("other.example.com").await,
        Err(CertActionError::UnknownHost(_))
    ));
}
