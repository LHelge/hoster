//! Settings through the whole app: Registries against a fake credential store and registry
//! client, Notifications against the real store and secret store with a fake mail provider.
//! Tokens and API keys must never come back in any response.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, header};
use reqwest::StatusCode as HttpStatus;

use super::*;
use crate::store::Timestamp;
use crate::web::auth::tests::{ORIGIN, TestApp};
use crate::web::stacks::tests::{FORM, body, encoded, post};

const TOKEN: &str = "ghp_T0ken-Never-Shown";

/// Credentials in memory, and a canned answer for every test.
#[derive(Default)]
pub struct FakeRegistries {
    pub stored: Mutex<Vec<(RegistryHost, RegistryCredential)>>,
    /// What `check` answers for each host; anything else is a network error.
    pub checks: Mutex<Vec<(String, Result<RegistryCheck, u16>)>>,
}

#[async_trait::async_trait]
impl RegistryControl for FakeRegistries {
    async fn list(&self) -> Result<Vec<RegistryEntry>, CredentialError> {
        Ok(self
            .stored
            .lock()
            .unwrap()
            .iter()
            .map(|(host, credential)| RegistryEntry {
                host: host.clone(),
                username: credential.username().to_owned(),
                updated_at: Timestamp::now(),
            })
            .collect())
    }

    async fn set(
        &self,
        host: &RegistryHost,
        credential: &RegistryCredential,
        _by: &Operator,
    ) -> Result<(), CredentialError> {
        let mut stored = self.stored.lock().unwrap();
        stored.retain(|(h, _)| h != host);
        stored.push((host.clone(), credential.clone()));
        Ok(())
    }

    async fn remove(&self, host: &RegistryHost, _by: &Operator) -> Result<bool, CredentialError> {
        let mut stored = self.stored.lock().unwrap();
        let before = stored.len();
        stored.retain(|(h, _)| h != host);
        Ok(stored.len() != before)
    }

    async fn check(&self, host: &RegistryHost) -> Result<RegistryCheck, RegistryError> {
        let answer = self
            .checks
            .lock()
            .unwrap()
            .iter()
            .find(|(h, _)| h == host.as_ref())
            .map(|(_, a)| *a);
        let registry = host.to_string();
        match answer {
            Some(Ok(check)) => Ok(check),
            Some(Err(429)) => Err(RegistryError::RateLimited {
                registry,
                retry_after: None,
            }),
            Some(Err(code)) => Err(RegistryError::Rejected {
                registry,
                status: HttpStatus::from_u16(code).unwrap(),
            }),
            None => Err(RegistryError::Challenge {
                registry,
                reason: "no answer".into(),
            }),
        }
    }
}

/// The body of `response`, asserted to be free of the token.
async fn clean(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let html = body(response).await;
    assert!(!html.contains(TOKEN), "the token leaked: {html}");
    (status, html)
}

fn ghcr() -> RegistryHost {
    "ghcr.io".parse().unwrap()
}

#[tokio::test]
async fn the_page_lists_hosts_and_usernames_but_no_tokens() {
    let app = TestApp::new().await;
    app.registries
        .stored
        .lock()
        .unwrap()
        .push((ghcr(), RegistryCredential::new("machine-bot", TOKEN)));
    let browser = app.sign_in().await;
    let (status, html) = clean(app.get("/settings/registries", Some(&browser.cookie)).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(&format!(
            r#"<a href="/settings/general" aria-current="page">{}Settings</a>"#,
            crate::web::Icon::Settings
        )),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<li><a href="/settings/registries" aria-current="page">registries<span class="settings-note">1</span></a></li>"#
        ),
        "{html}"
    );
    assert!(html.contains("~/settings/registries"), "{html}");
    assert!(
        html.contains(r#"<code class="field-name registry-host">ghcr.io</code>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="registry-user">machine-bot</span>"#),
        "{html}"
    );
    assert!(html.contains(r#"type="password" name="token""#), "{html}");
}

#[tokio::test]
async fn saving_stores_the_credential_without_echoing_the_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post(
            "/settings/registries",
            &browser,
            FORM,
            encoded(&[("host", " GHCR.io "), ("username", "bot"), ("token", TOKEN)]),
        ))
        .await;
    let (status, html) = clean(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Saved the credential for ghcr.io"), "{html}");
    assert!(html.contains("nothing was redeployed"), "{html}");
    let stored = app.registries.stored.lock().unwrap().clone();
    assert_eq!(stored, [(ghcr(), RegistryCredential::new("bot", TOKEN))]);
    // Registry credentials are hoster's own: the orchestrator is not involved.
    assert!(app.stacks.calls().is_empty());

    for fields in [
        [("host", "ghcr.io/x"), ("username", "bot"), ("token", TOKEN)],
        [("host", "ghcr.io"), ("username", ""), ("token", TOKEN)],
        [("host", "ghcr.io"), ("username", "bot"), ("token", " ")],
    ] {
        let response = app
            .send(post(
                "/settings/registries",
                &browser,
                FORM,
                encoded(&fields),
            ))
            .await;
        let (status, _) = clean(response).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{fields:?}");
    }
}

#[tokio::test]
async fn deleting_a_credential() {
    let app = TestApp::new().await;
    app.registries
        .stored
        .lock()
        .unwrap()
        .push((ghcr(), RegistryCredential::new("bot", TOKEN)));
    let browser = app.sign_in().await;
    let delete = || {
        post(
            "/settings/registries/delete",
            &browser,
            FORM,
            encoded(&[("host", "ghcr.io")]),
        )
    };
    let (status, html) = clean(app.send(delete()).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("accessed anonymously from now on"), "{html}");
    assert!(app.registries.stored.lock().unwrap().is_empty());
    let (status, _) = clean(app.send(delete()).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn testing_reports_success_or_the_error_class_never_the_token() {
    let app = TestApp::new().await;
    app.registries
        .stored
        .lock()
        .unwrap()
        .push((ghcr(), RegistryCredential::new("bot", TOKEN)));
    *app.registries.checks.lock().unwrap() = vec![
        ("ghcr.io".into(), Ok(RegistryCheck::Authenticated)),
        ("docker.io".into(), Err(429)),
        ("quay.io".into(), Err(401)),
    ];
    let browser = app.sign_in().await;
    let test = |host: &str| {
        post(
            "/settings/registries/test",
            &browser,
            FORM,
            encoded(&[("host", host)]),
        )
    };
    let (status, html) = clean(app.send(test("ghcr.io")).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("flash-success"), "{html}");
    assert!(
        html.contains("with the stored credential succeeded"),
        "{html}"
    );

    let (status, html) = clean(app.send(test("docker.io")).await).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(html.contains("(429 Too Many Requests)"), "{html}");

    let (_, html) = clean(app.send(test("quay.io")).await).await;
    assert!(html.contains("(401 Unauthorized)"), "{html}");
    assert!(html.contains("check the username and token"), "{html}");

    let (_, html) = clean(app.send(test("registry.example")).await).await;
    assert!(html.contains("(unusable registry response)"), "{html}");

    let (status, _) = clean(app.send(test("not a host")).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn registry_posts_need_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    for uri in [
        "/settings/registries",
        "/settings/registries/delete",
        "/settings/registries/test",
    ] {
        let request = Request::post(uri)
            .header(header::COOKIE, &browser.cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, FORM)
            .body(Body::from(encoded(&[
                ("host", "ghcr.io"),
                ("username", "bot"),
                ("token", TOKEN),
            ])))
            .unwrap();
        let (status, _) = clean(app.send(request).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    assert!(app.registries.stored.lock().unwrap().is_empty());
}

#[test]
fn the_page_renders_without_tokens() {
    let page = RegistriesPage {
        layout: Layout::new(
            Section::Settings,
            &crate::web::auth::Session::for_tests("octocat", "c0ffee"),
        ),
        nav: SettingsNav {
            current: SettingsPart::Registries,
            registries: Some(1),
        },
        entries: vec![EntryView {
            host: "ghcr.io".into(),
            username: "bot".into(),
            changed_at: "2026-11-08T00:00:00Z".into(),
        }],
    }
    .render()
    .unwrap();
    assert!(
        page.contains(r#"<span class="registry-user">bot</span>"#),
        "{page}"
    );
    // The only token field is the empty input of the form.
    assert_eq!(page.matches(r#"name="token""#).count(), 1, "{page}");
    assert!(!page.contains("value=\"ghp_"), "{page}");
}

#[tokio::test]
async fn settings_leads_to_its_first_section() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app.get("/settings", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/settings/general");
    let response = app.get("/settings/users", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Every Settings section and the slug of its URL.
const SECTIONS: [&str; 6] = [
    "general",
    "notifications",
    "registries",
    "webhook",
    "proxy",
    "certificates",
];

#[tokio::test]
async fn every_section_has_the_grouped_sub_nav_with_settings_current() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    for slug in SECTIONS {
        let response = app
            .get(&format!("/settings/{slug}"), Some(&browser.cookie))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{slug}");
        let html = body(response).await;
        assert!(
            html.contains(&format!(
                r#"<a href="/settings/general" aria-current="page">{}Settings</a>"#,
                crate::web::Icon::Settings
            )),
            "{slug}: {html}"
        );
        assert!(html.contains(&format!("~/settings/{slug}")), "{slug}");
        assert!(
            html.contains(r#"<nav class="settings-nav" aria-label="Settings sections">"#),
            "{slug}"
        );
        for group in ["instance", "deploy", "network"] {
            assert!(
                html.contains(&format!(r#"id="settings-group-{group}">{group}</span>"#)),
                "{slug}: {group}"
            );
        }
        assert_eq!(
            html.matches(r#"aria-current="page""#).count(),
            2,
            "the main nav and the sub-nav each mark one link: {slug}"
        );
        assert!(
            html.contains(&format!(
                r#"<a href="/settings/{slug}" aria-current="page">"#
            )),
            "{slug}"
        );
        assert!(!html.contains("style="), "{slug}: no inline styles");
        let configured = !matches!(slug, "registries" | "notifications");
        assert_eq!(
            html.contains("restart hoster to apply"),
            configured,
            "{slug}: the footer is on the configuration sections"
        );
        if configured && slug != "general" {
            assert!(!html.contains("<form"), "{slug}: read-only");
            assert!(!html.contains("<input"), "{slug}: read-only");
        }
    }
}

#[tokio::test]
async fn the_read_only_sections_show_the_effective_config() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let page = |slug: &'static str| {
        let app = &app;
        let cookie = browser.cookie.clone();
        async move { body(app.get(&format!("/settings/{slug}"), Some(&cookie)).await).await }
    };

    let general = page("general").await;
    for expected in [
        "<code>admin.example.com</code>",
        "<code>@octocat</code>",
        "set by <code>HOSTER_ADMIN_GITHUB</code>",
        "<code>/home/hoster/.local/share/hoster</code>",
        "<code>/home/hoster/.config/hoster/hoster.toml</code>",
        "<code>/home/hoster/.config/hoster/github-secret</code>",
        "systemd credential github_client_id",
    ] {
        assert!(general.contains(expected), "{expected}: {general}");
    }

    let webhook = page("webhook").await;
    for expected in [
        "<code>https://admin.example.com/hooks/deploy</code>",
        "<code>https://token.actions.githubusercontent.com</code>",
        r#"<a href="https://docs.hoster.lhelge.se/wiki/deploying-from-ci/">"#,
    ] {
        assert!(webhook.contains(expected), "{expected}: {webhook}");
    }

    let proxy = page("proxy").await;
    for expected in [
        "<code>0.0.0.0:80</code>",
        "<code>[::]:443</code>",
        "<code>20000-29999</code>",
        "<code>421</code>",
        "set by <code>HOSTER_PROXY__TIMEOUTS__READ</code>",
    ] {
        assert!(proxy.contains(expected), "{expected}: {proxy}");
    }

    let acme = page("certificates").await;
    for expected in [
        r#"<li class="choice-card choice-on">"#,
        "acme-staging-v02.api.letsencrypt.org",
        "<code>certs@example.com</code>",
        "<code>/home/hoster/.local/share/hoster/acme/account.json</code>",
    ] {
        assert!(acme.contains(expected), "{expected}: {acme}");
    }
    assert_eq!(acme.matches("choice-on").count(), 1, "{acme}");
}

#[tokio::test]
async fn no_section_renders_a_secret_value() {
    let dir = tempfile::TempDir::new().unwrap();
    let secret_file = dir.path().join("github-secret");
    let key_file = dir.path().join("secret.key");
    const CLIENT_SECRET: &str = "gh-oauth-client-secret-never-shown";
    const STORE_KEY: &str = "c2VjcmV0LXN0b3JlLWtleS1uZXZlci1zaG93bg";
    std::fs::write(&secret_file, CLIENT_SECRET).unwrap();
    std::fs::write(&key_file, STORE_KEY).unwrap();
    std::fs::create_dir_all(dir.path().join("state/acme")).unwrap();
    std::fs::write(
        dir.path().join("state/acme/account.json"),
        r#"{"key_pkcs8":"account-key-never-shown"}"#,
    )
    .unwrap();

    let mut app = TestApp::new().await;
    let config = EffectiveConfig::sample().with(|config| {
        config.state_dir = dir.path().join("state");
        config.secret_key_file = key_file.clone();
        config.web.github_client_secret_file = Some(secret_file.clone());
        config.credentials = crate::config::Credentials::in_dir(dir.path());
    });
    app.app.settings = Settings::new(
        config,
        app.certificates.clone(),
        app.registries.clone(),
        app.instance.clone(),
        app.notifications.clone(),
    );
    app.registries
        .stored
        .lock()
        .unwrap()
        .push((ghcr(), RegistryCredential::new("bot", TOKEN)));
    let browser = app.sign_in().await;
    for slug in SECTIONS {
        let (status, html) = clean(
            app.get(&format!("/settings/{slug}"), Some(&browser.cookie))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{slug}");
        for secret in [
            CLIENT_SECRET,
            STORE_KEY,
            "account-key-never-shown",
            // The OAuth App's secret the test app signs in with.
            "test-secret",
        ] {
            assert!(!html.contains(secret), "{slug} leaked {secret}: {html}");
        }
    }
}

#[tokio::test]
async fn general_edits_the_instance_name_and_keeps_the_rest_read_only() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/settings/general", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<label class="field-name" for="instance-name">Instance name</label>"#),
        "{html}"
    );
    assert!(
        html.contains("Shown in the sidebar and in notifications."),
        "{html}"
    );
    assert!(
        html.contains(r#"name="name" value="" placeholder="srv-01" maxlength="64""#),
        "{html}"
    );
    assert!(
        !html.contains("Reset to default"),
        "nothing to reset: {html}"
    );
    // The only form and input on General are the instance name's.
    assert_eq!(html.matches("<form").count(), 1, "{html}");
    assert_eq!(html.matches("<input").count(), 1, "{html}");
    // The footer no longer claims the editable field is set in hoster.toml.
    assert!(
        html.contains(
            "The instance name is saved here and applies at once. Everything else here is set in <code>hoster.toml</code>"
        ),
        "{html}"
    );
    // The sidebar shows the default, the hostname.
    assert!(html.contains(" · srv-01</span>"), "{html}");
}

#[tokio::test]
async fn saving_the_instance_name_shows_it_in_the_sidebar_on_the_next_page() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let save = |name: &str| {
        post(
            "/settings/general/name",
            &browser,
            FORM,
            encoded(&[("name", name)]),
        )
    };
    let response = app.send(save("  prod <eu> & \"main\" ")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("flash-success"), "{html}");
    assert!(
        html.contains("The instance name is now prod &#60;eu&#62; &#38; &#34;main&#34;."),
        "{html}"
    );
    // The field is swapped out of band to show what is stored, now with "Reset".
    assert!(
        html.contains(
            r#"<div id="name-inline" class="field-inline" hx-swap-oob="outerHTML"><input id="instance-name" type="text" name="name" value="prod &#60;eu&#62; &#38; &#34;main&#34;" placeholder="srv-01""#
        ),
        "{html}"
    );
    assert!(html.contains("Reset to default"), "{html}");
    assert_eq!(
        app.store
            .instance_name()
            .await
            .unwrap()
            .map(|n| n.to_string())
            .as_deref(),
        Some(r#"prod <eu> & "main""#)
    );

    // Every page's sidebar shows it, escaped.
    for uri in ["/", "/stacks", "/settings/general"] {
        let html = body(app.get(uri, Some(&browser.cookie)).await).await;
        assert!(
            html.contains(" · prod &#60;eu&#62; &#38; &#34;main&#34;</span>"),
            "{uri}: {html}"
        );
        assert!(!html.contains("prod <eu>"), "{uri}: unescaped");
    }
    let html = body(app.get("/settings/general", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"value="prod &#60;eu&#62; &#38; &#34;main&#34;" placeholder="srv-01""#),
        "{html}"
    );
    assert!(html.contains("Reset to default"), "{html}");

    // Invalid names are refused and change nothing.
    for bad in ["a\nb", &"x".repeat(65)] {
        let response = app.send(save(bad)).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad:?}"
        );
        let html = body(response).await;
        assert!(html.contains("flash-error"), "{html}");
        assert!(html.contains("The instance name"), "{html}");
    }
    assert_eq!(app.instance.name().to_string(), r#"prod <eu> & "main""#);

    // An empty name resets to the default rather than storing it.
    let response = app.send(save("   ")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("The instance name is the default again: the hostname, srv-01."),
        "{html}"
    );
    assert_eq!(app.store.instance_name().await.unwrap(), None);
    let html = body(app.get("/", Some(&browser.cookie)).await).await;
    assert!(html.contains(" · srv-01</span>"), "{html}");
}

#[tokio::test]
async fn resetting_the_instance_name() {
    let app = TestApp::new().await;
    app.instance.set(&"prod".parse().unwrap()).await.unwrap();
    let browser = app.sign_in().await;
    let response = app
        .send(post(
            "/settings/general/name/reset",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("the hostname, srv-01"), "{html}");
    // The field shows the default again, without "Reset".
    assert!(
        html.contains(r#"hx-swap-oob="outerHTML"><input id="instance-name" type="text" name="name" value="" placeholder="srv-01""#),
        "{html}"
    );
    assert!(!html.contains("Reset to default"), "{html}");
    assert_eq!(app.store.instance_name().await.unwrap(), None);
    assert_eq!(app.instance.name().to_string(), "srv-01");
}

#[tokio::test]
async fn instance_name_posts_need_the_csrf_token() {
    let app = TestApp::new().await;
    app.instance.set(&"prod".parse().unwrap()).await.unwrap();
    let browser = app.sign_in().await;
    for uri in ["/settings/general/name", "/settings/general/name/reset"] {
        let request = Request::post(uri)
            .header(header::COOKIE, &browser.cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, FORM)
            .body(Body::from(encoded(&[("name", "evil")])))
            .unwrap();
        assert_eq!(
            app.send(request).await.status(),
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    assert_eq!(app.instance.name().to_string(), "prod");
}

// ---- Settings → Notifications ----------------------------------------------------------------

use std::collections::VecDeque;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;

use self::notifications::Explained;
use crate::notify::{Delivery, DeliveryError, Message, Provider, ProviderKind};
use crate::store::{DeliveryOutcome, NotificationSettings};

const API_KEY: &str = "re_ApiKey-Never-Shown";

/// The mail provider the test mail goes through: it keeps what it was sent and answers from a
/// script, then `Ok` with the id `fake-<n>`.
#[derive(Debug, Default)]
pub struct FakeProvider {
    pub sent: Mutex<Vec<Message>>,
    pub answers: Mutex<VecDeque<Result<Delivery, DeliveryError>>>,
}

impl Provider for FakeProvider {
    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<Delivery, DeliveryError>> {
        let mut sent = self.sent.lock().unwrap();
        sent.push(message.clone());
        let n = sent.len();
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(Delivery {
                id: Some(format!("fake-{n}")),
            }));
        async move { answer }.boxed()
    }
}

/// Builds the [`FakeProvider`] whatever key is stored; the page checks for a key first.
#[derive(Debug, Default)]
pub struct FakeMailer(pub Arc<FakeProvider>);

impl crate::notify::Mailer for FakeMailer {
    fn provider(
        &self,
        _kind: ProviderKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Provider>, DeliveryError>> {
        let provider: Arc<dyn Provider> = self.0.clone();
        async move { Ok(provider) }.boxed()
    }
}

impl FakeMailer {
    fn sent(&self) -> Vec<Message> {
        self.0.sent.lock().unwrap().clone()
    }

    fn answer(&self, answer: Result<Delivery, DeliveryError>) {
        self.0.answers.lock().unwrap().push_back(answer);
    }
}

/// The body of `response`, asserted to be free of the API key.
async fn keyless(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let html = body(response).await;
    assert!(!html.contains(API_KEY), "the API key leaked: {html}");
    (status, html)
}

/// A complete, enabled configuration with the key.
const CONFIGURED: [(&str, &str); 5] = [
    ("provider", "resend"),
    ("api_key", API_KEY),
    ("from", "hoster@example.com"),
    ("recipient", "ops@example.com"),
    ("enabled", "on"),
];

async fn stored_key(app: &TestApp) -> Option<String> {
    app.secrets
        .reveal(ProviderKind::Resend.secret_name().as_ref())
        .await
        .unwrap()
        .map(|(value, _)| value.expose().to_owned())
}

/// The messages of the recorded events, newest first.
async fn event_messages(app: &TestApp) -> Vec<String> {
    let page = app
        .store
        .event_page(&crate::store::EventQuery {
            limit: 50,
            ..crate::store::EventQuery::default()
        })
        .await
        .unwrap();
    page.events.into_iter().map(|event| event.message).collect()
}

fn rejected(status: u16, code: &str, message: &str) -> DeliveryError {
    DeliveryError::Rejected {
        provider: ProviderKind::Resend,
        status: HttpStatus::from_u16(status).unwrap(),
        code: Some(code.into()),
        message: Some(message.into()),
    }
}

#[tokio::test]
async fn notifications_unconfigured_shows_the_empty_form_and_status() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .get("/settings/notifications", Some(&browser.cookie))
        .await;
    let (status, html) = keyless(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(
            r#"<li><a href="/settings/notifications" aria-current="page">notifications</a></li>"#
        ),
        "{html}"
    );
    // In the instance group, after general and before the deploy group.
    let general = html.find(r#"href="/settings/general""#).unwrap();
    let notifications = html.find(r#"href="/settings/notifications""#).unwrap();
    let registries = html.find(r#"href="/settings/registries""#).unwrap();
    assert!(
        general < notifications && notifications < registries,
        "{html}"
    );
    for expected in [
        "not configured",
        "No mail is sent.",
        "No key is stored.",
        "No mail has been sent yet.",
        r#"<option value="resend" selected>Resend</option>"#,
        r#"type="password" name="api_key""#,
        "Must be on a domain verified at Resend.",
        r#"href="/activity?kind=operator&#38;severity=warning""#,
        r#"hx-post="/settings/notifications/test""#,
    ] {
        assert!(html.contains(expected), "{expected}: {html}");
    }
    assert!(!html.contains(" checked"), "nothing is switched on: {html}");
    assert!(
        !html.contains("Remove configuration"),
        "nothing to remove: {html}"
    );
}

#[tokio::test]
async fn saving_notifications_stores_the_key_and_the_settings_without_echoing_the_key() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post(
            "/settings/notifications",
            &browser,
            FORM,
            encoded(&CONFIGURED),
        ))
        .await;
    let (status, html) = keyless(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("flash-success"), "{html}");
    assert!(
        html.contains(
            "Notifications are on: problems and recoveries are mailed to ops@example.com"
        ),
        "{html}"
    );
    assert!(html.contains("The API key is stored"), "{html}");
    // The form shows what is stored, and the status block is swapped out of band.
    assert!(html.contains(r#"<section id="notify-form""#), "{html}");
    assert!(html.contains(r#"value="hoster@example.com""#), "{html}");
    assert!(
        html.contains(r#"<div id="notify-status" hx-swap-oob="outerHTML">"#),
        "{html}"
    );
    assert!(
        html.contains("Mailing ops@example.com from hoster@example.com"),
        "{html}"
    );
    assert_eq!(
        app.store.notification_settings().await.unwrap(),
        Some(NotificationSettings {
            provider: ProviderKind::Resend,
            from: Some("hoster@example.com".parse().unwrap()),
            recipient: Some("ops@example.com".parse().unwrap()),
            enabled: true,
            also_successes: false,
        })
    );
    assert_eq!(stored_key(&app).await.as_deref(), Some(API_KEY));

    // The page shows that a key is stored, never the key, and offers removal.
    let (_, html) = keyless(
        app.get("/settings/notifications", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(html.contains("A key is stored, last changed"), "{html}");
    assert!(
        html.contains(r#"name="enabled" aria-labelledby="notify-enabled-label" checked"#),
        "{html}"
    );
    assert!(html.contains("Remove configuration"), "{html}");
    // The key's change is in the event log, by name only.
    assert!(
        event_messages(&app)
            .await
            .contains(&"Set the Resend API key for notifications".to_owned())
    );
}

#[tokio::test]
async fn an_empty_key_field_keeps_the_stored_key() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&CONFIGURED),
    ))
    .await;
    let response = app
        .send(post(
            "/settings/notifications",
            &browser,
            FORM,
            encoded(&[
                ("provider", "resend"),
                ("api_key", ""),
                ("from", "hoster@example.com"),
                ("recipient", "oncall@example.com"),
                ("enabled", "on"),
                ("also_successes", "on"),
            ]),
        ))
        .await;
    let (status, html) = keyless(response).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("successes are mailed to oncall@example.com"),
        "{html}"
    );
    assert!(!html.contains("The API key is stored"), "{html}");
    assert_eq!(stored_key(&app).await.as_deref(), Some(API_KEY));
    let settings = app.store.notification_settings().await.unwrap().unwrap();
    assert_eq!(
        settings.recipient.unwrap().to_string(),
        "oncall@example.com"
    );
    assert!(settings.also_successes);
    assert_eq!(
        event_messages(&app).await,
        ["Set the Resend API key for notifications"],
        "an empty key field records no change"
    );
}

#[tokio::test]
async fn saving_notifications_shows_field_errors_and_stores_nothing() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let save =
        |fields: &[(&str, &str)]| post("/settings/notifications", &browser, FORM, encoded(fields));
    let response = app
        .send(save(&[
            ("provider", "resend"),
            ("api_key", ""),
            ("from", "hoster@"),
            ("recipient", "two words@example.com"),
            ("enabled", "on"),
        ]))
        .await;
    let (status, html) = keyless(response).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("Nothing was saved"), "{html}");
    assert!(
        html.contains(
            r#"<p id="notify-from-error" class="field-error">The address needs exactly one `@`"#
        ),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<p id="notify-recipient-error" class="field-error">The address contains a space"#
        ),
        "{html}"
    );
    assert!(
        html.contains("Enter the API key (none is stored) to switch notifications on."),
        "{html}"
    );
    // What was typed stays in the form; the key field is never filled in.
    assert!(html.contains(r#"value="hoster@""#), "{html}");
    assert!(html.contains(r#"aria-invalid="true""#), "{html}");

    // Enabled without addresses.
    let (status, html) = keyless(
        app.send(save(&[
            ("provider", "resend"),
            ("api_key", API_KEY),
            ("enabled", "on"),
        ]))
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        html.contains("Enter the from address to switch notifications on."),
        "{html}"
    );
    assert!(
        html.contains("Enter the recipient to switch notifications on."),
        "{html}"
    );
    // A key that is not one printable line, and an unknown provider.
    let (status, html) = keyless(
        app.send(save(&[
            ("provider", "mailjet"),
            ("api_key", "re_two words"),
        ]))
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("one line of printable characters"), "{html}");
    assert!(html.contains("Unknown mail provider `mailjet`."), "{html}");
    assert!(!html.contains("re_two words"), "{html}");

    assert_eq!(app.store.notification_settings().await.unwrap(), None);
    assert_eq!(stored_key(&app).await, None);

    // Switched off, an incomplete configuration may be saved.
    let (status, html) = keyless(
        app.send(save(&[
            ("provider", "resend"),
            ("from", "hoster@example.com"),
        ]))
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Notifications are off"), "{html}");
}

#[tokio::test]
async fn send_test_with_nothing_saved_sends_no_request() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let test = || {
        post(
            "/settings/notifications/test",
            &browser,
            FORM,
            String::new(),
        )
    };
    let (status, html) = keyless(app.send(test()).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("Nothing is saved yet"), "{html}");
    assert!(html.contains("No mail was sent."), "{html}");

    // Addresses saved, but no key.
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&[
            ("provider", "resend"),
            ("from", "hoster@example.com"),
            ("recipient", "ops@example.com"),
        ]),
    ))
    .await;
    let (status, html) = keyless(app.send(test()).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("No Resend API key is stored"), "{html}");
    assert!(app.mailer.sent().is_empty());
    assert_eq!(app.store.last_delivery().await.unwrap(), None);
}

#[tokio::test]
async fn send_test_delivers_through_the_saved_configuration_with_a_cooldown() {
    let app = TestApp::new().await;
    app.instance.set(&"prod-eu".parse().unwrap()).await.unwrap();
    let browser = app.sign_in().await;
    // Saved switched off: the test still goes out.
    let mut fields = CONFIGURED.to_vec();
    fields.retain(|(name, _)| *name != "enabled");
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&fields),
    ))
    .await;
    let test = || {
        post(
            "/settings/notifications/test",
            &browser,
            FORM,
            String::new(),
        )
    };
    let (status, html) = keyless(app.send(test()).await).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Sent: Resend accepted the test mail to ops@example.com (id fake-1)."),
        "{html}"
    );
    // The status block shows the delivery at once.
    assert!(
        html.contains(r#"<div id="notify-status" hx-swap-oob="outerHTML">"#),
        "{html}"
    );
    assert!(html.contains("accepted, id fake-1"), "{html}");
    let sent = app.mailer.sent();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].subject.starts_with("[prod-eu] "),
        "{}",
        sent[0].subject
    );
    assert_eq!(sent[0].from.to_string(), "hoster@example.com");
    assert_eq!(sent[0].to.to_string(), "ops@example.com");
    assert!(sent[0].idempotency_key.as_ref().starts_with("hoster-test-"));
    assert!(
        sent[0]
            .html
            .contains("https://hoster.example.com/settings/notifications")
    );
    assert_eq!(
        app.store.last_delivery().await.unwrap().unwrap().outcome,
        DeliveryOutcome::Sent {
            id: Some("fake-1".into())
        }
    );

    // At once again: refused without a request.
    let (status, html) = keyless(app.send(test()).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(html.contains("before sending another test"), "{html}");
    assert_eq!(app.mailer.sent().len(), 1);

    // The page shows the last delivery.
    let (_, html) = keyless(
        app.get("/settings/notifications", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(html.contains("accepted, id fake-1"), "{html}");
    assert!(html.contains("pill-ok"), "{html}");
}

#[tokio::test]
async fn send_test_shows_the_providers_refusal_in_plain_words() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&CONFIGURED),
    ))
    .await;
    app.mailer.answer(Err(rejected(
        403,
        "validation_error",
        "The example.com domain is not verified. Please, add and verify your domain.",
    )));
    let response = app
        .send(post(
            "/settings/notifications/test",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    let (status, html) = keyless(response).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(html.contains("flash-error"), "{html}");
    assert!(
        html.contains(
            "Sender domain not verified: the from address must be on a domain verified at Resend."
        ),
        "{html}"
    );
    assert!(
        html.contains("Resend answered 403 Forbidden (validation_error): The example.com domain is not verified"),
        "{html}"
    );
    assert!(html.contains("pill-fail"), "{html}");
    let last = app.store.last_delivery().await.unwrap().unwrap();
    assert!(
        matches!(&last.outcome, DeliveryOutcome::Failed { error } if error.contains("validation_error")),
        "{last:?}"
    );
}

#[test]
fn delivery_errors_are_explained_in_plain_words() {
    let explained = |err: DeliveryError| Explained(&err).to_string();
    for (err, expected) in [
        (
            rejected(403, "invalid_api_key", "API key is invalid"),
            "Invalid key: Resend refused the API key",
        ),
        (
            rejected(401, "missing_api_key", "Missing API key"),
            "Invalid key:",
        ),
        (
            rejected(
                403,
                "validation_error",
                "You can only send testing emails to your own email address",
            ),
            "Sender domain not verified: Resend's test sender only mails",
        ),
        (
            rejected(
                429,
                "daily_quota_exceeded",
                "You have reached your daily quota",
            ),
            "Quota used up: the Resend account's daily sending quota is spent.",
        ),
        (
            rejected(429, "monthly_quota_exceeded", "quota"),
            "monthly sending quota",
        ),
        (
            DeliveryError::Unavailable {
                provider: ProviderKind::Resend,
                status: HttpStatus::TOO_MANY_REQUESTS,
                code: Some("rate_limit_exceeded".into()),
                message: Some("Too many requests".into()),
                retry_after: None,
            },
            "Rate limited: Resend asked hoster to slow down",
        ),
        (
            DeliveryError::Unavailable {
                provider: ProviderKind::Resend,
                status: HttpStatus::BAD_GATEWAY,
                code: None,
                message: None,
                retry_after: None,
            },
            "Resend is unavailable at the moment. Resend answered 502 Bad Gateway.",
        ),
        (
            DeliveryError::Network {
                provider: ProviderKind::Resend,
                reason: "connection refused".into(),
            },
            "Network: hoster could not get an answer from Resend. Cannot reach Resend: connection refused.",
        ),
        (
            DeliveryError::Timeout {
                provider: ProviderKind::Resend,
                timeout: std::time::Duration::from_secs(20),
            },
            "Network: hoster could not get an answer from Resend.",
        ),
        (
            rejected(422, "invalid_parameter", "Invalid `to` field"),
            "Resend refused the mail. Resend answered 422 Unprocessable Entity",
        ),
    ] {
        let text = explained(err);
        assert!(text.contains(expected), "{expected}: {text}");
    }
}

#[tokio::test]
async fn removing_notifications_deletes_the_settings_and_the_key() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&CONFIGURED),
    ))
    .await;
    let remove = || {
        post(
            "/settings/notifications/remove",
            &browser,
            FORM,
            String::new(),
        )
    };
    let (status, html) = keyless(app.send(remove()).await).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Removed the notification settings and the API key"),
        "{html}"
    );
    // The form and the status show the empty configuration, out of band.
    assert!(
        html.contains(r#"<section id="notify-form" class="card settings-card" aria-labelledby="notify-config" hx-swap-oob="outerHTML">"#),
        "{html}"
    );
    assert!(html.contains("No key is stored."), "{html}");
    assert!(html.contains("not configured"), "{html}");
    assert_eq!(app.store.notification_settings().await.unwrap(), None);
    assert_eq!(stored_key(&app).await, None);
    assert!(
        event_messages(&app)
            .await
            .contains(&"Deleted the Resend API key for notifications".to_owned())
    );

    let (status, html) = keyless(app.send(remove()).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("nothing to remove"), "{html}");

    // The page asks for confirmation, as the secret pages do.
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&CONFIGURED),
    ))
    .await;
    let (_, html) = keyless(
        app.get("/settings/notifications", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(
        html.contains(r#"hx-confirm="Remove the notification settings and the API key?""#),
        "{html}"
    );
}

/// Makes the store fail as a busy or broken database would: each of `triggers` is a
/// `CREATE TRIGGER` that aborts a write.
async fn break_writes(app: &TestApp, triggers: &[&'static str]) {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", app.store.path().display()))
        .await
        .unwrap();
    for trigger in triggers {
        sqlx::query(*trigger).execute(&pool).await.unwrap();
    }
    pool.close().await;
}

#[tokio::test]
async fn a_save_the_store_refuses_keeps_the_form_and_stores_no_key() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    break_writes(
        &app,
        &[
            "CREATE TRIGGER fail_notification_settings_insert BEFORE INSERT ON notification_settings BEGIN SELECT RAISE(ABORT, 'broken'); END",
            "CREATE TRIGGER fail_notification_settings_update BEFORE UPDATE ON notification_settings BEGIN SELECT RAISE(ABORT, 'broken'); END",
        ],
    )
    .await;
    let (status, html) = keyless(
        app.send(post(
            "/settings/notifications",
            &browser,
            FORM,
            encoded(&CONFIGURED),
        ))
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{html}");
    // The answer is the form again, with what was typed and the error in its result area.
    assert!(html.contains(r#"<section id="notify-form""#), "{html}");
    assert!(html.contains(r#"value="hoster@example.com""#), "{html}");
    assert!(html.contains("could not be read or stored"), "{html}");
    // The settings are written first, so nothing is half-applied: no key, no key event.
    assert_eq!(app.store.notification_settings().await.unwrap(), None);
    assert_eq!(stored_key(&app).await, None);
    assert!(
        !event_messages(&app)
            .await
            .iter()
            .any(|message| message.contains("API key")),
    );
}

#[tokio::test]
async fn a_key_the_secret_store_refuses_says_the_settings_were_saved() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    break_writes(
        &app,
        &[
            "CREATE TRIGGER fail_secrets_insert BEFORE INSERT ON secrets BEGIN SELECT RAISE(ABORT, 'broken'); END",
            "CREATE TRIGGER fail_secrets_update BEFORE UPDATE ON secrets BEGIN SELECT RAISE(ABORT, 'broken'); END",
        ],
    )
    .await;
    let (status, html) = keyless(
        app.send(post(
            "/settings/notifications",
            &browser,
            FORM,
            encoded(&CONFIGURED),
        ))
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{html}");
    assert!(html.contains(r#"<section id="notify-form""#), "{html}");
    assert!(
        html.contains("The settings were saved, but the API key could not be stored"),
        "{html}"
    );
    assert!(app.store.notification_settings().await.unwrap().is_some());
    assert_eq!(stored_key(&app).await, None);
}

#[tokio::test]
async fn a_removal_the_store_refuses_says_what_is_left() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.send(post(
        "/settings/notifications",
        &browser,
        FORM,
        encoded(&CONFIGURED),
    ))
    .await;
    break_writes(
        &app,
        &["CREATE TRIGGER fail_notification_settings_delete BEFORE DELETE ON notification_settings BEGIN SELECT RAISE(ABORT, 'broken'); END"],
    )
    .await;
    let (status, html) = keyless(
        app.send(post(
            "/settings/notifications/remove",
            &browser,
            FORM,
            String::new(),
        ))
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{html}");
    // The key goes first; the settings that are left show in the form, out of band.
    assert!(
        html.contains("The API key was removed, but the notification settings could not be"),
        "{html}"
    );
    assert!(
        html.contains(r#"<section id="notify-form" class="card settings-card" aria-labelledby="notify-config" hx-swap-oob="outerHTML">"#),
        "{html}"
    );
    assert!(html.contains("No key is stored."), "{html}");
    assert!(app.store.notification_settings().await.unwrap().is_some());
    assert_eq!(stored_key(&app).await, None);
}

#[tokio::test]
async fn notification_posts_need_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    for uri in [
        "/settings/notifications",
        "/settings/notifications/test",
        "/settings/notifications/remove",
    ] {
        let request = Request::post(uri)
            .header(header::COOKIE, &browser.cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, FORM)
            .body(Body::from(encoded(&CONFIGURED)))
            .unwrap();
        let (status, _) = keyless(app.send(request).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(app.store.notification_settings().await.unwrap(), None);
    assert_eq!(stored_key(&app).await, None);
    assert!(app.mailer.sent().is_empty());
}
