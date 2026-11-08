//! The stack environment tab and its secret forms through the whole app against the fake orchestrator. Every response is
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
async fn the_tab_lists_variables_and_secret_names_but_no_values() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let (status, html) = clean(
        app.get("/stacks/notes/environment", Some(&browser.cookie))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Under the shared header, the environment tab current.
    assert!(
        html.contains(r#"<a role="tab" href="/stacks/notes/environment" aria-selected="true" aria-current="page">environment</a>"#),
        "{html}"
    );
    assert!(html.contains(">notes</a>/environment</nav>"), "{html}");
    // A plain variable shows its value from the stack file, with no edit.
    assert!(
        html.contains(r#"<code class="env-key">RUST_LOG</code>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<code class="wrap env-value">info,notes=debug</code>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<code class="env-key">RUSTFS_ADDRESS</code>"#),
        "{html}"
    );
    // A secret shows its name, when it was set and its type.
    assert!(
        html.contains(r#"<code class="env-key">DATABASE_URL</code>"#),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<code class="env-secret">notes/db</code><span class="env-changed">set <time"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag env-type">secret</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag tag-neutral env-type">var</span>"#),
        "{html}"
    );
    // Referenced but not set: flagged, offered to set, not to delete.
    assert!(
        html.contains(&format!(
            r#"<code class="env-secret">notes/missing</code> <span class="pill pill-warn">{}not set</span>"#,
            crate::web::Icon::Warning
        )),
        "{html}"
    );
    assert!(
        html.contains(r##"href="?set=missing#set-secret">set<"##),
        "{html}"
    );
    assert!(
        html.contains(r##"href="?set=db#set-secret">replace<"##),
        "{html}"
    );
    // Stored but unreferenced: listed without a key or service, deleted after a browser
    // confirm; a referenced one asks the server for the typed confirmation.
    assert!(
        html.contains(r#"<code class="env-secret">notes/old</code>"#),
        "{html}"
    );
    assert!(html.contains(r#"hx-confirm="Delete notes/old?""#), "{html}");
    assert!(!html.contains(r#"hx-confirm="Delete notes/db?""#), "{html}");
    assert!(
        html.contains(r#"<input type="hidden" name="name" value="db">"#),
        "{html}"
    );
    assert!(
        !html.contains(r#"<input type="hidden" name="name" value="missing">"#),
        "{html}"
    );
    assert!(html.contains("· 3 variables · 3 secrets"), "{html}");
    // The PORT hoster adds for api's route, marked as such.
    assert!(
        html.contains(
            r#"<code class="wrap env-value">8080</code><span class="env-changed">added by hoster"#
        ),
        "{html}"
    );
    // The forms post to the tab's own URL; the value field starts empty.
    assert!(
        html.contains(r##"hx-post="/stacks/notes/environment" hx-target="#secret-result""##),
        "{html}"
    );
    assert!(
        html.contains(r#"hx-post="/stacks/notes/environment/delete""#),
        "{html}"
    );
    assert!(
        html.contains(r#"required autocomplete="off" spellcheck="false"></textarea>"#),
        "{html}"
    );
    assert!(html.contains(r#"<option value="old">"#), "{html}");

    let (status, _) = clean(
        app.get("/stacks/ghost/environment", Some(&browser.cookie))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_prefills_a_valid_name_only() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(
        app.get(
            "/stacks/notes/environment?set=missing",
            Some(&browser.cookie),
        )
        .await,
    )
    .await;
    assert!(
        html.contains(r#"list="secret-names" value="missing">"#),
        "{html}"
    );
    let html = body(
        app.get(
            "/stacks/notes/environment?set=%22%3E%3Cb",
            Some(&browser.cookie),
        )
        .await,
    )
    .await;
    assert!(html.contains(r#"list="secret-names">"#), "{html}");
}

#[tokio::test]
async fn the_old_secrets_url_redirects_to_the_tab() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .get("/stacks/notes/secrets", Some(&browser.cookie))
        .await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        response_header(&response, "location"),
        "/stacks/notes/environment"
    );
    // The old form endpoint still saves.
    let response = app
        .send(post(
            "/stacks/notes/secrets",
            &browser,
            FORM,
            encoded(&[("name", "old"), ("value", SECRET_VALUE), ("confirm", "old")]),
        ))
        .await;
    let (status, html) = clean(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Saved notes/old."), "{html}");
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
        .send(post(
            "/stacks/notes/environment",
            &browser,
            FORM,
            form(None),
        ))
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
            "/stacks/notes/environment",
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
            "/stacks/notes/environment",
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
            "/stacks/notes/environment",
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
            "/stacks/notes/environment",
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
                "/stacks/notes/environment",
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
            "/stacks/notes/environment/delete",
            &browser,
            FORM,
            encoded(&[("name", "db"), ("confirm", confirm)]),
        )
    };
    // The first post, from the row's button: the services it redeploys and the name to type.
    let response = app
        .send(post(
            "/stacks/notes/environment/delete",
            &browser,
            FORM,
            encoded(&[("name", "db")]),
        ))
        .await;
    let (status, html) = clean(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Deleting <code>notes/db</code> redeploys"),
        "{html}"
    );
    assert!(html.contains("<li>api</li>"), "{html}");
    assert!(
        html.contains("Type <strong class=\"mono\">db</strong> to confirm"),
        "{html}"
    );
    assert!(
        html.contains(r##"hx-post="/stacks/notes/environment/delete" hx-target="#delete-result""##),
        "{html}"
    );
    assert!(
        html.contains("Delete and redeploy 1 service</button>"),
        "{html}"
    );
    assert!(app.stacks.calls().is_empty());

    // A wrong name typed is refused.
    let (status, html) = clean(app.send(delete("")).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("is referenced by api"), "{html}");
    assert!(
        html.contains("fail until the secret is set again"),
        "{html}"
    );
    // The form stays, so the name can be typed again.
    assert!(html.contains(r#"name="confirm""#), "{html}");
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
            "/stacks/notes/environment/delete",
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
                "/stacks/notes/environment/delete",
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
    for uri in [
        "/stacks/notes/environment",
        "/stacks/notes/environment/delete",
        "/stacks/notes/secrets",
        "/stacks/notes/secrets/delete",
    ] {
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

#[test]
fn rows_follow_the_stack_file_then_the_unreferenced_secrets() {
    let status = crate::web::stacks::tests::notes_status();
    let secrets = vec![
        StackSecret {
            name: "db".into(),
            updated_at: Some(crate::store::Timestamp::now()),
            services: vec!["api".into()],
        },
        StackSecret {
            name: "old".into(),
            updated_at: Some(crate::store::Timestamp::now()),
            services: Vec::new(),
        },
    ];
    let table = EnvTable::new(&status, &secrets, SystemTime::now());
    let rows: Vec<_> = table
        .rows
        .iter()
        .map(|r| {
            (
                r.key.as_deref(),
                r.value.text(),
                r.service.as_deref(),
                r.value.to_string(),
                r.value
                    .secret()
                    .map(|s| (s.changed.is_some(), s.referenced)),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                Some("DATABASE_URL"),
                "notes/db",
                Some("api"),
                "secret".to_owned(),
                Some((true, true))
            ),
            (Some("PORT"), "8080", Some("api"), "var".to_owned(), None),
            (
                Some("RUST_LOG"),
                "info,notes=debug",
                Some("api"),
                "var".to_owned(),
                None
            ),
            // Referenced, but the store has no such secret: not set.
            (
                Some("S3_SECRET_KEY"),
                "notes/missing",
                Some("api"),
                "secret".to_owned(),
                Some((false, true))
            ),
            (
                Some("RUSTFS_ADDRESS"),
                ":9000",
                Some("storage"),
                "var".to_owned(),
                None
            ),
            (
                None,
                "notes/old",
                None,
                "secret".to_owned(),
                Some((true, false))
            ),
        ]
    );
    assert_eq!(table.counts(), (3, 3));
    assert!(table.error.is_none());
}

#[test]
fn an_unreadable_stack_file_still_lists_the_stored_secrets() {
    let mut status = crate::web::stacks::tests::notes_status();
    status.revision.toml = "not toml".into();
    let secrets = vec![StackSecret {
        name: "db".into(),
        updated_at: None,
        services: vec!["api".into(), "worker".into()],
    }];
    let table = EnvTable::new(&status, &secrets, SystemTime::now());
    assert!(table.error.is_some());
    assert_eq!(table.rows.len(), 1);
    assert_eq!(table.rows[0].service.as_deref(), Some("api, worker"));
    assert!(table.rows[0].value.secret().unwrap().referenced);
}

#[test]
fn the_delete_confirmation_carries_no_value() {
    let html = DeleteConfirm {
        action: "/stacks/notes/environment/delete".into(),
        secret: "notes/db".into(),
        name: "db".into(),
        services: vec!["api".into(), "worker".into()],
        error: None,
    }
    .render()
    .unwrap();
    assert!(html.contains("Delete and redeploy 2 services"), "{html}");
    assert!(!html.contains(r#"name="value""#), "{html}");
}

#[test]
fn the_head_counts_each_secret_once() {
    let secret = |key: &str, service: &str| EnvRow {
        key: Some(key.into()),
        service: Some(service.into()),
        value: EnvValue::Secret {
            full: "notes/db".into(),
            cell: SecretCell {
                name: "db".into(),
                changed: None,
                referenced: true,
            },
        },
    };
    let table = EnvTable {
        rows: vec![
            secret("DATABASE_URL", "api"),
            secret("DATABASE_URL", "worker"),
            EnvRow {
                key: Some("RUST_LOG".into()),
                service: Some("api".into()),
                value: EnvValue::Var {
                    value: "info".into(),
                    injected: false,
                },
            },
        ],
        revision: 1,
        error: None,
    };
    assert_eq!(table.counts(), (1, 1));
}
