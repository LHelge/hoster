//! Settings → Registries through the whole app against a fake credential store and registry
//! client. Tokens must never come back in any response.

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
const SECTIONS: [&str; 5] = ["general", "registries", "webhook", "proxy", "certificates"];

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
        let configured = slug != "registries";
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
