use std::collections::BTreeMap;

use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::registry::Anonymous;

const REGISTRY: &str = "registry.test";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const MANIFEST: &str = "/v2/owner/app/manifests/v1";

fn image(s: &str) -> ImageRef {
    s.parse().unwrap()
}

fn client(server: &MockServer, credentials: Arc<dyn RegistryCredentials>) -> RegistryClient {
    RegistryClient::new(credentials)
        .unwrap()
        .with_endpoint(REGISTRY, server.uri().parse().unwrap())
}

fn challenge(realm: &str) -> ResponseTemplate {
    ResponseTemplate::new(401).insert_header(
        "www-authenticate",
        format!(r#"Bearer realm="{realm}",service="{REGISTRY}",scope="repository:owner/app:pull""#)
            .as_str(),
    )
}

fn found() -> ResponseTemplate {
    ResponseTemplate::new(200).insert_header("docker-content-digest", DIGEST)
}

fn token(value: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": value, "expires_in": 300}))
}

/// The unauthenticated `HEAD` gets the challenge (requests carrying a token match other mocks
/// registered with a higher priority).
async fn mount_challenge(server: &MockServer, realm: &str) {
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .respond_with(challenge(realm))
        .with_priority(10)
        .mount(server)
        .await;
}

#[tokio::test]
async fn anonymous_token_then_head_returns_digest_and_caches_token() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    mount_challenge(&server, &realm).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("service", REGISTRY))
        .and(query_param("scope", "repository:owner/app:pull"))
        .respond_with(token("anon-token"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer anon-token"))
        .respond_with(found())
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, Arc::new(Anonymous));
    let image = image("registry.test/owner/app:v1");
    for _ in 0..2 {
        let pinned = client.resolve(&image).await.unwrap();
        assert_eq!(pinned.digest().as_ref(), DIGEST);
        assert_eq!(pinned.tag(), Some("v1"));
        assert_eq!(
            pinned.to_string(),
            format!("registry.test/owner/app@{DIGEST}")
        );
    }
    // The second resolve reused the cached token: one challenge, one token request.
    let requests = server.received_requests().await.unwrap();
    let heads: Vec<_> = requests
        .iter()
        .filter(|r| r.method.as_str() == "HEAD")
        .collect();
    let challenged = heads
        .iter()
        .filter(|r| !r.headers.contains_key("authorization"))
        .count();
    assert_eq!(challenged, 1);
    // Every HEAD asks for indexes and single-platform manifests, OCI and Docker.
    for head in heads {
        let accept = head.headers["accept"].to_str().unwrap();
        for media_type in [
            "application/vnd.oci.image.index.v1+json",
            "application/vnd.docker.distribution.manifest.list.v2+json",
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.v2+json",
        ] {
            assert!(accept.contains(media_type), "{media_type} in {accept}");
        }
    }
}

#[tokio::test]
async fn separate_token_server_gets_basic_auth_from_the_credential() {
    // Docker Hub's shape: the registry and the token server are different hosts.
    let registry = MockServer::start().await;
    let auth = MockServer::start().await;
    mount_challenge(&registry, &format!("{}/token", auth.uri())).await;
    // "bot:secret" in base64.
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(header("authorization", "Basic Ym90OnNlY3JldA=="))
        .respond_with(
            // Docker Hub also sends `access_token`; either field is accepted.
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"access_token": "hub-token"})),
        )
        .expect(1)
        .mount(&auth)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer hub-token"))
        .respond_with(found())
        .mount(&registry)
        .await;

    let credentials = BTreeMap::from([(
        REGISTRY.to_owned(),
        RegistryCredential::new("bot", "secret"),
    )]);
    let pinned = client(&registry, Arc::new(credentials))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap();
    assert_eq!(pinned.digest().as_ref(), DIGEST);
}

#[tokio::test]
async fn rejected_cached_token_is_refreshed_once() {
    let server = MockServer::start().await;
    mount_challenge(&server, &format!("{}/token", server.uri())).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(token("first"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(token("second"))
        .mount(&server)
        .await;
    // "first" works once, then the registry revokes it.
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer first"))
        .respond_with(found())
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer first"))
        .respond_with(challenge(&format!("{}/token", server.uri())))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer second"))
        .respond_with(found())
        .mount(&server)
        .await;

    let client = client(&server, Arc::new(Anonymous));
    let image = image("registry.test/owner/app:v1");
    client.resolve(&image).await.unwrap();
    let pinned = client.resolve(&image).await.unwrap();
    assert_eq!(pinned.digest().as_ref(), DIGEST);
}

#[tokio::test]
async fn persistent_401_is_unauthorized_after_one_refresh() {
    let server = MockServer::start().await;
    mount_challenge(&server, &format!("{}/token", server.uri())).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(token("useless"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header_exists("authorization"))
        .respond_with(challenge(&format!("{}/token", server.uri())))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server, Arc::new(Anonymous))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::Unauthorized {
                status: StatusCode::UNAUTHORIZED,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn rejected_credential_at_the_token_server_is_unauthorized() {
    let server = MockServer::start().await;
    mount_challenge(&server, &format!("{}/token", server.uri())).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let credentials = BTreeMap::from([(
        REGISTRY.to_owned(),
        RegistryCredential::new("bot", "ghp_secretvalue"),
    )]);
    let err = client(&server, Arc::new(credentials))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::Unauthorized { .. }), "{err:?}");
    assert!(!err.to_string().contains("ghp_secretvalue"));
    assert!(!format!("{err:?}").contains("ghp_secretvalue"));
}

#[tokio::test]
async fn rate_limit_is_typed_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server)
        .await;
    let err = client(&server, Arc::new(Anonymous))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::RateLimited { .. }), "{err:?}");
    assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
    assert!(err.to_string().contains("retry after 7s"), "{err}");
}

#[tokio::test]
async fn rate_limited_token_server_is_typed() {
    let server = MockServer::start().await;
    mount_challenge(&server, &format!("{}/token", server.uri())).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let err = client(&server, Arc::new(Anonymous))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::RateLimited {
                retry_after: None,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn basic_challenge_retries_with_the_credential() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Basic Ym90OnNlY3JldA=="))
        .respond_with(found())
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .respond_with(
            ResponseTemplate::new(401).insert_header("www-authenticate", r#"Basic realm="reg""#),
        )
        .with_priority(10)
        .mount(&server)
        .await;
    let credentials = BTreeMap::from([(
        REGISTRY.to_owned(),
        RegistryCredential::new("bot", "secret"),
    )]);
    let pinned = client(&server, Arc::new(credentials))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap();
    assert_eq!(pinned.digest().as_ref(), DIGEST);
}

#[tokio::test]
async fn missing_manifest_and_missing_digest_header() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/owner/app/manifests/v2"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(Anonymous));
    let err = client
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::NotFound { .. }), "{err:?}");
    let err = client
        .resolve(&image("registry.test/owner/app:v2"))
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::Digest { .. }), "{err:?}");
}

#[tokio::test]
async fn digest_only_images_need_no_request_and_untagged_images_fail() {
    let server = MockServer::start().await;
    let client = client(&server, Arc::new(Anonymous));
    let pinned = client
        .resolve(&image(&format!("registry.test/owner/app@{DIGEST}")))
        .await
        .unwrap();
    assert_eq!(pinned.digest().as_ref(), DIGEST);
    let err = client
        .resolve(&image("registry.test/owner/app"))
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::NoTag { .. }), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn plain_http_realm_is_refused_for_an_https_registry() {
    let client = RegistryClient::new(Arc::new(Anonymous)).unwrap();
    let https = Url::parse("https://ghcr.io/").unwrap();
    let err = client
        .realm_url("ghcr.io", &https, "http://evil.test/token")
        .unwrap_err();
    assert!(matches!(err, RegistryError::Challenge { .. }), "{err:?}");
    assert!(
        client
            .realm_url("ghcr.io", &https, "https://ghcr.io/token")
            .is_ok()
    );
}

#[test]
fn docker_hub_uses_its_registry_api_host() {
    let client = RegistryClient::new(Arc::new(Anonymous)).unwrap();
    assert_eq!(
        client.endpoint("docker.io").unwrap().as_str(),
        "https://registry-1.docker.io/"
    );
    assert_eq!(
        client.endpoint("ghcr.io").unwrap().as_str(),
        "https://ghcr.io/"
    );
}

#[test]
fn challenges_parse() {
    let ghcr: Challenge =
        r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/n:pull""#
            .parse()
            .unwrap();
    assert_eq!(
        ghcr,
        Challenge::Bearer {
            realm: "https://ghcr.io/token".to_owned(),
            service: Some("ghcr.io".to_owned()),
            scope: Some("repository:o/n:pull".to_owned()),
        }
    );
    // Commas inside quotes, escapes, unquoted tokens, spacing and case.
    let odd: Challenge =
        r#"bearer  Realm=https://a.test/t , scope="repository:x:pull,push", service="a\"b""#
            .parse()
            .unwrap();
    assert_eq!(
        odd,
        Challenge::Bearer {
            realm: "https://a.test/t".to_owned(),
            service: Some("a\"b".to_owned()),
            scope: Some("repository:x:pull,push".to_owned()),
        }
    );
    assert_eq!(
        r#"Basic realm="x""#.parse::<Challenge>(),
        Ok(Challenge::Basic)
    );
    for bad in [
        r#"Bearer service="x""#,
        r#"Negotiate abc="d""#,
        r#"Bearer realm="unterminated"#,
        "Bearer realm",
    ] {
        assert!(bad.parse::<Challenge>().is_err(), "{bad}");
    }
}

#[test]
fn cached_auth_expires_with_a_margin() {
    let now = Instant::now();
    let auth = CachedAuth::new(
        HeaderValue::from_static("Bearer t"),
        now,
        Duration::from_secs(60),
    );
    assert!(auth.header.is_sensitive());
    assert!(auth.is_fresh(now + Duration::from_secs(49)));
    assert!(!auth.is_fresh(now + Duration::from_secs(50)));
}

#[test]
fn short_token_lifetimes_are_raised_to_the_minimum() {
    let body: TokenResponse = serde_json::from_str(r#"{"token":"t","expires_in":5}"#).unwrap();
    assert_eq!(body.lifetime(), MIN_TOKEN_LIFETIME);
    let body: TokenResponse = serde_json::from_str(r#"{"token":"t"}"#).unwrap();
    assert_eq!(body.lifetime(), MIN_TOKEN_LIFETIME);
    let body: TokenResponse = serde_json::from_str(r#"{"token":"t","expires_in":300}"#).unwrap();
    assert_eq!(body.lifetime(), Duration::from_secs(300));
}

#[test]
fn huge_token_lifetimes_are_capped() {
    let body: TokenResponse =
        serde_json::from_str(&format!(r#"{{"token":"t","expires_in":{}}}"#, u64::MAX)).unwrap();
    assert_eq!(body.lifetime(), MAX_TOKEN_LIFETIME);
    // Caching it must not overflow the expiry instant.
    let auth = CachedAuth::new(
        HeaderValue::from_static("Bearer t"),
        Instant::now(),
        body.lifetime(),
    );
    assert!(auth.is_fresh(Instant::now()));
}

#[test]
fn an_empty_token_field_falls_back_to_access_token() {
    let body: TokenResponse = serde_json::from_str(r#"{"token":"","access_token":"hub"}"#).unwrap();
    assert_eq!(body.into_token().as_deref(), Some("hub"));
    let body: TokenResponse = serde_json::from_str(r#"{"token":"","access_token":""}"#).unwrap();
    assert_eq!(body.into_token(), None);
}

#[tokio::test]
async fn a_token_server_claiming_an_endless_lifetime_does_not_panic() {
    let server = MockServer::start().await;
    mount_challenge(&server, &format!("{}/token", server.uri())).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"token": "t", "expires_in": u64::MAX})),
        )
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(found())
        .mount(&server)
        .await;
    let pinned = client(&server, Arc::new(Anonymous))
        .resolve(&image("registry.test/owner/app:v1"))
        .await
        .unwrap();
    assert_eq!(pinned.digest().as_ref(), DIGEST);
}

// ---------------------------------------------------------------------------------------------
// The "test" action of Settings → Registries
// ---------------------------------------------------------------------------------------------

/// `GET /v2/` answers with a challenge without a scope.
async fn mount_api_challenge(server: &MockServer, realm: &str) {
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{realm}",service="{REGISTRY}""#).as_str(),
        ))
        .mount(server)
        .await;
}

fn bot() -> Arc<dyn RegistryCredentials> {
    Arc::new(BTreeMap::from([(
        REGISTRY.to_owned(),
        RegistryCredential::new("bot", "s3cret-token"),
    )]))
}

#[tokio::test]
async fn check_requests_an_unscoped_token_with_the_stored_credential() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    mount_api_challenge(&server, &realm).await;
    // base64("bot:s3cret-token")
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("service", REGISTRY))
        .and(header("authorization", "Basic Ym90OnMzY3JldC10b2tlbg=="))
        .respond_with(token("t"))
        .expect(1)
        .mount(&server)
        .await;
    let check = client(&server, bot()).check(REGISTRY).await.unwrap();
    assert_eq!(check, RegistryCheck::Authenticated);
    // No scope was asked for: the credential itself is tested.
    let requests = server.received_requests().await.unwrap();
    let token_request = requests.iter().find(|r| r.url.path() == "/token").unwrap();
    assert!(
        !token_request.url.query().unwrap().contains("scope"),
        "{}",
        token_request.url
    );
}

#[tokio::test]
async fn check_without_a_credential_is_anonymous_and_open_registries_say_so() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    mount_api_challenge(&server, &realm).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(token("anon"))
        .mount(&server)
        .await;
    let check = client(&server, Arc::new(Anonymous))
        .check(REGISTRY)
        .await
        .unwrap();
    assert_eq!(check, RegistryCheck::Anonymous);

    let open = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&open)
        .await;
    assert_eq!(
        client(&open, bot()).check(REGISTRY).await.unwrap(),
        RegistryCheck::Open
    );
}

#[tokio::test]
async fn check_failures_carry_their_class_and_never_the_token() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    mount_api_challenge(&server, &realm).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = client(&server, bot()).check(REGISTRY).await.unwrap_err();
    assert!(matches!(err, RegistryError::Rejected { .. }), "{err:?}");
    assert_eq!(err.class(), "401 Unauthorized");
    assert!(!err.to_string().contains("s3cret"), "{err}");
    assert!(!format!("{err:?}").contains("s3cret"), "{err:?}");

    let limited = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&limited)
        .await;
    let err = client(&limited, bot()).check(REGISTRY).await.unwrap_err();
    assert_eq!(err.class(), "429 Too Many Requests");

    // Nothing listens: a network error.
    let gone =
        client(&limited, bot()).with_endpoint(REGISTRY, "http://127.0.0.1:9/".parse().unwrap());
    let err = gone.check(REGISTRY).await.unwrap_err();
    assert_eq!(err.class(), "network error", "{err}");
}

#[tokio::test]
async fn basic_auth_registries_are_checked_with_the_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .and(header("authorization", "Basic Ym90OnMzY3JldC10b2tlbg=="))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("www-authenticate", r#"Basic realm="r""#),
        )
        .mount(&server)
        .await;
    assert_eq!(
        client(&server, bot()).check(REGISTRY).await.unwrap(),
        RegistryCheck::Authenticated
    );
    let err = client(&server, Arc::new(Anonymous))
        .check(REGISTRY)
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::Rejected { .. }), "{err:?}");
}

#[tokio::test]
async fn forgetting_a_registry_drops_its_cached_tokens() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    mount_challenge(&server, &realm).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(token("t"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(found())
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(Anonymous));
    let image = image("registry.test/owner/app:v1");
    client.resolve(&image).await.unwrap();
    client.forget_registry("other.test");
    client.resolve(&image).await.unwrap(); // still cached
    client.forget_registry(REGISTRY);
    client.resolve(&image).await.unwrap(); // a fresh token
}

/// Live checks against public images; run with `TEST_HOSTER_REGISTRY_LIVE=1 cargo test`.
///
/// `TEST_HOSTER_REGISTRY_LIVE_IMAGE=ghcr.io/owner/private:tag` and
/// `TEST_HOSTER_REGISTRY_LIVE_CREDENTIAL=username:token` additionally resolve a private image with
/// that credential: the manual check of which ghcr token kinds work for pulls (ADR-0025).
mod live {
    use super::*;

    fn enabled() -> bool {
        let on = std::env::var("TEST_HOSTER_REGISTRY_LIVE").is_ok_and(|v| v == "1");
        if !on {
            eprintln!("skipped: set TEST_HOSTER_REGISTRY_LIVE=1 to run against live registries");
        }
        on
    }

    async fn resolve(reference: &str) -> PinnedImage {
        RegistryClient::new(Arc::new(Anonymous))
            .unwrap()
            .resolve(&image(reference))
            .await
            .unwrap_or_else(|e| panic!("{reference}: {e}"))
    }

    #[tokio::test]
    async fn public_ghcr_image() {
        if enabled() {
            let pinned = resolve("ghcr.io/home-assistant/home-assistant:stable").await;
            assert!(pinned.digest().as_ref().starts_with("sha256:"));
        }
    }

    #[tokio::test]
    async fn public_docker_hub_image() {
        if enabled() {
            let pinned = resolve("docker.io/library/alpine:latest").await;
            assert!(pinned.digest().as_ref().starts_with("sha256:"));
        }
    }

    #[tokio::test]
    async fn private_image_with_credential() {
        let (Ok(reference), Ok(credential)) = (
            std::env::var("TEST_HOSTER_REGISTRY_LIVE_IMAGE"),
            std::env::var("TEST_HOSTER_REGISTRY_LIVE_CREDENTIAL"),
        ) else {
            return;
        };
        if !enabled() {
            return;
        }
        let image = image(&reference);
        let credential: RegistryCredential = credential.parse().unwrap();
        let credentials = BTreeMap::from([(image.registry().to_owned(), credential)]);
        let pinned = RegistryClient::new(Arc::new(credentials))
            .unwrap()
            .resolve(&image)
            .await
            .unwrap_or_else(|e| panic!("{reference}: {e}"));
        eprintln!("{reference} -> {}", pinned.digest());
    }
}

#[tokio::test]
async fn contains_heads_the_manifest_by_digest() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/owner/app/manifests/{DIGEST}")))
        .respond_with(found())
        .expect(1)
        .mount(&server)
        .await;
    let foreign = format!("sha256:{}", "ab".repeat(32));
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/owner/app/manifests/{foreign}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = client(&server, Arc::new(Anonymous));
    let image = image("registry.test/owner/app");
    client
        .contains(&image, &DIGEST.parse().unwrap())
        .await
        .unwrap();
    let err = client
        .contains(&image, &foreign.parse().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::NotFound { .. }), "{err}");
    assert!(err.to_string().contains(&format!("app@{foreign}")), "{err}");
}
