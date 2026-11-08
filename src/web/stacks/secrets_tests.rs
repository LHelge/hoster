//! Stack → Secrets through the whole app against the fake orchestrator. Every response is
//! checked for the secret's value, which must never come back.

use axum::body::Body;
use axum::http::{Request, header};

use super::*;
use crate::web::auth::tests::{ORIGIN, TestApp, header as response_header};
use crate::web::stacks::tests::{Call, FORM, SECRET_VALUE, body, encoded, post};

/// The body of `response`, asserted to be free of the secret's value.
async fn clean(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let html = body(response).await;
    assert!(!html.contains(SECRET_VALUE), "the value leaked: {html}");
    (status, html)
}

#[tokio::test]
async fn the_page_lists_names_times_and_users_but_no_values() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let (status, html) = clean(
        app.get("/stacks/notes/secrets", Some(&browser.cookie))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<code>notes/db</code>"), "{html}");
    assert!(html.contains("<code>notes/old</code>"), "{html}");
    // Referenced but not set.
    assert!(
        html.contains(&format!(
            r#"<code>notes/missing</code></td>
      <td><span class="badge badge-error">{}not set</span>"#,
            crate::web::Icon::Warning
        )),
        "{html}"
    );
    // A referenced secret needs its name typed to delete; an unused one only a click.
    assert!(
        html.contains("Type <strong>db</strong> to delete"),
        "{html}"
    );
    assert!(html.contains(r#"hx-confirm="Delete notes/old?""#), "{html}");
    assert!(!html.contains("Type <strong>missing</strong>"), "{html}");
    // The value field starts empty.
    assert!(
        html.contains(r#"required autocomplete="off" spellcheck="false"></textarea>"#),
        "{html}"
    );

    let (status, _) = clean(
        app.get("/stacks/ghost/secrets", Some(&browser.cookie))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn saving_first_lists_the_services_to_redeploy_then_stores_on_confirmation() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let form = |confirm: Option<&str>| {
        let mut fields = vec![("name", "db"), ("value", SECRET_VALUE)];
        if let Some(confirm) = confirm {
            fields.push(("confirm", confirm));
        }
        encoded(&fields)
    };

    // Review: nothing is stored, the value is not echoed.
    let response = app
        .send(post("/stacks/notes/secrets", &browser, FORM, form(None)))
        .await;
    let (status, html) = clean(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Replaces the value of <code>notes/db</code>"),
        "{html}"
    );
    assert!(html.contains("<li>api</li>"), "{html}");
    assert!(
        html.contains(r#"<input type="hidden" name="confirm" value="db" form="secret-form">"#),
        "{html}"
    );
    assert!(
        html.contains("Save and redeploy 1 service</button>"),
        "{html}"
    );
    assert!(app.stacks.calls().is_empty());

    // A confirmation for another name is a new review.
    let response = app
        .send(post(
            "/stacks/notes/secrets",
            &browser,
            FORM,
            form(Some("old")),
        ))
        .await;
    let (_, html) = clean(response).await;
    assert!(html.contains("Save and redeploy"), "{html}");
    assert!(app.stacks.calls().is_empty());

    // Confirmed: stored (with browser line endings normalized) and redeployed.
    let response = app
        .send(post(
            "/stacks/notes/secrets",
            &browser,
            FORM,
            encoded(&[
                ("name", "db"),
                ("value", &format!("{SECRET_VALUE}\r\nline2")),
                ("confirm", "db"),
            ]),
        ))
        .await;
    assert_eq!(
        response_header(&response, "hx-redirect"),
        "/stacks/notes/deployments/11"
    );
    let (status, html) = clean(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Saved notes/db; deployment 11 redeploys api"),
        "{html}"
    );
    assert_eq!(
        app.stacks.calls(),
        [Call::SetSecret(
            "notes".into(),
            "db".into(),
            format!("{SECRET_VALUE}\nline2")
        )]
    );
}

#[tokio::test]
async fn an_unreferenced_secret_is_saved_without_a_redeploy() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let review = app
        .send(post(
            "/stacks/notes/secrets",
            &browser,
            FORM,
            encoded(&[("name", "fresh"), ("value", SECRET_VALUE)]),
        ))
        .await;
    let (_, html) = clean(review).await;
    assert!(html.contains("Sets <code>notes/fresh</code>"), "{html}");
    assert!(html.contains("saving redeploys nothing"), "{html}");
    assert!(html.contains(">Save</button>"), "{html}");

    let saved = app
        .send(post(
            "/stacks/notes/secrets",
            &browser,
            FORM,
            encoded(&[
                ("name", "fresh"),
                ("value", SECRET_VALUE),
                ("confirm", "fresh"),
            ]),
        ))
        .await;
    assert!(saved.headers().get("hx-redirect").is_none());
    let (status, html) = clean(saved).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("nothing was redeployed"), "{html}");
}

#[tokio::test]
async fn bad_names_and_empty_values_are_refused() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    for (name, value) in [
        ("a/b", SECRET_VALUE),
        ("..", SECRET_VALUE),
        ("", SECRET_VALUE),
        ("db", ""),
    ] {
        let response = app
            .send(post(
                "/stacks/notes/secrets",
                &browser,
                FORM,
                encoded(&[("name", name), ("value", value), ("confirm", name)]),
            ))
            .await;
        let (status, _) = clean(response).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name:?}");
    }
    assert!(app.stacks.calls().is_empty());
}

#[tokio::test]
async fn deleting_a_referenced_secret_needs_its_name_typed() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let delete = |confirm: &str| {
        post(
            "/stacks/notes/secrets/delete",
            &browser,
            FORM,
            encoded(&[("name", "db"), ("confirm", confirm)]),
        )
    };
    let (status, html) = clean(app.send(delete("")).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("is referenced by api"), "{html}");
    assert!(
        html.contains("fail until the secret is set again"),
        "{html}"
    );
    assert!(app.stacks.calls().is_empty());

    let response = app.send(delete("db")).await;
    assert_eq!(
        response_header(&response, "hx-redirect"),
        "/stacks/notes/deployments/12"
    );
    let (_, html) = clean(response).await;
    assert!(
        html.contains("Deleted notes/db; deployment 12 redeploys api"),
        "{html}"
    );
    assert_eq!(
        app.stacks.calls(),
        [Call::DeleteSecret("notes".into(), "db".into())]
    );
}

#[tokio::test]
async fn an_unused_secret_deletes_without_typing_and_unknown_ones_are_not_found() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post(
            "/stacks/notes/secrets/delete",
            &browser,
            FORM,
            encoded(&[("name", "old")]),
        ))
        .await;
    let (status, html) = clean(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Deleted notes/old. No service references it"),
        "{html}"
    );

    for name in ["missing", "ghost"] {
        let response = app
            .send(post(
                "/stacks/notes/secrets/delete",
                &browser,
                FORM,
                encoded(&[("name", name), ("confirm", name)]),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{name}");
    }
}

#[tokio::test]
async fn secret_posts_need_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    for uri in ["/stacks/notes/secrets", "/stacks/notes/secrets/delete"] {
        let request = Request::post(uri)
            .header(header::COOKIE, &browser.cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, FORM)
            .body(Body::from(encoded(&[
                ("name", "db"),
                ("value", SECRET_VALUE),
                ("confirm", "db"),
            ])))
            .unwrap();
        let (status, _) = clean(app.send(request).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    assert!(app.stacks.calls().is_empty());
}

#[test]
fn templates_render_without_any_value() {
    let confirm = SecretConfirm {
        secret: "notes/db".into(),
        name: "db".into(),
        exists: true,
        services: vec!["api".into(), "worker".into()],
    }
    .render()
    .unwrap();
    assert!(
        confirm.contains("Save and redeploy 2 services"),
        "{confirm}"
    );
    // The form does not carry the value: only the name is echoed.
    assert!(!confirm.contains(r#"name="value""#), "{confirm}");
}
