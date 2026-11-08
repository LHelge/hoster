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
    ) -> Result<(), CredentialError> {
        let mut stored = self.stored.lock().unwrap();
        stored.retain(|(h, _)| h != host);
        stored.push((host.clone(), credential.clone()));
        Ok(())
    }

    async fn remove(&self, host: &RegistryHost) -> Result<bool, CredentialError> {
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
        html.contains(r#"<a href="/settings/registries" aria-current="page">Settings</a>"#),
        "{html}"
    );
    assert!(html.contains("<code>ghcr.io</code>"), "{html}");
    assert!(html.contains("<td>machine-bot</td>"), "{html}");
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
        layout: Layout::new(Section::Settings, &Session::for_tests("octocat", "c0ffee")),
        entries: vec![EntryView {
            host: "ghcr.io".into(),
            username: "bot".into(),
            changed_at: "2026-11-08T00:00:00Z".into(),
        }],
    }
    .render()
    .unwrap();
    assert!(page.contains("<td>bot</td>"), "{page}");
    // The only token field is the empty input of the form.
    assert_eq!(page.matches(r#"name="token""#).count(), 1, "{page}");
    assert!(!page.contains("value=\"ghp_"), "{page}");
}
