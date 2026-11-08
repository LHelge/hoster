//! Issuance against a scripted fake CA, plus a gated end-to-end test against Pebble through the
//! real `:80` and `:443` listeners.

use std::sync::{Arc, Mutex};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};

use super::*;

/// What the fake CA does with an order.
#[derive(Debug, Clone)]
enum Script {
    /// Validates the challenge and issues the certificate.
    Issue,
    /// Refuses to open the order with this problem.
    RejectOrder(Problem),
    /// Offers no HTTP-01 challenge.
    NoHttp01,
    /// The order turns invalid, optionally recording why on the challenge.
    Invalid(Option<Problem>),
    /// `set_ready` fails with this error.
    SetReadyError(fn() -> instant_acme::Error),
    /// `poll_ready` fails with this error.
    ReadyError(fn() -> instant_acme::Error),
    /// `poll_certificate` fails with this error.
    CertificateError(fn() -> instant_acme::Error),
}

/// Something the fake CA saw.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    /// An order for these identifiers.
    Order(String),
    /// `set_ready` for `token`, with what `:80` would have answered for it at that moment.
    Ready {
        token: String,
        served: Option<String>,
    },
    Finalize,
    Certificate,
}

/// A throwaway CA signing one leaf per order, scripted per test.
struct FakeCa {
    script: Script,
    issuer: rcgen::Issuer<'static, KeyPair>,
    root_pem: String,
    /// The map `:80` reads, to observe what is served when.
    challenges: ChallengeMap,
    events: Arc<Mutex<Vec<Event>>>,
    /// The renewal information it answers with (`None`: no ARI), and how often it was asked.
    ari: Mutex<Option<(RenewalWindow, Duration)>>,
    ari_asks: Mutex<u32>,
}

impl FakeCa {
    fn new(script: Script, challenges: ChallengeMap) -> Arc<Self> {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "fake ACME CA");
        let root = params.self_signed(&key).unwrap();
        Arc::new(Self {
            script,
            issuer: rcgen::Issuer::new(params, key),
            root_pem: root.pem(),
            challenges,
            events: Arc::default(),
            ari: Mutex::default(),
            ari_asks: Mutex::default(),
        })
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn record(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

#[async_trait]
impl Ca for Arc<FakeCa> {
    async fn new_order(&self, host: &str) -> Result<Box<dyn CaOrder>, instant_acme::Error> {
        self.record(Event::Order(host.to_owned()));
        if let Script::RejectOrder(problem) = &self.script {
            return Err(instant_acme::Error::Api(problem.clone()));
        }
        Ok(Box::new(FakeOrder {
            ca: Arc::clone(self),
            host: host.to_owned(),
            key: None,
        }))
    }

    async fn renewal_window(
        &self,
        _id: &CertificateIdentifier<'_>,
    ) -> Result<Option<(RenewalWindow, Duration)>, instant_acme::Error> {
        *self.ari_asks.lock().unwrap() += 1;
        Ok(*self.ari.lock().unwrap())
    }
}

struct FakeOrder {
    ca: Arc<FakeCa>,
    host: String,
    key: Option<KeyPair>,
}

impl FakeOrder {
    fn token(&self) -> String {
        format!("token-{}", self.host)
    }
}

#[async_trait]
impl CaOrder for FakeOrder {
    async fn http01_challenges(&mut self) -> Result<Vec<Http01>, IssueFailure> {
        if let Script::NoHttp01 = self.ca.script {
            return Err(IssueFailure::NoHttp01);
        }
        let token = self.token();
        Ok(vec![Http01 {
            key_authorization: format!("{token}.thumbprint"),
            token,
        }])
    }

    async fn set_ready(&mut self, challenge: &Http01) -> Result<(), instant_acme::Error> {
        let token = challenge.token.clone();
        let served = self.ca.challenges.get(&token).map(|s| s.to_string());
        self.ca.record(Event::Ready { token, served });
        match &self.ca.script {
            Script::SetReadyError(error) => Err(error()),
            _ => Ok(()),
        }
    }

    async fn poll_ready(
        &mut self,
        _retry: &RetryPolicy,
    ) -> Result<OrderStatus, instant_acme::Error> {
        match &self.ca.script {
            Script::Invalid(_) => Ok(OrderStatus::Invalid),
            Script::ReadyError(error) => Err(error()),
            _ => Ok(OrderStatus::Ready),
        }
    }

    async fn validation_problem(&mut self) -> Option<Problem> {
        match &self.ca.script {
            Script::Invalid(problem) => problem.clone(),
            _ => None,
        }
    }

    async fn finalize(&mut self) -> Result<String, instant_acme::Error> {
        self.ca.record(Event::Finalize);
        let key = KeyPair::generate().unwrap();
        let pem = key.serialize_pem();
        self.key = Some(key);
        Ok(pem)
    }

    async fn poll_certificate(
        &mut self,
        _retry: &RetryPolicy,
    ) -> Result<String, instant_acme::Error> {
        self.ca.record(Event::Certificate);
        if let Script::CertificateError(error) = &self.ca.script {
            return Err(error());
        }
        let key = self.key.as_ref().expect("finalized before polling");
        let mut params = CertificateParams::new(vec![self.host.clone()]).unwrap();
        // What ARI identifies a certificate by, together with its serial.
        params.use_authority_key_identifier_extension = true;
        let leaf = params.signed_by(key, &self.ca.issuer).unwrap();
        Ok(format!("{}{}", leaf.pem(), self.ca.root_pem))
    }
}

fn problem(kind: &str, detail: &str) -> Problem {
    serde_json::from_value(serde_json::json!({
        "type": format!("urn:ietf:params:acme:error:{kind}"),
        "detail": detail,
        "status": 400,
    }))
    .unwrap()
}

/// An issuer backed by a fake CA following `script`, with an empty `certs/` in a tempdir.
struct Fixture {
    issuer: Issuer,
    ca: Arc<FakeCa>,
    challenges: ChallengeMap,
    certs: CertMap,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(script: Script) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let challenges = ChallengeMap::default();
        let ca = FakeCa::new(script, challenges.clone());
        let issuer = Issuer::with_ca(Arc::new(Arc::clone(&ca)), challenges.clone(), certs.clone());
        Self {
            issuer,
            ca,
            challenges,
            certs,
            dir,
        }
    }

    async fn fail(&self, host: &str) -> IssueFailure {
        let error = self.issuer.issue(host).await.unwrap_err();
        assert_eq!(error.host, host.to_ascii_lowercase());
        // However the order ends, nothing is left on :80 and nothing new is served.
        assert!(
            self.challenges
                .get(&format!("token-{}", error.host))
                .is_none()
        );
        assert!(self.certs.hosts().is_empty());
        error.failure
    }
}

#[tokio::test]
async fn issues_installs_and_withdraws_the_challenge() {
    let fx = Fixture::new(Script::Issue);
    let validity = fx.issuer.issue("App.Example.com").await.unwrap();

    let token = "token-app.example.com".to_owned();
    assert_eq!(
        fx.ca.events(),
        vec![
            Event::Order("app.example.com".to_owned()),
            Event::Ready {
                token: token.clone(),
                served: Some(format!("{token}.thumbprint")),
            },
            Event::Finalize,
            Event::Certificate,
        ],
        "one single-name order; the answer is served before the CA is told it is ready"
    );
    assert!(fx.challenges.get(&token).is_none(), "challenge withdrawn");

    assert!(fx.certs.get("app.example.com").is_some());
    assert_eq!(fx.certs.validity("app.example.com"), Some(validity));
    let host_dir = fx.dir.path().join("certs/app.example.com");
    for file in ["fullchain.pem", "privkey.pem", "meta.json"] {
        assert!(host_dir.join(file).is_file(), "{file} written");
    }
}

#[tokio::test(start_paused = true)]
async fn renewal_information_is_asked_again_only_after_its_retry_after() {
    let fx = Fixture::new(Script::Issue);
    assert_eq!(fx.issuer.renewal_window("app.example.com").await, None);
    assert_eq!(
        *fx.ca.ari_asks.lock().unwrap(),
        0,
        "no certificate, nothing to ask"
    );

    fx.issuer.issue("app.example.com").await.unwrap();
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let window = RenewalWindow {
        start,
        end: start + Duration::from_secs(3600),
    };
    let retry_after = Duration::from_secs(6 * 3600);
    *fx.ca.ari.lock().unwrap() = Some((window, retry_after));
    let asks = || *fx.ca.ari_asks.lock().unwrap();

    assert_eq!(
        fx.issuer.renewal_window("app.example.com").await,
        Some(window)
    );
    tokio::time::advance(retry_after - Duration::from_secs(1)).await;
    assert_eq!(
        fx.issuer.renewal_window("app.example.com").await,
        Some(window)
    );
    assert_eq!(asks(), 1, "the answer stands until Retry-After");

    tokio::time::advance(Duration::from_secs(1)).await;
    *fx.ca.ari.lock().unwrap() = None;
    assert_eq!(fx.issuer.renewal_window("app.example.com").await, None);
    assert_eq!(asks(), 2);

    // A new certificate is asked about at once.
    *fx.ca.ari.lock().unwrap() = Some((window, retry_after));
    fx.issuer.issue("app.example.com").await.unwrap();
    assert_eq!(
        fx.issuer.renewal_window("app.example.com").await,
        Some(window)
    );
    assert_eq!(asks(), 3);
}

#[tokio::test]
async fn each_host_gets_its_own_order_and_certificate() {
    let fx = Fixture::new(Script::Issue);
    fx.issuer.issue("a.example.com").await.unwrap();
    fx.issuer.issue("b.example.com").await.unwrap();
    let orders: Vec<_> = fx
        .ca
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::Order(_)))
        .collect();
    assert_eq!(
        orders,
        vec![
            Event::Order("a.example.com".to_owned()),
            Event::Order("b.example.com".to_owned()),
        ]
    );
    assert_eq!(fx.certs.hosts(), vec!["a.example.com", "b.example.com"]);
}

#[tokio::test]
async fn an_invalid_order_reports_the_ca_detail() {
    let fx = Fixture::new(Script::Invalid(Some(problem(
        "connection",
        "127.0.0.1: Connection refused",
    ))));
    match fx.fail("app.example.com").await {
        IssueFailure::Validation { detail } => {
            assert_eq!(detail, "127.0.0.1: Connection refused");
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }
    assert!(!fx.ca.events().contains(&Event::Finalize));
}

#[tokio::test]
async fn an_invalid_order_without_detail_is_still_a_validation_failure() {
    let fx = Fixture::new(Script::Invalid(None));
    match fx.fail("app.example.com").await {
        IssueFailure::Validation { detail } => assert_eq!(detail, "the order became Invalid"),
        other => panic!("expected a validation failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_validation_problem_while_polling_is_a_validation_failure() {
    fn unauthorized() -> instant_acme::Error {
        instant_acme::Error::Api(problem("unauthorized", "wrong key authorization"))
    }
    let fx = Fixture::new(Script::ReadyError(unauthorized));
    assert!(matches!(
        fx.fail("app.example.com").await,
        IssueFailure::Validation { detail } if detail == "wrong key authorization"
    ));
}

#[tokio::test]
async fn rate_limits_are_typed() {
    let mut limited = problem("rateLimited", "too many certificates already issued");
    limited.subproblems = serde_json::from_value(serde_json::json!([
        { "type": "urn:ietf:params:acme:error:rateLimited", "detail": "for app.example.com" }
    ]))
    .unwrap();
    let fx = Fixture::new(Script::RejectOrder(limited));
    match fx.fail("app.example.com").await {
        IssueFailure::RateLimited { detail } => assert_eq!(
            detail,
            "too many certificates already issued; for app.example.com"
        ),
        other => panic!("expected a rate limit, got {other:?}"),
    }
}

#[tokio::test]
async fn timeouts_are_typed_and_withdraw_the_challenge() {
    fn timeout() -> instant_acme::Error {
        instant_acme::Error::Timeout(None)
    }
    let fx = Fixture::new(Script::ReadyError(timeout));
    assert!(matches!(
        fx.fail("app.example.com").await,
        IssueFailure::Timeout
    ));

    let fx = Fixture::new(Script::CertificateError(timeout));
    assert!(matches!(
        fx.fail("app.example.com").await,
        IssueFailure::Timeout
    ));
}

#[tokio::test]
async fn other_ca_errors_keep_their_source() {
    let fx = Fixture::new(Script::RejectOrder(problem(
        "rejectedIdentifier",
        "policy forbids issuing for this name",
    )));
    match fx.fail("app.example.com").await {
        IssueFailure::Acme(instant_acme::Error::Api(problem)) => {
            assert_eq!(
                problem.detail.as_deref(),
                Some("policy forbids issuing for this name")
            );
        }
        other => panic!("expected an ACME error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_challenge_the_ca_refuses_is_withdrawn() {
    fn unauthorized() -> instant_acme::Error {
        instant_acme::Error::Api(problem("unauthorized", "account is not authorized"))
    }
    let fx = Fixture::new(Script::SetReadyError(unauthorized));
    assert!(matches!(
        fx.fail("app.example.com").await,
        IssueFailure::Validation { detail } if detail == "account is not authorized"
    ));
    assert!(!fx.ca.events().contains(&Event::Finalize));
}

#[tokio::test]
async fn a_host_the_store_would_refuse_is_never_ordered() {
    for host in ["", "*.example.com", "../etc", "example.com."] {
        let fx = Fixture::new(Script::Issue);
        let error = fx.issuer.issue(host).await.unwrap_err();
        assert_eq!(error.host, host);
        assert!(
            matches!(
                error.failure,
                IssueFailure::Store(CertError::InvalidHost { .. })
            ),
            "{host:?}: {:?}",
            error.failure
        );
        assert!(fx.ca.events().is_empty(), "{host:?} reached the CA");
    }
}

#[tokio::test]
async fn a_ca_without_http01_is_reported() {
    let fx = Fixture::new(Script::NoHttp01);
    assert!(matches!(
        fx.fail("app.example.com").await,
        IssueFailure::NoHttp01
    ));
}

#[test]
fn the_error_names_the_host_and_keeps_the_failure_as_source() {
    let error = IssueError {
        host: "app.example.com".to_owned(),
        failure: IssueFailure::RateLimited {
            detail: "slow down".to_owned(),
        },
    };
    assert_eq!(
        error.to_string(),
        "cannot issue a certificate for app.example.com"
    );
    let source = std::error::Error::source(&error).unwrap();
    assert_eq!(source.to_string(), "rate limited by the CA: slow down");
}

/// Against a Pebble container, through hoster's own `:80` (on Pebble's HTTP-01 port) and `:443`:
/// issues a certificate for `localhost` and checks it is the one served for that SNI name.
///
/// Pebble must reach hoster's `:80` on the port it validates against, so it shares the host
/// network and gets `httpPort` overridden in its config:
///
/// ```sh
/// podman create --name pebble-files ghcr.io/letsencrypt/pebble
/// podman cp pebble-files:/test/config/pebble-config.json /tmp/pebble.json
/// podman cp pebble-files:/test/certs/pebble.minica.pem /tmp/pebble.minica.pem
/// podman rm pebble-files
/// sed -i 's/"httpPort": [0-9]*/"httpPort": 18080/' /tmp/pebble.json
/// podman run --rm -d --name pebble --network host -e PEBBLE_VA_NOSLEEP=1 \
///   -v /tmp/pebble.json:/pebble.json:ro,Z ghcr.io/letsencrypt/pebble -config /pebble.json
/// TEST_HOSTER_PEBBLE_DIRECTORY=https://localhost:14000/dir \
/// TEST_HOSTER_PEBBLE_ROOT=/tmp/pebble.minica.pem \
/// TEST_HOSTER_PEBBLE_HTTP_PORT=18080 \
///   cargo test pebble -- --ignored --test-threads=1
/// ```
///
/// `--test-threads=1`: both Pebble tests use the one HTTP-01 port.
#[tokio::test]
#[ignore = "needs a Pebble ACME server; see the doc comment"]
async fn pebble_issues_over_http01_and_the_certificate_is_served() {
    use std::net::{Ipv6Addr, SocketAddr};
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use arc_swap::ArcSwap;
    use rustls::pki_types::ServerName;
    use tokio::sync::watch;

    use crate::acme::AccountStore;
    use crate::config::{AcmeConfig, ProxyConfig};
    use crate::proxy::{Proxy, ProxyState};
    use crate::routing::RouteTable;

    const HOST: &str = "localhost";
    let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let config = AcmeConfig {
        directory_url: env("TEST_HOSTER_PEBBLE_DIRECTORY"),
        root_certificate: Some(PathBuf::from(env("TEST_HOSTER_PEBBLE_ROOT"))),
        ..AcmeConfig::default()
    };
    let http_port: u16 = env("TEST_HOSTER_PEBBLE_HTTP_PORT").parse().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let (certs, _) = CertMap::open(dir.path(), Some(config.directory_url.clone())).unwrap();
    let challenges = ChallengeMap::default();
    let https = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let proxy_config = ProxyConfig {
        // Dual-stack, so Pebble reaches it whether `localhost` resolves to 127.0.0.1 or ::1.
        http_listen: SocketAddr::from((Ipv6Addr::UNSPECIFIED, http_port)),
        https_listen: https,
        threads: NonZeroUsize::new(1).unwrap(),
        grace_period: Duration::from_secs(1),
        graceful_shutdown_timeout: Duration::from_secs(1),
        ..ProxyConfig::default()
    };
    let state = ProxyState {
        routes: Arc::new(ArcSwap::from_pointee(RouteTable::default())),
        certs: certs.clone(),
        challenges: challenges.clone(),
        traffic: Default::default(),
    };
    let (shutdown, rx) = watch::channel(None);
    let mut running = Proxy::new(&proxy_config, state).spawn(rx).unwrap();
    running.events.ready(Duration::from_secs(10)).await.unwrap();

    let account = AccountStore::new(dir.path())
        .load_or_create(&config)
        .await
        .expect("ACME account");
    let issuer = Issuer::new(account, challenges.clone(), certs.clone());
    let validity = issuer.issue(HOST).await.expect("issued");
    assert!(validity.not_after > std::time::SystemTime::now());

    // The served leaf is the one on disk, and it names only HOST (ADR-0035).
    let fullchain = std::fs::read(dir.path().join("certs/localhost/fullchain.pem")).unwrap();
    let leaf = {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject;
        CertificateDer::pem_slice_iter(&fullchain)
            .next()
            .unwrap()
            .unwrap()
    };
    let (_, parsed) = x509_parser::parse_x509_certificate(&leaf).unwrap();
    let names: Vec<String> = parsed
        .subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, vec![format!("DNSName({HOST})")]);

    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyServerCert))
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(https).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(ServerName::try_from(HOST).unwrap(), tcp)
        .await
        .expect("handshake for the issued host");
    let served = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
    assert_eq!(
        served, leaf,
        "the issued certificate is served for its SNI name"
    );

    shutdown
        .send(Some(pingora::server::ShutdownSignal::FastShutdown))
        .unwrap();
    running.thread.join().unwrap();
}

/// Against Pebble as above, with nothing listening on its HTTP-01 port: the CA's validation
/// error comes back as a typed failure with its detail, and nothing is installed.
#[tokio::test]
#[ignore = "needs a Pebble ACME server; see pebble_issues_over_http01_and_the_certificate_is_served"]
async fn pebble_validation_failure_carries_the_ca_detail() {
    use std::path::PathBuf;

    use crate::acme::AccountStore;
    use crate::config::AcmeConfig;

    let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let config = AcmeConfig {
        directory_url: env("TEST_HOSTER_PEBBLE_DIRECTORY"),
        root_certificate: Some(PathBuf::from(env("TEST_HOSTER_PEBBLE_ROOT"))),
        ..AcmeConfig::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let (certs, _) = CertMap::open(dir.path(), None).unwrap();
    let challenges = ChallengeMap::default();
    let account = AccountStore::new(dir.path())
        .load_or_create(&config)
        .await
        .expect("ACME account");
    let issuer = Issuer::new(account, challenges, certs.clone());

    let error = issuer.issue("localhost").await.unwrap_err();
    match error.failure {
        IssueFailure::Validation { detail } => {
            assert!(!detail.is_empty());
            assert_ne!(detail, "the order became Invalid", "the CA's own detail");
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }
    assert!(certs.hosts().is_empty());
}

/// Accepts any server certificate: the Pebble test compares the served leaf byte for byte instead
/// of chasing Pebble's per-run root.
#[derive(Debug)]
struct AnyServerCert;

impl rustls::client::danger::ServerCertVerifier for AnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
