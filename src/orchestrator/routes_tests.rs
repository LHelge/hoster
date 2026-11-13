//! The `RouteTable` the orchestrator publishes (architecture §6.4): after a commit, during a
//! stop-first gap, after an abort, a removal and a restart.

use std::sync::Arc;
use std::time::Duration;

use super::tests::{Harness, NOTES, digest, harness, harness_with};
use super::*;
use crate::routing::{DesiredStack, Target};
use crate::runtime::Operation;

const BLOG: &str = r#"
version = 1
name = "blog"

[services.web]
image = "ghcr.io/acme/blog-web"

[[services.web.routes]]
host = "blog.example.com"
port = 80
"#;

impl Harness {
    /// The upstream the published table sends `host` + `path` to.
    fn upstream(&self, host: &str, path: &str) -> Option<Arc<crate::routing::Upstream>> {
        match &self.routes.load().lookup(host, path)?.target {
            Target::Loopback(upstream) => Some(Arc::clone(upstream)),
            _ => None,
        }
    }

    fn port(&self, host: &str, path: &str) -> Option<u16> {
        self.upstream(host, path).map(|u| u.addr.port())
    }

    fn unavailable(&self, host: &str, path: &str) -> bool {
        self.routes
            .load()
            .lookup(host, path)
            .is_some_and(|r| matches!(r.target, Target::Unavailable { .. }))
    }

    /// The loopback port `stack`'s `service` currently publishes `container` on.
    async fn pointer_port(&self, stack: &str, service: &str, container: u16) -> u16 {
        let pointers = self.store.services(stack).await.unwrap();
        let pointer = pointers.iter().find(|p| p.service == service).unwrap();
        pointer.ports[&container]
    }

    async fn deploy_to(&self, stack: &str, images: &[(&str, u8)]) -> DeployOutcome {
        let images = images
            .iter()
            .map(|(s, d)| ((*s).to_owned(), digest(*d)))
            .collect();
        self.orchestrator
            .deploy(stack, images, DeploymentSource::Webhook)
            .await
            .unwrap()
    }

    /// NOTES bootstrapped, plus BLOG applied and its web deployed.
    async fn two_stacks(&self) {
        self.bootstrap().await;
        self.apply(BLOG).await;
        let deployed = self.deploy_to("blog", &[("web", 1)]).await;
        assert_eq!(self.finish(&deployed).await.state, S::Live);
    }
}

use DeploymentState as S;

#[tokio::test]
async fn a_deployment_publishes_every_route_on_its_new_upstreams() {
    let h = harness().await;
    assert_eq!(h.routes.load().version(), 0);
    h.bootstrap().await;

    let web = h.upstream("notes.example.com", "/").unwrap();
    assert_eq!(web.addr.port(), h.pointer_port("notes", "web", 80).await);
    assert_eq!(web.generation, 2);
    assert_eq!(
        h.port("notes.example.com", "/api/notes"),
        Some(h.pointer_port("notes", "api", 8080).await)
    );
    assert_eq!(
        h.port("s3.notes.example.com", "/bucket"),
        Some(h.pointer_port("notes", "storage", 9000).await)
    );
    let version = h.routes.load().version();
    assert!(version > 0);

    // A second deployment of api only: api moves, web keeps the very same upstream.
    let deployed = h.deploy(&[("api", 2)]).await;
    assert_eq!(h.finish(&deployed).await.state, S::Live);
    assert!(h.routes.load().version() > version);
    assert_eq!(
        h.port("notes.example.com", "/api"),
        Some(h.pointer_port("notes", "api", 8080).await)
    );
    assert_eq!(
        h.upstream("notes.example.com", "/api").unwrap().generation,
        3
    );
    assert!(Arc::ptr_eq(
        &web,
        &h.upstream("notes.example.com", "/").unwrap()
    ));
}

#[tokio::test]
async fn two_stacks_deploying_concurrently_never_lose_each_others_routes() {
    let h = harness().await;
    h.two_stacks().await;
    let notes_api = h.pointer_port("notes", "api", 8080).await;
    let blog_web = h.pointer_port("blog", "web", 80).await;

    // Both deployments wait in `Pulling` until released, in the order the test chooses.
    let notes_hold = h.runtime.hold_pulls("notes-api");
    let blog_hold = h.runtime.hold_pulls("blog-web");
    let notes = h.deploy(&[("api", 2)]).await;
    let blog = h.deploy_to("blog", &[("web", 2)]).await;
    notes_hold.reached().await;
    blog_hold.reached().await;
    assert_eq!(h.port("notes.example.com", "/api"), Some(notes_api));
    assert_eq!(h.port("blog.example.com", "/"), Some(blog_web));

    // The later one commits first.
    blog_hold.release();
    assert_eq!(h.finish(&blog).await.state, S::Live);
    let blog_web2 = h.pointer_port("blog", "web", 80).await;
    assert_ne!(blog_web2, blog_web);
    assert_eq!(h.port("blog.example.com", "/"), Some(blog_web2));
    assert_eq!(h.port("notes.example.com", "/api"), Some(notes_api));
    assert!(h.port("notes.example.com", "/").is_some());
    assert!(h.port("s3.notes.example.com", "/").is_some());

    notes_hold.release();
    assert_eq!(h.finish(&notes).await.state, S::Live);
    let notes_api2 = h.pointer_port("notes", "api", 8080).await;
    assert_ne!(notes_api2, notes_api);
    assert_eq!(h.port("notes.example.com", "/api"), Some(notes_api2));
    assert_eq!(h.port("blog.example.com", "/"), Some(blog_web2));
    assert!(h.port("notes.example.com", "/").is_some());
    assert!(h.port("s3.notes.example.com", "/").is_some());
}

/// NOTES with a changed storage env: a storage-only (stop-first) deployment.
fn storage_changed() -> String {
    NOTES.replace(
        "data = \"/data\"",
        "data = \"/data\"\n\n[services.storage.env]\nA = \"1\"",
    )
}

#[tokio::test]
async fn a_stop_first_service_is_unavailable_during_its_gap_only() {
    let h = harness_with(Duration::from_millis(300)).await;
    h.bootstrap().await;
    let old = h.upstream("s3.notes.example.com", "/").unwrap();
    let web = h.upstream("notes.example.com", "/").unwrap();

    let applied = h.apply(&storage_changed()).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !h.unavailable("s3.notes.example.com", "/") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the gap is published");
    // Only the stop-first service is in its gap.
    assert!(Arc::ptr_eq(
        &web,
        &h.upstream("notes.example.com", "/").unwrap()
    ));

    let done = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(done.state, S::Live, "{done:?}");
    let new = h.upstream("s3.notes.example.com", "/").unwrap();
    assert!(!Arc::ptr_eq(&old, &new));
    assert_eq!(
        new.addr.port(),
        h.pointer_port("notes", "storage", 9000).await
    );
}

#[tokio::test]
async fn an_aborted_stop_first_replacement_restores_the_old_upstream() {
    let h = harness().await;
    h.bootstrap().await;
    let old = h.upstream("s3.notes.example.com", "/").unwrap();
    // In a storage-only deployment the first start is the new storage container's.
    h.runtime.fail_next(
        Operation::Start,
        RuntimeError::Api {
            operation: Operation::Start,
            message: "boom".into(),
        },
    );
    let applied = h.apply(&storage_changed()).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed);
    assert!(Arc::ptr_eq(
        &old,
        &h.upstream("s3.notes.example.com", "/").unwrap()
    ));
}

#[tokio::test]
async fn a_removed_stack_loses_its_routes_and_the_others_keep_theirs() {
    let h = harness().await;
    h.two_stacks().await;
    let blog = h.upstream("blog.example.com", "/").unwrap();
    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    let table = h.routes.load();
    assert!(table.lookup("notes.example.com", "/").is_none());
    assert!(table.lookup("notes.example.com", "/api").is_none());
    assert!(table.lookup("s3.notes.example.com", "/").is_none());
    assert!(Arc::ptr_eq(
        &blog,
        &h.upstream("blog.example.com", "/").unwrap()
    ));
}

#[tokio::test]
async fn a_service_token_verifies_while_its_stack_is_committed_and_not_after_removal() {
    use crate::secrets::{KnownServices as _, SecretKey, TokenError, TokenKey};

    let name = |s: &str| s.parse::<Name>().unwrap();
    let key = TokenKey::from(&SecretKey::generate());
    let h = harness().await;
    h.two_stacks().await;
    let known = &*h.orchestrator;
    assert!(known.contains(&name("notes"), &name("storage")));
    assert!(known.contains(&name("blog"), &name("web")));
    assert!(!known.contains(&name("blog"), &name("api")));
    assert!(!known.contains(&name("gone"), &name("web")));

    let token = key.token(&name("notes"), &name("web")).to_string();
    let verified = token.parse::<crate::secrets::ServiceToken>().unwrap();
    assert_eq!(verified.verify(&key, known).unwrap().stack, name("notes"));

    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    let presented = token.parse::<crate::secrets::ServiceToken>().unwrap();
    assert!(matches!(
        presented.verify(&key, known),
        Err(TokenError::Unknown { .. })
    ));
    assert!(known.contains(&name("blog"), &name("web")));
}

#[tokio::test]
async fn a_route_stays_owned_until_the_deployment_dropping_it_commits() {
    let h = harness().await;
    h.two_stacks().await;
    // blog drops its route in a revision whose deployment fails, so the route is still served.
    h.runtime.fail_next(
        Operation::PullImage,
        RuntimeError::Api {
            operation: Operation::PullImage,
            message: "boom".into(),
        },
    );
    let moved = BLOG.replace("blog.example.com", "www.blog.example.com");
    let applied = h.apply(&moved).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed);
    assert!(h.port("blog.example.com", "/").is_some());

    let claim = NOTES.replace("s3.notes.example.com", "blog.example.com");
    let err = h.orchestrator.apply(&claim).await.unwrap_err();
    assert!(matches!(err, OrchestratorError::Collision(_)), "{err}");
}

#[tokio::test]
async fn a_restarted_orchestrator_restores_the_table_from_the_store() {
    let h = harness().await;
    h.two_stacks().await;
    let routes = Arc::<ArcSwap<RouteTable>>::default();
    let restarted = Orchestrator::new(
        h.store.clone(),
        Arc::clone(&h.runtime),
        Arc::clone(&h.resolver) as Arc<dyn TagResolver>,
        PortAllocator::new(
            h.store.clone(),
            crate::config::PortRange::try_from(20000..=20015).unwrap(),
        ),
        h.secrets.clone(),
        Settings::default(),
        Arc::clone(&routes),
    );
    restarted.recover().await.unwrap();
    let version = restarted.restore_routes().await.unwrap();
    assert_eq!(routes.load().version(), version);
    for (host, path) in [
        ("notes.example.com", "/"),
        ("notes.example.com", "/api"),
        ("s3.notes.example.com", "/"),
        ("blog.example.com", "/"),
    ] {
        let restored = routes.load();
        let Target::Loopback(upstream) = &restored.lookup(host, path).unwrap().target else {
            panic!("{host}{path} is not routed to a container");
        };
        assert_eq!(
            Some(upstream.addr.port()),
            h.port(host, path),
            "{host}{path}"
        );
    }
}

#[tokio::test]
async fn every_publish_logs_its_version_and_diff() {
    use crate::logging::LogFormat;
    use crate::logging::capture::subscriber;

    let h = harness().await;
    let (buffer, subscriber) = subscriber(LogFormat::Json, "hoster=info");
    let guard = tracing::subscriber::set_default(subscriber);
    let applied = h.apply(NOTES).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    drop(guard);

    let lines = buffer.json();
    let published = lines
        .iter()
        .find(|l| l["message"] == "published route table")
        .unwrap_or_else(|| panic!("no publish in {lines:?}"));
    assert_eq!(
        published["version"].as_u64(),
        Some(h.routes.load().version())
    );
    let diff = published["diff"].as_str().unwrap();
    assert!(diff.contains("1 added"), "{diff}");
    assert!(diff.contains("+s3.notes.example.com/"), "{diff}");
}

#[tokio::test]
async fn the_gap_tells_clients_to_retry_after_five_seconds() {
    let h = harness_with(Duration::from_millis(300)).await;
    h.bootstrap().await;
    let applied = h.apply(&storage_changed()).await;
    let retry_after = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Target::Unavailable { retry_after }) = h
                .routes
                .load()
                .lookup("s3.notes.example.com", "/")
                .map(|r| r.target.clone())
            {
                return retry_after;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the gap is published");
    assert_eq!(retry_after, Duration::from_secs(5));
    h.finish(applied.deployment.as_ref().unwrap()).await;
}

#[tokio::test]
async fn a_failed_pointer_commit_puts_the_old_upstreams_back() {
    let h = harness().await;
    h.bootstrap().await;
    let api = h.upstream("notes.example.com", "/api").unwrap();
    let web = h.upstream("notes.example.com", "/").unwrap();
    // The commit transaction fails on its last write, after `Switching` published the table.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", h.store.path().display()))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_commit BEFORE UPDATE OF committed_at ON deployments
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let deployed = h.deploy(&[("api", 2)]).await;
    let halted = h.finish(&deployed).await;
    assert_eq!(halted.state, S::Switching, "{halted:?}");
    assert!(halted.committed_at.is_none());
    // The pointers still name the old generation, so the routes do too.
    assert!(Arc::ptr_eq(
        &api,
        &h.upstream("notes.example.com", "/api").unwrap()
    ));
    assert!(Arc::ptr_eq(
        &web,
        &h.upstream("notes.example.com", "/").unwrap()
    ));
}

fn stack(toml: &str) -> Arc<Stack> {
    Arc::new(toml.parse::<Stack>().unwrap())
}

fn live(stack: &Arc<Stack>, service: &str, port: u16, host_port: u16) -> DesiredStack {
    let service: Name = service.parse().unwrap();
    let upstream =
        crate::routing::Upstream::loopback(host_port, stack.name.clone(), service.clone(), 1);
    DesiredStack::new(Arc::clone(stack)).with_live(service, [(port, Arc::new(upstream))])
}

#[test]
fn a_colliding_stored_stack_is_left_out_and_the_rest_is_restored() {
    let table = Arc::<ArcSwap<RouteTable>>::default();
    let mut routes = routes::Routes::new(
        Arc::clone(&table),
        Reserved::default(),
        Duration::from_secs(5),
    );
    let blog = stack(BLOG);
    let thief = stack(&BLOG.replace("name = \"blog\"", "name = \"thief\""));
    let version = routes.restore([
        live(&blog, "web", 80, 20001),
        live(&thief, "web", 80, 20002),
    ]);

    let published = table.load();
    assert_eq!(published.version(), version);
    let Target::Loopback(upstream) = &published.lookup("blog.example.com", "/").unwrap().target
    else {
        panic!("blog is not routed");
    };
    assert_eq!(upstream.addr.port(), 20001);
    assert_eq!(routes.definitions().count(), 1);
}

#[test]
fn a_change_that_does_not_build_is_dropped_and_later_ones_still_publish() {
    let table = Arc::<ArcSwap<RouteTable>>::default();
    let mut routes = routes::Routes::new(
        Arc::clone(&table),
        Reserved::default(),
        Duration::from_secs(5),
    );
    let blog = stack(BLOG);
    routes.restore([live(&blog, "web", 80, 20001)]);

    // A colliding commit (validation raced) is not published and not kept.
    let thief = stack(&BLOG.replace("name = \"blog\"", "name = \"thief\""));
    let web: Name = "web".parse().unwrap();
    let upstreams = routes::Routes::loopback(
        &thief.name,
        &web,
        1,
        &crate::store::PortMap::from([(80, 20002)]),
    );
    let before = table.load().version();
    routes.commit(Arc::clone(&thief), [(web, upstreams)], &[]);
    assert_eq!(table.load().version(), before);
    assert_eq!(routes.definitions().count(), 1);

    // The next change publishes as usual.
    routes.open_gap(&blog.name, &"web".parse().unwrap());
    assert!(table.load().version() > before);
    assert!(matches!(
        table.load().lookup("blog.example.com", "/").unwrap().target,
        Target::Unavailable { .. }
    ));
}

/// Deploys a worked-example-shaped stack on real rootless podman and sends requests through
/// the real proxy, by `Host` header: `/` to web, `/api` to api, the s3 host to storage. Run with
/// `TEST_HOSTER_PODMAN=1 cargo test`.
#[tokio::test(flavor = "multi_thread")]
async fn routes_the_worked_example_through_the_proxy_on_real_podman() {
    use std::num::NonZeroUsize;

    use pingora::server::ShutdownSignal;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use rustls::pki_types::ServerName;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;

    use crate::acme::{CertMap, ChallengeMap};
    use crate::config::{PortRange, ProxyConfig};
    use crate::proxy::{Proxy, ProxyState};
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};

    if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to deploy on rootless podman");
        return;
    }
    const EXAMPLE: &str = r#"
version = 1
name = "hosterroutes"

[services.api]
image = "ghcr.io/containerd/busybox"
command = ["sh", "-c", "mkdir -p /www/api && echo api > /www/api/index.html && echo ok > /www/healthz && exec httpd -f -p 8080 -h /www"]

[[services.api.routes]]
host = "notes.hoster.test"
prefix = "/api"
port = 8080

[services.web]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "mkdir -p /www && echo web > /www/index.html && echo ok > /www/healthz && exec httpd -f -p 80 -h /www"]

[[services.web.routes]]
host = "notes.hoster.test"
port = 80

[services.storage]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "mkdir -p /www && echo s3 > /www/index.html && echo ok > /www/healthz && exec httpd -f -p 9000 -h /www"]
update = "stop-first"

[[services.storage.routes]]
host = "s3.hoster.test"
port = 9000
"#;
    const HOSTS: [&str; 2] = ["notes.hoster.test", "s3.hoster.test"];

    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
    let runtime = Arc::new(
        PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman"),
    );
    let registry = Arc::new(RegistryClient::new(Arc::new(Anonymous)).unwrap());
    let api_digest = registry
        .resolve(&"ghcr.io/containerd/busybox:1.36".parse().unwrap())
        .await
        .unwrap()
        .digest()
        .to_string();
    let routes = Arc::<ArcSwap<RouteTable>>::default();
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        registry,
        PortAllocator::new(store.clone(), PortRange::try_from(28200..=28215).unwrap()),
        SecretStore::new(store.clone(), &crate::secrets::SecretKey::generate()),
        Settings::default(),
        Arc::clone(&routes),
    );

    // A test CA and a certificate for each host.
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let (certs, _) = CertMap::open(dir.path(), None).unwrap();
    for host in HOSTS {
        let key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec![host.to_owned()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        let chain = format!("{}{}", leaf.pem(), ca_cert.pem());
        certs.insert(host, chain, key.serialize_pem()).unwrap();
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca_cert.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tls = TlsConnector::from(Arc::new(client));

    let bind = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (https, http) = (bind(), bind());
    let config = ProxyConfig {
        https_listen: https.local_addr().unwrap(),
        http_listen: http.local_addr().unwrap(),
        threads: NonZeroUsize::new(1).unwrap(),
        grace_period: Duration::from_secs(1),
        graceful_shutdown_timeout: Duration::from_secs(1),
        ..ProxyConfig::default()
    };
    drop((https, http));
    let (shutdown, rx) = tokio::sync::watch::channel(None);
    let state = ProxyState {
        routes: Arc::clone(&routes),
        certs,
        challenges: ChallengeMap::default(),
        traffic: Default::default(),
        admission: Default::default(),
        logs: crate::logs::LogSink::disconnected(),
        receiver: crate::proxy::ReceiverRoute::new("/nonexistent/otlp.sock".into()),
    };
    let mut proxy = Proxy::new(&config, state).spawn(rx).unwrap();
    proxy.events.ready(Duration::from_secs(10)).await.unwrap();

    let get = |host: &'static str, path: &'static str| {
        let tls = tls.clone();
        let addr = config.https_listen;
        async move {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let name = ServerName::try_from(host.to_owned()).unwrap();
            let mut stream = tls.connect(name, tcp).await.unwrap();
            let request =
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut reply = Vec::new();
            let _ = stream.read_to_end(&mut reply).await;
            String::from_utf8_lossy(&reply).into_owned()
        }
    };

    let applied = orchestrator.apply(EXAMPLE).await.unwrap();
    let first = orchestrator
        .wait(applied.deployment.unwrap().deployment)
        .await
        .unwrap();
    let deployed = orchestrator
        .deploy(
            "hosterroutes",
            DigestSet::from([("api".into(), api_digest)]),
            DeploymentSource::Manual,
        )
        .await
        .unwrap();
    let second = orchestrator.wait(deployed.deployment).await.unwrap();
    let mut replies = Vec::new();
    for (host, path) in [
        ("notes.hoster.test", "/"),
        ("notes.hoster.test", "/api/"),
        ("s3.hoster.test", "/"),
    ] {
        replies.push(get(host, path).await);
    }
    let removed = orchestrator
        .remove(RemoveStack::keeping_volumes("hosterroutes"))
        .await;
    let after_removal = get("notes.hoster.test", "/").await;
    shutdown.send_replace(Some(ShutdownSignal::FastShutdown));

    assert_eq!(first.state, S::Live, "{first:?}");
    assert_eq!(second.state, S::Live, "{second:?}");
    removed.unwrap();
    for (reply, body) in replies.iter().zip(["web", "api", "s3"]) {
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        let (_, content) = reply.split_once("\r\n\r\n").unwrap();
        assert_eq!(content.trim(), body, "{reply}");
    }
    assert!(after_removal.starts_with("HTTP/1.1 404"), "{after_removal}");
}
