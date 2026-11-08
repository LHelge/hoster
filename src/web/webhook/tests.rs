//! The deploy webhook: token verification against locally generated RSA keys and a mock JWKS
//! (wiremock), then the endpoint through the whole app against the fake orchestrator.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::Request;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http_body_util::BodyExt as _;
use ring::rand::SystemRandom;
use ring::rsa::PublicKeyComponents;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::oidc::{OidcVerifier, TokenError};
use crate::store::{DeploymentState, Provenance};
use crate::web::auth::tests::{JWKS_PATH, ORIGIN, TestApp, header};
use crate::web::stacks::tests::{Call, NOTES};

const KEY_A: &str = include_str!("testdata/key-a.pem");
const KEY_B: &str = include_str!("testdata/key-b.pem");

/// A digest as CI would send it.
fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An RSA signing key with the `kid` the JWKS publishes it under.
struct Signer {
    kid: &'static str,
    key: RsaKeyPair,
}

impl Signer {
    fn new(kid: &'static str, pem: &str) -> Self {
        let der: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        let key = RsaKeyPair::from_pkcs8(&STANDARD.decode(der).unwrap()).unwrap();
        Self { kid, key }
    }

    fn a() -> Self {
        Self::new("key-a", KEY_A)
    }

    fn b() -> Self {
        Self::new("key-b", KEY_B)
    }

    /// The public key as GitHub's JWKS lists it.
    fn jwk(&self) -> Value {
        let public = PublicKeyComponents::<Vec<u8>>::from(self.key.public());
        json!({
            "kty": "RSA",
            "alg": "RS256",
            "use": "sig",
            "kid": self.kid,
            "n": URL_SAFE_NO_PAD.encode(&public.n),
            "e": URL_SAFE_NO_PAD.encode(&public.e),
        })
    }

    /// A compact JWS of `header` and `claims`, signed with this key whatever `header` says.
    fn sign_raw(&self, header: &Value, claims: &Value) -> String {
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    /// An RS256 token for `claims`, as GitHub would send it.
    fn sign(&self, claims: &Value) -> String {
        let header = json!({ "typ": "JWT", "alg": "RS256", "x5t": "thumbprint", "kid": self.kid });
        self.sign_raw(&header, claims)
    }
}

/// docs/deploying-from-ci.md, whose example token claims these tests sign.
const GUIDE: &str = include_str!("../../../docs/deploying-from-ci.md");

/// The claims example in [`GUIDE`]: the code block after its `oidc-claims-example` marker. It
/// is shaped like GitHub's documented example token, for a tag push of `acme/notes`
/// (repository id 123456789) running its release workflow in the `production` environment.
fn documented_claims() -> Value {
    let (_, rest) = GUIDE
        .split_once("<!-- oidc-claims-example")
        .expect("the guide marks its claims example");
    let (_, rest) = rest.split_once("```json\n").expect("a json block follows");
    let (block, _) = rest.split_once("```").expect("the json block ends");
    serde_json::from_str(block).expect("the guide's claims example is JSON")
}

/// [`documented_claims`] as a token minted just now with id `jti`, for [`ORIGIN`].
fn claims(jti: &str) -> Value {
    let now = now();
    let mut claims = documented_claims();
    claims["jti"] = json!(jti);
    claims["aud"] = json!(ORIGIN);
    claims["nbf"] = json!(now - 5);
    claims["iat"] = json!(now - 5);
    claims["exp"] = json!(now + 300);
    claims
}

#[test]
fn the_guide_documents_a_github_token_for_the_default_audience() {
    let claims = documented_claims();
    assert_eq!(claims["iss"], "https://token.actions.githubusercontent.com");
    assert_eq!(claims["aud"], ORIGIN);
    for field in [
        "jti",
        "repository",
        "repository_id",
        "ref",
        "sha",
        "run_id",
        "actor",
        "environment",
        "workflow_ref",
        "job_workflow_ref",
    ] {
        assert!(claims[field].is_string(), "{field} is missing");
    }
    assert!(claims["exp"].as_u64() > claims["iat"].as_u64());
}

/// `claims` with `field` replaced.
fn with(mut claims: Value, field: &str, value: Value) -> Value {
    claims[field] = value;
    claims
}

async fn serve_jwks(server: &MockServer, signers: &[&Signer]) {
    let keys: Vec<Value> = signers.iter().map(|s| s.jwk()).collect();
    Mock::given(method("GET"))
        .and(path(JWKS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": keys })))
        .mount(server)
        .await;
}

fn verifier(server: &MockServer) -> OidcVerifier {
    let url = format!("{}{JWKS_PATH}", server.uri()).parse().unwrap();
    OidcVerifier::new(ORIGIN, url).unwrap()
}

async fn jwks_requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

// ---------------------------------------------------------------------------------------------
// Token verification
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_github_token_verifies_and_yields_its_claims() {
    let server = MockServer::start().await;
    let signer = Signer::a();
    serve_jwks(&server, &[&signer, &Signer::b()]).await;
    let claims = verifier(&server)
        .verify(&signer.sign(&claims("id-1")))
        .await
        .unwrap();
    assert_eq!(claims.jti, "id-1");
    assert_eq!(claims.repository, "acme/notes");
    assert_eq!(claims.repository_id, 123456789);
    assert_eq!(claims.git_ref, "refs/tags/v1.4.2");
    assert_eq!(claims.run_id, "123456");
    assert_eq!(claims.environment.as_deref(), Some("production"));
    assert_eq!(
        claims.job_workflow_ref.as_deref(),
        Some("acme/notes/.github/workflows/release.yml@refs/tags/v1.4.2")
    );
}

#[tokio::test]
async fn numeric_ids_and_audience_lists_are_accepted() {
    let server = MockServer::start().await;
    let signer = Signer::a();
    serve_jwks(&server, &[&signer]).await;
    let token = with(
        with(claims("id"), "repository_id", json!(123456789)),
        "aud",
        json!(["other", ORIGIN]),
    );
    let claims = verifier(&server)
        .verify(&signer.sign(&token))
        .await
        .unwrap();
    assert_eq!(claims.repository_id, 123456789);
}

#[tokio::test]
async fn only_rs256_is_accepted() {
    let server = MockServer::start().await;
    let signer = Signer::a();
    serve_jwks(&server, &[&signer]).await;
    let verifier = verifier(&server);
    for alg in ["none", "HS256", "RS512", "PS256", "rs256"] {
        let header = json!({ "alg": alg, "kid": signer.kid });
        let token = signer.sign_raw(&header, &claims("id"));
        assert_eq!(
            verifier.verify(&token).await.unwrap_err(),
            TokenError::Algorithm(alg.into())
        );
    }
    // An unsigned token (`alg: none` with an empty signature) is refused the same way.
    let unsigned = format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(json!({ "alg": "none" }).to_string()),
        URL_SAFE_NO_PAD.encode(claims("id").to_string())
    );
    assert!(matches!(
        verifier.verify(&unsigned).await.unwrap_err(),
        TokenError::Algorithm(_)
    ));
    // None of these needed a key.
    assert_eq!(jwks_requests(&server).await, 0);
}

#[tokio::test]
async fn bad_signatures_and_malformed_tokens_are_refused() {
    let server = MockServer::start().await;
    let a = Signer::a();
    serve_jwks(&server, &[&a]).await;
    let verifier = verifier(&server);
    // Signed by key B but naming key A.
    let forged = Signer {
        kid: "key-a",
        key: Signer::b().key,
    };
    assert_eq!(
        verifier
            .verify(&forged.sign(&claims("id")))
            .await
            .unwrap_err(),
        TokenError::Signature
    );
    // Claims changed after signing.
    let token = a.sign(&claims("id"));
    let mut parts: Vec<&str> = token.split('.').collect();
    let tampered_claims =
        URL_SAFE_NO_PAD.encode(with(claims("id"), "repository_id", json!("75")).to_string());
    parts[1] = &tampered_claims;
    assert_eq!(
        verifier.verify(&parts.join(".")).await.unwrap_err(),
        TokenError::Signature
    );
    for malformed in ["", "abc", "a.b", "a.b.c.d", "!!.??.**"] {
        assert!(
            matches!(
                verifier.verify(malformed).await.unwrap_err(),
                TokenError::Malformed(_)
            ),
            "{malformed}"
        );
    }
}

#[tokio::test]
async fn issuer_audience_and_lifetime_are_checked_with_skew() {
    let server = MockServer::start().await;
    let signer = Signer::a();
    serve_jwks(&server, &[&signer]).await;
    let verifier = verifier(&server);
    let now = now();
    let verify = |claims: Value| {
        let token = signer.sign(&claims);
        let verifier = &verifier;
        async move { verifier.verify(&token).await }
    };
    assert!(matches!(
        verify(with(claims("id"), "iss", json!("https://evil.example"))).await,
        Err(TokenError::Issuer(_))
    ));
    assert!(matches!(
        verify(with(claims("id"), "aud", json!("https://github.com/acme"))).await,
        Err(TokenError::Audience(_))
    ));
    assert_eq!(
        verify(with(claims("id"), "exp", json!(now - 120))).await,
        Err(TokenError::Expired)
    );
    assert_eq!(
        verify(with(claims("id"), "nbf", json!(now + 120))).await,
        Err(TokenError::NotYetValid)
    );
    assert_eq!(
        verify(with(claims("id"), "iat", json!(now + 120))).await,
        Err(TokenError::IssuedInFuture)
    );
    // Within the 60 s skew.
    verify(with(claims("id"), "exp", json!(now - 30)))
        .await
        .unwrap();
    verify(with(claims("id"), "nbf", json!(now + 30)))
        .await
        .unwrap();
    // An `exp` no clock can hold is refused, not a panic.
    assert!(matches!(
        verify(with(claims("id"), "exp", json!(u64::MAX))).await,
        Err(TokenError::Claims(_))
    ));
    // A token without the claims hoster needs.
    let mut incomplete = claims("id");
    incomplete.as_object_mut().unwrap().remove("repository_id");
    assert!(matches!(
        verify(incomplete).await,
        Err(TokenError::Claims(_))
    ));
}

#[tokio::test]
async fn unknown_keys_refetch_the_key_set_at_most_once_a_minute() {
    let server = MockServer::start().await;
    let a = Signer::a();
    serve_jwks(&server, &[&a]).await;
    let verifier = verifier(&server);
    verifier.verify(&a.sign(&claims("1"))).await.unwrap();
    verifier.verify(&a.sign(&claims("2"))).await.unwrap();
    assert_eq!(jwks_requests(&server).await, 1, "the key set is cached");
    // Unknown `kid`s within the minute do not hammer GitHub.
    let b = Signer::b();
    for _ in 0..3 {
        assert_eq!(
            verifier.verify(&b.sign(&claims("3"))).await.unwrap_err(),
            TokenError::UnknownKey("key-b".into())
        );
    }
    assert_eq!(jwks_requests(&server).await, 1);
}

#[tokio::test]
async fn an_unknown_key_refreshes_the_key_set() {
    let server = MockServer::start().await;
    let (a, b) = (Signer::a(), Signer::b());
    // GitHub rotates: first only key A, then both.
    Mock::given(method("GET"))
        .and(path(JWKS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": [a.jwk()] })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    serve_jwks(&server, &[&a, &b]).await;
    let verifier = verifier(&server).with_refresh_interval(Duration::ZERO);
    verifier.verify(&a.sign(&claims("1"))).await.unwrap();
    verifier.verify(&b.sign(&claims("2"))).await.unwrap();
    assert_eq!(jwks_requests(&server).await, 2);
}

#[tokio::test]
async fn an_unreachable_key_set_rejects_never_accepts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(JWKS_PATH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = verifier(&server)
        .verify(&Signer::a().sign(&claims("1")))
        .await
        .unwrap_err();
    assert!(matches!(err, TokenError::Unavailable(_)), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The endpoint, through the app
// ---------------------------------------------------------------------------------------------

/// [`NOTES`] with a `[deploy]` rule for `acme/notes` (repository id 123456789), tags only.
fn hooked() -> String {
    format!("{NOTES}\n[deploy]\nrepository_id = 123456789\nrefs = [\"refs/tags/v*\"]\n")
}

/// The app with `notes` deployable by the webhook and GitHub's key set served.
async fn app() -> (TestApp, Signer) {
    let app = TestApp::new().await;
    app.stacks.stacks.lock().unwrap()[0].revision.toml = hooked();
    let signer = Signer::a();
    serve_jwks(&app.github, &[&signer]).await;
    (app, signer)
}

fn hook(token: Option<&str>, body: impl Into<Body>) -> Request<Body> {
    let mut request = Request::post("/hooks/deploy").header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(body.into()).unwrap()
}

/// The body of the CI snippet in spec.md §5.2:
/// `jq -nc --arg d "$digest" --arg t "$ref_name" '{stack:"notes", images:{api:$d}, tag:$t}'`.
fn ci_body(d: &str, t: &str) -> String {
    json!({ "stack": "notes", "images": { "api": d }, "tag": t }).to_string()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn the_spec_ci_snippet_deploys_with_provenance_from_the_token() {
    let (app, signer) = app().await;
    let token = signer.sign(&claims("run-1"));
    let response = app
        .send(hook(Some(&token), ci_body(&digest(1), "v1.4.2")))
        .await;
    assert_eq!(response.status(), 202);
    assert_eq!(header(&response, "cache-control"), "no-store");
    let body = json_body(response).await;
    assert_eq!(body["result"], "accepted");
    assert_eq!(body["deployment_id"], "8");
    let calls = app.stacks.calls();
    let [Call::Webhook(request)] = calls.as_slice() else {
        panic!("expected one webhook deploy, got {calls:?}");
    };
    assert_eq!(request.stack, "notes");
    assert_eq!(request.images["api"], digest(1));
    assert_eq!(request.caller.repository_id, 123456789);
    assert_eq!(request.caller.environment.as_deref(), Some("production"));
    assert_eq!(
        request.provenance,
        Provenance {
            repository: "acme/notes".into(),
            sha: "0f3c5e1a9d2b7c4e8f6a1b3d5c7e9f0a2b4c6d8e".into(),
            git_ref: "refs/tags/v1.4.2".into(),
            run_id: "123456".into(),
            actor: "octocat".into(),
            tag: Some("v1.4.2".into()),
        }
    );
}

#[tokio::test]
async fn an_unchanged_target_answers_noop() {
    let (app, signer) = app().await;
    *app.stacks.deploy_state.lock().unwrap() = DeploymentState::Noop;
    let token = signer.sign(&claims("run-1"));
    // `github.ref_name` of a branch is no OCI tag; the informational tag is taken as it is.
    let response = app
        .send(hook(Some(&token), ci_body(&digest(1), "feature/login")))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["result"], "noop");
}

#[tokio::test]
async fn bad_tokens_get_401_without_detail_and_the_body_is_not_parsed() {
    let (app, signer) = app().await;
    let expired = signer.sign(&with(claims("x"), "exp", json!(now() - 600)));
    let wrong_audience = signer.sign(&with(claims("y"), "aud", json!("https://other")));
    for token in [None, Some("garbage"), Some(&expired), Some(&wrong_audience)] {
        // Not JSON at all: a 400 would mean the body was looked at.
        let response = app.send(hook(token, "{not json")).await;
        assert_eq!(response.status(), 401, "{token:?}");
        assert_eq!(header(&response, "www-authenticate"), "Bearer");
        assert_eq!(
            json_body(response).await,
            json!({ "error": "invalid token" })
        );
    }
    assert!(app.stacks.calls().is_empty());
    assert!(app.stacks.rejections.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_token_is_single_use() {
    let (app, signer) = app().await;
    let token = signer.sign(&claims("once"));
    let first = app
        .send(hook(Some(&token), ci_body(&digest(1), "v1")))
        .await;
    assert_eq!(first.status(), 202);
    let replay = app
        .send(hook(Some(&token), ci_body(&digest(2), "v1")))
        .await;
    assert_eq!(replay.status(), 401);
    assert_eq!(app.stacks.calls().len(), 1);
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let (app, signer) = app().await;
    let token = signer.sign(&claims("big"));
    let padding = "x".repeat(super::Webhook::MAX_BODY);
    let body = json!({ "stack": "notes", "images": { "api": digest(1) }, "tag": padding });
    let response = app.send(hook(Some(&token), body.to_string())).await;
    assert_eq!(response.status(), 413);
    assert!(app.stacks.calls().is_empty());
}

#[tokio::test]
async fn payload_problems_are_4xx() {
    let (app, signer) = app().await;
    let cases = [
        ("{not json".to_owned(), 400),
        (json!({ "stack": "notes" }).to_string(), 400),
        (
            json!({ "stack": "notes", "images": { "api": digest(1) }, "timestamp": 1 }).to_string(),
            400,
        ),
        (
            json!({ "stack": "notes", "images": { "api": digest(1) }, "stack_file": "x" })
                .to_string(),
            422,
        ),
        (json!({ "stack": "notes", "images": {} }).to_string(), 422),
        (
            json!({ "stack": "notes", "images": { "api": "latest" } }).to_string(),
            422,
        ),
        (
            json!({ "stack": "Not A Name", "images": { "api": digest(1) } }).to_string(),
            422,
        ),
        (
            json!({ "stack": "notes", "images": { "api": digest(1) }, "tag": "a\u{7}b" })
                .to_string(),
            422,
        ),
        (
            json!({ "stack": "notes", "images": { "api": digest(1) }, "tag": "" }).to_string(),
            422,
        ),
        // Unknown and not CI-built services, from the orchestrator.
        (ci_body(&digest(1), "v1").replace("\"api\"", "\"db\""), 422),
        (
            ci_body(&digest(1), "v1").replace("\"api\"", "\"storage\""),
            422,
        ),
    ];
    for (i, (body, status)) in cases.into_iter().enumerate() {
        let token = signer.sign(&claims(&format!("payload-{i}")));
        let response = app.send(hook(Some(&token), body.clone())).await;
        assert_eq!(response.status(), status, "{body}");
        assert!(json_body(response).await["error"].is_string(), "{body}");
    }
}

#[tokio::test]
async fn several_services_go_in_one_request() {
    let (app, signer) = app().await;
    let token = signer.sign(&claims("multi"));
    let body = json!({ "stack": "notes", "images": { "api": digest(1), "storage": digest(2) } });
    // storage is not CI-built, so the whole request is refused; nothing deploys half.
    let response = app.send(hook(Some(&token), body.to_string())).await;
    assert_eq!(response.status(), 422);
    let calls = app.stacks.calls();
    let [Call::Webhook(request)] = calls.as_slice() else {
        panic!("{calls:?}");
    };
    assert_eq!(request.images.len(), 2);
}

#[tokio::test]
async fn a_foreign_digest_is_refused() {
    let (app, signer) = app().await;
    app.stacks.foreign.lock().unwrap().push(digest(9));
    let token = signer.sign(&claims("foreign"));
    let response = app
        .send(hook(Some(&token), ci_body(&digest(9), "v1")))
        .await;
    assert_eq!(response.status(), 422);
    let body = json_body(response).await;
    assert_eq!(
        body["error"],
        "the digest for service api is not in its repository"
    );
}

#[tokio::test]
async fn rule_mismatches_get_403_and_show_on_the_stack_page() {
    let (app, signer) = app().await;
    // A branch push, but the rule allows tags only.
    let token = signer.sign(&with(claims("branch"), "ref", json!("refs/heads/main")));
    let response = app
        .send(hook(Some(&token), ci_body(&digest(1), "main")))
        .await;
    assert_eq!(response.status(), 403);
    assert_eq!(
        json_body(response).await,
        json!({ "error": "not allowed to deploy this stack" })
    );
    // Another repository entirely, deploying through a reusable workflow.
    let token = signer.sign(&with(
        with(claims("other"), "repository_id", json!("99")),
        "job_workflow_ref",
        json!("acme/shared/.github/workflows/deploy.yml@refs/heads/main"),
    ));
    let response = app
        .send(hook(Some(&token), ci_body(&digest(1), "v1")))
        .await;
    assert_eq!(response.status(), 403);

    let rejections = app.stacks.rejections.lock().unwrap().clone();
    assert_eq!(rejections.len(), 2);
    assert_eq!(rejections[0].git_ref, "refs/heads/main");
    assert!(
        rejections[0].reason.contains("refs/heads/main"),
        "{rejections:?}"
    );
    assert_eq!(rejections[1].repository_id, "99");
    assert_eq!(rejections[1].environment.as_deref(), Some("production"));

    let browser = app.sign_in().await;
    let page = app.get("/stacks/notes", Some(&browser.cookie)).await;
    assert_eq!(page.status(), 200);
    let html = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Refused webhook deploys"), "{html}");
    assert!(html.contains("<code>99</code>"), "{html}");
    assert!(html.contains("acme/notes"), "{html}");
    // Both workflow claims, so the admin can copy the one a `[deploy]` rule names.
    assert!(
        html.contains("acme/notes/.github/workflows/release.yml@refs/tags/v1.4.2"),
        "{html}"
    );
    assert!(
        html.contains("acme/shared/.github/workflows/deploy.yml@refs/heads/main"),
        "{html}"
    );
    // The token itself is never shown.
    assert!(!html.contains(&token), "{html}");
}

#[tokio::test]
async fn a_stack_without_a_deploy_rule_or_an_unknown_stack_is_403() {
    let app = TestApp::new().await;
    let signer = Signer::a();
    serve_jwks(&app.github, &[&signer]).await;
    // `notes` as the fake has it: no `[deploy]`.
    let token = signer.sign(&claims("no-rule"));
    let response = app
        .send(hook(Some(&token), ci_body(&digest(1), "v1")))
        .await;
    assert_eq!(response.status(), 403);
    let token = signer.sign(&claims("ghost"));
    let body = ci_body(&digest(1), "v1").replace("\"notes\"", "\"ghost\"");
    let unknown = app.send(hook(Some(&token), body)).await;
    assert_eq!(unknown.status(), 403);
    // Both answers read the same.
    assert_eq!(
        json_body(response).await,
        json!({ "error": "not allowed to deploy this stack" })
    );
    assert_eq!(
        json_body(unknown).await,
        json!({ "error": "not allowed to deploy this stack" })
    );
}

#[tokio::test]
async fn an_unreachable_key_set_answers_503() {
    let app = TestApp::new().await;
    let token = Signer::a().sign(&claims("down"));
    // The mock GitHub serves no key set.
    let response = app
        .send(hook(Some(&token), ci_body(&digest(1), "v1")))
        .await;
    assert_eq!(response.status(), 503);
    assert!(app.stacks.calls().is_empty());
}
