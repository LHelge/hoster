//! The Updates card and one-click updates (ADR-0061 §9), against the fake orchestrator.

use axum::http::StatusCode;

use crate::orchestrator::{StackUpdates, UpdateChoice, UpdateOffer};
use crate::store::{Timestamp, UpdateKind};
use crate::web::auth::tests::TestApp;
use crate::web::stacks::tests::{Call, FORM, body, digest, encoded, post};

/// `notes`' storage (rustfs 1.0.0): its tag moved, and a minor and a major are out.
fn offers() -> StackUpdates {
    let offer = |kind, to: &str, digest: Option<String>| UpdateOffer {
        kind,
        services: vec!["storage".into()],
        from: "1.0.0".into(),
        to: to.into(),
        digest: digest.map(|d| d.parse().unwrap()),
        first_seen: Timestamp::now(),
    };
    StackUpdates {
        offers: vec![
            offer(UpdateKind::Digest, "1.0.0", Some(digest(0xbb))),
            offer(UpdateKind::Minor, "1.1.0", None),
            offer(UpdateKind::Major, "2.0.0", None),
        ],
        errors: vec![("api".into(), "ghcr.io is rate-limiting requests".into())],
        checked_at: Some(Timestamp::now()),
    }
}

async fn app_with_offers() -> TestApp {
    let app = TestApp::new().await;
    app.stacks
        .updates
        .lock()
        .unwrap()
        .insert("notes".into(), offers());
    app
}

#[tokio::test]
async fn the_overview_lists_the_offers_and_update_all_leaves_majors_out() {
    let app = app_with_offers().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<h2 id="updates-title">Updates <span class="card-count">· 3</span></h2>"#),
        "{html}"
    );
    assert!(
        html.contains("tag 1.0.0 points at a new image (sha256:bbbbbbbbbbbb)"),
        "{html}"
    );
    assert!(html.contains("1.0.0 → 1.1.0"), "{html}");
    assert!(html.contains("1.0.0 → 2.0.0"), "{html}");
    assert!(
        html.contains("checked 0 s ago") || html.contains("checked "),
        "{html}"
    );
    for choice in ["storage:digest", "storage:minor", "storage:major"] {
        assert!(
            html.contains(&format!(
                r#"<input type="hidden" name="updates" value="{choice}">"#
            )),
            "{choice}: {html}"
        );
    }
    assert!(
        html.contains(
            r#"<input type="hidden" name="updates" value="storage:digest,storage:minor">"#
        ),
        "{html}"
    );
    assert!(
        html.contains("could not check: ghcr.io is rate-limiting requests"),
        "{html}"
    );

    // Nothing on offer: no card.
    app.stacks.updates.lock().unwrap().clear();
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(!html.contains(r#"id="updates-title""#), "{html}");
}

#[tokio::test]
async fn update_previews_the_file_change_then_applies_the_reviewed_revision() {
    let app = app_with_offers().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post(
            "/stacks/notes/updates/preview",
            &browser,
            FORM,
            encoded(&[("updates", "storage:minor")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("Updating notes"), "{html}");
    assert!(
        html.contains("image = &#34;docker.io/rustfs/rustfs:1.1.0&#34;"),
        "{html}"
    );
    assert!(
        html.contains(r#"<input type="hidden" name="revision" value="1">"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<input type="hidden" name="updates" value="storage:minor">"#),
        "{html}"
    );
    let choice: UpdateChoice = "storage:minor".parse().unwrap();
    assert!(
        app.stacks
            .calls()
            .contains(&Call::UpdatePreview("notes".into(), vec![choice.clone()]))
    );

    let response = app
        .send(post(
            "/stacks/notes/updates",
            &browser,
            FORM,
            encoded(&[("updates", "storage:minor"), ("revision", "1")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["hx-redirect"],
        "/stacks/notes/deployments/9"
    );
    assert!(
        app.stacks
            .calls()
            .contains(&Call::Update("notes".into(), vec![choice], 1))
    );
    assert_eq!(app.stacks.operators().len(), 1);

    // A stale confirmation is refused.
    let response = app
        .send(post(
            "/stacks/notes/updates",
            &browser,
            FORM,
            encoded(&[("updates", "storage:minor"), ("revision", "7")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn choices_that_are_not_on_offer_or_do_not_parse_are_refused() {
    let app = app_with_offers().await;
    let browser = app.sign_in().await;
    let preview = |updates: &'static str| {
        post(
            "/stacks/notes/updates/preview",
            &browser,
            FORM,
            encoded(&[("updates", updates)]),
        )
    };
    let response = app.send(preview("storage:patch")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let html = body(response).await;
    assert!(
        html.contains("patch update of storage is on offer any more"),
        "{html}"
    );
    for bad in ["", "storage", "storage:huge"] {
        let response = app.send(preview(bad)).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn the_stacks_list_and_the_dashboard_count_the_updates() {
    let app = app_with_offers().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<a href="/stacks/notes#updates">"#),
        "{html}"
    );
    assert!(html.contains("2 updates available"), "{html}");

    let html = body(app.get("/", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<a href="/stacks/notes#updates">notes</a>"#),
        "{html}"
    );
    assert!(html.contains("2 updates available"), "{html}");

    // Only a major: no badge, no warning.
    app.stacks.updates.lock().unwrap().insert(
        "notes".into(),
        StackUpdates {
            offers: offers().offers.split_off(2),
            ..StackUpdates::default()
        },
    );
    let html = body(app.get("/stacks", Some(&browser.cookie)).await).await;
    assert!(!html.contains("available</span>"), "{html}");
}
