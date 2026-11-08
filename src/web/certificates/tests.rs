//! The certificates page through the whole app against a fake [`CertificateControl`], its
//! templates on their own, and the production [`CertificateAccess`] against a real orchestrator
//! (fake runtime) and state store.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Mutex;

use axum::http::Request;
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
                directory: AcmeDirectory::LetsEncrypt,
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
                owner: CertOwner::Admin,
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
                owner: CertOwner::Stacks(vec!["notes".into()]),
            },
            CertRow {
                host: "new.example.com".into(),
                issuer: None,
                validity: None,
                status: CertStatus::default(),
                backoff: None,
                owner: CertOwner::Stacks(vec!["new".into(), "next".into()]),
            },
        ],
        directory: AcmeDirectory::LetsEncrypt,
    }
}

#[test]
fn failing_hosts_are_the_erroring_the_pending_and_the_expired_ones() {
    let now = SystemTime::now();
    let mut overview = overview(now);
    // Expired, with no error recorded since startup.
    overview.hosts.push(CertRow {
        host: "old.example.com".into(),
        issuer: Some("Let's Encrypt R11".into()),
        validity: Some(Validity {
            not_before: now - 100 * 24 * HOUR,
            not_after: now - 10 * 24 * HOUR,
        }),
        status: CertStatus::default(),
        backoff: None,
        owner: CertOwner::Stacks(vec![]),
    });
    let failing: Vec<_> = overview
        .hosts
        .iter()
        .filter(|row| row.is_failing(now))
        .map(|row| row.host.as_str())
        .collect();
    assert_eq!(
        failing,
        ["notes.example.com", "new.example.com", "old.example.com"]
    );
    assert_eq!(overview.failing(now), 3);
}

fn page(now: SystemTime) -> String {
    CertificatesPage::new(
        Layout::new(
            Section::Certificates,
            &crate::web::auth::Session::for_tests("octocat", "c0ffee"),
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
        .find(&format!(
            r#"<td><code class="cert-host">{host}</code></td>"#
        ))
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
        admin.contains(&format!(
            r#"<span class="pill pill-ok">{}ok</span>"#,
            crate::web::Icon::Ok
        )),
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
    // No certificate yet, and the last attempt failed.
    assert!(
        notes.contains(&format!(
            r#"<span class="pill pill-fail">{}failed</span>"#,
            crate::web::Icon::Failed
        )),
        "{notes}"
    );
    assert!(notes.contains("no certificate yet"), "{notes}");
    assert!(
        notes.contains(r#"<a href="/stacks/notes">notes</a>"#),
        "{notes}"
    );
    assert!(
        notes.contains(&format!(
            r#"<span class="pill pill-warn">{}mismatch</span>"#,
            crate::web::Icon::Warning
        )),
        "{notes}"
    );
    assert!(
        notes.contains(
            "resolves to <code>203.0.113.9</code>; hoster is <code>192.0.2.1, 2001:db8::1</code>"
        ),
        "{notes}"
    );
    assert!(
        notes.contains("validation failed:<wbr> &#60;connection refused&#62;"),
        "{notes}"
    );
    assert!(notes.contains("in 1h ("), "{notes}");
    assert!(
        notes.contains(r##"hx-post="/certificates/dns" hx-target="#dns-1""##),
        "{notes}"
    );
    assert!(
        notes.contains(r#"<td id="dns-1" class="cert-dns">"#),
        "{notes}"
    );
    assert!(
        notes.contains(r#"hx-post="/certificates/retry""#),
        "{notes}"
    );

    let new = row(&html, "new.example.com");
    assert!(new.contains("not checked yet"), "{new}");
    assert!(new.contains("next: pending"), "{new}");
    assert!(new.contains("pill-progress"), "{new}");
    assert!(
        new.contains(r#"<a href="/stacks/new">new</a>, <a href="/stacks/next">next</a>"#),
        "{new}"
    );
    assert!(admin.contains("hoster admin"), "{admin}");
}

#[test]
fn an_attempt_without_known_public_addresses_is_not_shown_as_ok() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let mut overview = overview(now);
    overview.public.clear();
    let html = CertificatesPage::new(
        Layout::new(
            Section::Certificates,
            &crate::web::auth::Session::for_tests("octocat", "c0ffee"),
        ),
        &overview,
        now,
    )
    .render()
    .unwrap();
    assert!(html.contains("<strong>unknown</strong>"), "{html}");
    let admin = row(&html, "admin.example.com");
    let dns = &admin[admin.find("cert-dns").unwrap()..];
    let dns = &dns[..dns.find("</td>").unwrap()];
    assert!(!dns.contains("pill-ok"), "{dns}");
    assert!(admin.contains("public addresses are unknown"), "{admin}");
    // A mismatch recorded earlier still names the addresses it was compared with.
    let notes = row(&html, "notes.example.com");
    assert!(
        notes.contains(&format!(
            "pill-warn\">{}mismatch",
            crate::web::Icon::Warning
        )),
        "{notes}"
    );
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
    let overview = overview(SystemTime::now());
    let failing = overview.failing(SystemTime::now());
    assert!(failing > 0, "the fixture has failing hosts");
    *app.certificates.overview.lock().unwrap() = Some(overview);
    let browser = app.sign_in().await;
    let response = app.get("/certificates", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(&format!(
            r#"<a href="/certificates" aria-current="page">{}Certificates <span class="nav-count nav-count-alert" role="img" aria-label="{failing} failing">{failing}</span></a>"#,
            crate::web::Icon::Certificates
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"<code class="cert-host">notes.example.com</code>"#),
        "{html}"
    );
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
    assert_eq!(
        html,
        format!(
            r#"<span class="pill pill-ok">{}ok</span>"#,
            crate::web::Icon::Ok
        )
    );

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

/// The body's rows, in table order, by host.
fn order(body: &CertBody) -> Vec<&str> {
    body.rows.iter().map(|row| row.host.as_str()).collect()
}

fn issued(host: &str, left: Duration, now: SystemTime) -> CertRow {
    CertRow {
        host: host.into(),
        issuer: Some("Let's Encrypt R11".into()),
        validity: Some(Validity {
            not_before: now + left - 90 * 24 * HOUR,
            not_after: now + left,
        }),
        status: CertStatus::default(),
        backoff: None,
        owner: CertOwner::Stacks(vec!["blog".into()]),
    }
}

#[test]
fn rows_sort_problems_first_then_by_expiry() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let mut overview = overview(now);
    overview.hosts.extend([
        issued("later.example.com", 80 * 24 * HOUR, now),
        issued("renewing.example.com", 20 * 24 * HOUR, now),
        issued("expiring.example.com", 5 * 24 * HOUR, now),
    ]);
    let body = CertBody::new(&overview, now);
    assert_eq!(
        order(&body),
        [
            "new.example.com",
            "notes.example.com",
            "expiring.example.com",
            "renewing.example.com",
            "admin.example.com",
            "later.example.com",
        ]
    );
    let standings: Vec<_> = body.rows.iter().map(|row| row.standing).collect();
    assert_eq!(
        standings,
        [
            Standing::Pending,
            Standing::Failed,
            Standing::Expiring,
            Standing::Renewing,
            Standing::Valid,
            Standing::Valid,
        ]
    );
    // The ids follow the table order.
    assert!(body.rows.iter().enumerate().all(|(i, row)| row.index == i));
}

#[test]
fn the_tiles_count_each_standing_and_name_the_acme_directory() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let mut overview = overview(now);
    overview
        .hosts
        .push(issued("renewing.example.com", 20 * 24 * HOUR, now));
    overview.directory = AcmeDirectory::LetsEncryptStaging;
    let tiles: Vec<String> = CertBody::new(&overview, now)
        .tiles
        .iter()
        .map(ToString::to_string)
        .collect();
    let tile = |label: &str| {
        tiles
            .iter()
            .find(|t| t.contains(&format!(r#"<dt class="kpi-label">{label}</dt>"#)))
            .unwrap_or_else(|| panic!("no {label} tile: {tiles:?}"))
            .clone()
    };
    assert!(tile("held").contains(r#"<dd class="kpi-value">2</dd>"#));
    assert!(tile("held").contains("of 4 hosts"));
    assert!(tile("valid").contains(r#"<dd class="kpi-value">1</dd>"#));
    assert!(tile("valid").contains("kpi-ok"));
    let renewing = tile("renewing / pending");
    assert!(
        renewing.contains(r#"<dd class="kpi-value">2</dd>"#),
        "{renewing}"
    );
    assert!(renewing.contains("1 due for renewal · 1 awaiting a first"));
    let failed = tile("failed");
    assert!(failed.contains("kpi-fail"), "{failed}");
    assert!(
        failed.contains("3 attempts · notes.example.com"),
        "{failed}"
    );
    let acme = tile("acme");
    assert!(acme.contains("letsencrypt · staging"), "{acme}");
    assert!(
        acme.contains("acme-staging-v02.api.letsencrypt.org"),
        "{acme}"
    );
    assert!(acme.contains("renews with ⅓ of lifetime left"), "{acme}");
}

#[test]
fn the_glance_lists_the_soonest_expiry_first_with_every_problem() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let day = 24 * HOUR;
    let mut overview = overview(now);
    overview.hosts.extend([
        issued("expiring.example.com", 5 * day, now),
        issued("renewing.example.com", 20 * day, now),
    ]);
    for n in 0..6 {
        overview
            .hosts
            .push(issued(&format!("v{n}.example.com"), (70 + n) * day, now));
    }
    // A failing host with a long-valid certificate is listed even past the first six.
    let mut late = issued("late-failing.example.com", 89 * day, now);
    late.status.error = Some("urn:ietf:params:acme:error:connection".into());
    overview.hosts.push(late);

    let glance = CertGlance::new(&overview, now);
    let hosts: Vec<&str> = glance.rows.iter().map(|r| r.host.as_str()).collect();
    assert_eq!(
        hosts,
        [
            "new.example.com",
            "notes.example.com",
            "expiring.example.com",
            "renewing.example.com",
            "admin.example.com",
            "v0.example.com",
            "late-failing.example.com",
        ],
        "hosts without a certificate first, then by expiry"
    );
    assert_eq!(glance.more_valid, 5);
    assert_eq!(glance.total, 12);
    // Failing: notes (error), new (pending), late-failing; expiring: one.
    assert_eq!((glance.valid, glance.failing, glance.expiring), (8, 3, 1));
    assert_eq!(glance.rows[0].left, "no certificate yet");
    assert!(glance.rows[0].meter.is_none());
    assert_eq!(glance.rows[2].left, "5d left");
    assert!(
        glance.rows[2]
            .meter
            .as_ref()
            .unwrap()
            .to_string()
            .contains("meter-warn")
    );
    assert!(glance.rows[2].pill.to_string().contains("expiring"));

    let kpi = glance.kpi().to_string();
    assert!(kpi.contains("kpi-fail"), "{kpi}");
    assert!(
        kpi.contains(
            r#"<dd class="kpi-value">8 / 12</dd><dd class="kpi-sub">3 failing · 1 expiring</dd>"#
        ),
        "{kpi}"
    );
    let calm = CertGlance::new(
        &CertOverview {
            hosts: vec![issued("a.example.com", 60 * day, now)],
            ..overview
        },
        now,
    );
    assert!(calm.kpi().to_string().contains("kpi-ok"));
    assert_eq!(calm.more_valid, 0);
}

#[test]
fn acme_directories_are_named_by_their_url() {
    let named = |url: &str| AcmeDirectory::from(url).to_string();
    assert_eq!(
        named("https://acme-v02.api.letsencrypt.org/directory"),
        "letsencrypt · prod"
    );
    assert_eq!(
        named("https://acme-staging-v02.api.letsencrypt.org/directory/"),
        "letsencrypt · staging"
    );
    assert_eq!(
        named("https://ca.example.net:14000/acme/dir"),
        "custom · ca.example.net:14000"
    );
}

#[test]
fn the_alert_names_a_failing_renewal_before_a_host_without_a_certificate() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let mut overview = overview(now);
    let body = CertBody::new(&overview, now);
    let alert = body.alert.as_ref().expect("notes has no certificate");
    assert_eq!(alert.host, "notes.example.com");
    assert_eq!(
        alert.title,
        "notes.example.com has no certificate after 3 failed attempts"
    );
    assert_eq!(alert.others, 1, "new.example.com is pending too");
    assert!(
        alert.next_attempt.starts_with("in 1h ("),
        "{}",
        alert.next_attempt
    );

    let mut failing = issued("wiki.example.com", 3 * 24 * HOUR + HOUR, now);
    failing.status.error = Some("challenge timed out".into());
    overview.hosts.push(failing);
    let body = CertBody::new(&overview, now);
    let alert = body.alert.as_ref().unwrap();
    assert_eq!(
        alert.title,
        "wiki.example.com expires in 3 days and renewal is failing"
    );
    assert_eq!(
        alert.error.as_ref().unwrap().to_string(),
        "challenge timed out"
    );

    let html = CertBodyPartial { body }.render().unwrap();
    assert!(
        html.contains(r#"<section class="alert alert-fail" aria-labelledby="cert-alert-title">"#),
        "{html}"
    );
    assert!(!html.contains(r#"role="alert""#), "{html}");
    let at = html.find(r#"class="alert alert-fail""#).unwrap();
    let alert = &html[at..at + html[at..].find("</section>").unwrap()];
    assert!(alert.contains(r#"id="cert-alert-title""#), "{alert}");
    assert!(alert.contains(r##"hx-target="#alert-dns""##), "{alert}");
    assert!(alert.contains(r#"<div id="alert-dns""#), "{alert}");
    assert!(alert.contains(r#"value="wiki.example.com""#), "{alert}");
    assert!(alert.contains("retry now"), "{alert}");

    // An expired certificate outranks both, even with no error recorded (after a restart).
    let mut expired = overview.clone();
    expired.hosts.push(CertRow {
        host: "old.example.com".into(),
        issuer: Some("Let's Encrypt R11".into()),
        validity: Some(Validity {
            not_before: now - 100 * 24 * HOUR,
            not_after: now - 10 * 24 * HOUR,
        }),
        status: CertStatus::default(),
        backoff: None,
        owner: CertOwner::Stacks(vec!["old".into()]),
    });
    let body = CertBody::new(&expired, now);
    let alert = body.alert.as_ref().unwrap();
    assert_eq!(alert.host, "old.example.com");
    assert_eq!(
        alert.title,
        "old.example.com's certificate expired 10 days ago"
    );
    assert!(alert.error.is_none());
    assert_eq!(alert.others, 3);
    let html = CertBodyPartial { body }.render().unwrap();
    assert!(
        html.contains(
            r#"<p class="alert-detail">No failed attempt recorded since hoster started; retry now to renew.</p>"#
        ),
        "{html}"
    );
    assert!(html.contains(r#"value="old.example.com""#), "{html}");

    // Nothing failing: no alert.
    let calm = CertOverview {
        hosts: vec![issued("blog.example.com", 60 * 24 * HOUR, now)],
        ..overview
    };
    assert!(CertBody::new(&calm, now).alert.is_none());
}

#[test]
fn the_header_offers_retry_failing_only_with_failing_hosts() {
    let now = SystemTime::UNIX_EPOCH + 20_000 * 24 * HOUR;
    let html = page(now);
    assert!(
        html.contains(r#"hx-post="/certificates/dns/all""#),
        "{html}"
    );
    assert!(html.contains("Retry failing (2)"), "{html}");
    let calm = CertOverview {
        hosts: vec![issued("blog.example.com", 60 * 24 * HOUR, now)],
        ..overview(now)
    };
    let html = CertificatesPage::new(
        Layout::new(
            Section::Certificates,
            &crate::web::auth::Session::for_tests("octocat", "c0ffee"),
        ),
        &calm,
        now,
    )
    .render()
    .unwrap();
    assert!(!html.contains("Retry failing"), "{html}");
    assert!(html.contains("Check all DNS"), "{html}");
}

#[tokio::test]
async fn check_all_dns_rerenders_the_body_under_a_summary() {
    let app = TestApp::new().await;
    *app.certificates.overview.lock().unwrap() = Some(overview(SystemTime::now()));
    app.certificates.dns.lock().unwrap().extend([
        (
            "notes.example.com".to_owned(),
            DnsVerdict::Mismatch(mismatch()),
        ),
        ("admin.example.com".to_owned(), DnsVerdict::Matches),
        ("new.example.com".to_owned(), DnsVerdict::Matches),
    ]);
    let browser = app.sign_in().await;
    let response = app
        .send(post("/certificates/dns/all", &browser, FORM, ""))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(!html.contains("<html"), "a partial, not a page: {html}");
    assert!(
        html.contains(
            "Checked DNS for 3 hosts: 2 point here, 1 resolves elsewhere (notes.example.com)."
        ),
        "{html}"
    );
    // The flash fills the page's persistent live region out of band.
    assert!(
        html.contains(r#"<div id="cert-result" hx-swap-oob="innerHTML">"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<td id="dns-1" class="cert-dns">"#),
        "{html}"
    );
}

#[tokio::test]
async fn retry_failing_asks_for_every_failing_host_outside_its_backoff() {
    let app = TestApp::new().await;
    *app.certificates.overview.lock().unwrap() = Some(overview(SystemTime::now()));
    let browser = app.sign_in().await;
    let retry = || post("/certificates/retry/failing", &browser, FORM, "");

    let response = app.send(retry()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(
            "Asked the ACME task to try 2 hosts now: notes.example.com, new.example.com."
        ),
        "{html}"
    );
    assert_eq!(
        *app.certificates.retried.lock().unwrap(),
        ["notes.example.com", "new.example.com"]
    );

    // One backing off, one queued.
    app.certificates.retried.lock().unwrap().clear();
    app.certificates
        .backing_off
        .lock()
        .unwrap()
        .insert("notes.example.com".into(), SystemTime::now() + HOUR);
    let html = body(app.send(retry()).await).await;
    assert!(html.contains("try 1 host now: new.example.com."), "{html}");
    assert!(
        html.contains(
            "Not retrying the ones backing off after a failed attempt: notes.example.com (59m"
        ),
        "{html}"
    );

    // Every failing host backing off.
    app.certificates
        .backing_off
        .lock()
        .unwrap()
        .insert("new.example.com".into(), SystemTime::now() + HOUR);
    let response = app.send(retry()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let html = body(response).await;
    assert!(html.contains("every failing host is backing off"), "{html}");

    // Nothing failing.
    *app.certificates.overview.lock().unwrap() = Some(CertOverview {
        hosts: vec![issued(
            "blog.example.com",
            60 * 24 * HOUR,
            SystemTime::now(),
        )],
        ..overview(SystemTime::now())
    });
    let html = body(app.send(retry()).await).await;
    assert!(html.contains("nothing to retry"), "{html}");
}

#[tokio::test]
async fn the_bulk_actions_need_the_csrf_token() {
    let app = TestApp::new().await;
    *app.certificates.overview.lock().unwrap() = Some(overview(SystemTime::now()));
    let browser = app.sign_in().await;
    for uri in ["/certificates/dns/all", "/certificates/retry/failing"] {
        let request = Request::post(uri)
            .header(axum::http::header::COOKIE, &browser.cookie)
            .header(axum::http::header::ORIGIN, crate::web::auth::tests::ORIGIN)
            .header("hx-request", "true")
            .header(axum::http::header::CONTENT_TYPE, FORM)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.send(request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    assert!(app.certificates.retried.lock().unwrap().is_empty());
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
            directory: AcmeDirectory::from("https://pebble.test:14000/dir"),
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
            owner: CertOwner::Admin,
        }]
    );
    assert_eq!(
        overview.directory,
        AcmeDirectory::Custom("https://pebble.test:14000/dir".into())
    );
    // Without a certificate yet, the admin host counts as failing in the navigation.
    assert_eq!(p.access.failing().await, 1);
    assert_eq!(overview.failing(SystemTime::now()), 1);

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

#[test]
fn an_expiry_hours_ago_reads_expired_today() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let expiry = Expiry::new(now - 3 * HOUR, Tone::Fail, now);
    assert_eq!(expiry.left, "expired today");
    let expiry = Expiry::new(now - 50 * HOUR, Tone::Fail, now);
    assert_eq!(expiry.left, "expired 2d ago");
    assert!(
        expiry.time.ends_with('Z') && expiry.time.len() == 6,
        "{}",
        expiry.time
    );
}

#[test]
fn an_acme_error_is_escaped_and_breaks_after_its_colons() {
    let error = AcmeError("urn:ietf:params:acme:error:connection: <refused> & co".into());
    assert_eq!(
        error.to_string(),
        "urn:<wbr>ietf:<wbr>params:<wbr>acme:<wbr>error:<wbr>connection:<wbr> &#60;refused&#62; &#38; co"
    );
}
