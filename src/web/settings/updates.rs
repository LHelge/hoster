//! Settings → Updates (ADR-0061 §7): how often the update checker runs, when it last did, and
//! "Check now".

use std::time::{Duration, SystemTime};

use askama::Template;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Form};
use serde::Deserialize;

use super::{Settings, SettingsNav, SettingsPart};
use crate::registry::UpdateCheckHandle;
use crate::store::{CheckInterval, Store, Timestamp, UpdateSettings};
use crate::web::pages::{Layout, Page, Section, Shell};
use crate::web::stacks::{Ago, Outcome};

/// The Updates section: the settings row and the running checker.
#[derive(Clone)]
pub struct UpdatesSection {
    store: Store,
    checker: UpdateCheckHandle,
}

/// `GET /settings/updates`.
#[derive(Template)]
#[template(path = "settings_updates.html")]
struct UpdatesPage {
    layout: Layout,
    nav: SettingsNav,
    /// The interval as stored, written as the form takes it (`6h`).
    interval: String,
    /// "2 h ago", or that no check has run.
    last_pass: String,
}

/// The interval form.
#[derive(Deserialize)]
pub struct IntervalForm {
    interval: String,
}

impl UpdatesSection {
    /// The key-value item the checker writes the time of its last pass to.
    const LAST_PASS_KEY: &str = "updates.last_pass";

    pub fn new(store: Store, checker: UpdateCheckHandle) -> Self {
        Self { store, checker }
    }

    async fn page(&self, layout: Layout, nav: SettingsNav) -> Response {
        let settings = self.store.update_settings().await.unwrap_or_default();
        let last_pass = match self.store.kv_get(Self::LAST_PASS_KEY).await {
            Ok(Some(millis)) => millis
                .parse::<i64>()
                .ok()
                .and_then(|millis| u64::try_from(millis).ok())
                .map(|millis| SystemTime::UNIX_EPOCH + Duration::from_millis(millis))
                .map(|at| Ago::since(Timestamp::from(at), SystemTime::now()).to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            Ok(None) => "not yet".to_owned(),
            Err(err) => {
                tracing::warn!(error = %err, "cannot read the time of the last update check");
                "unknown".to_owned()
            }
        };
        Page(UpdatesPage {
            layout,
            nav,
            interval: settings.interval.to_string(),
            last_pass,
        })
        .into_response()
    }
}

impl Settings {
    /// `GET /settings/updates`.
    pub async fn updates(Extension(settings): Extension<Settings>, shell: Shell) -> Response {
        let nav = settings.nav(SettingsPart::Updates).await;
        let layout = shell.layout(Section::Settings);
        match &settings.updates {
            Some(updates) => updates.page(layout, nav).await,
            None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
        }
    }

    /// `POST /settings/updates`: stores the check interval; the checker reschedules at once.
    pub async fn save_updates(
        Extension(settings): Extension<Settings>,
        Form(form): Form<IntervalForm>,
    ) -> Response {
        let Some(updates) = &settings.updates else {
            return (StatusCode::NOT_FOUND, "not found\n").into_response();
        };
        let interval = humantime::parse_duration(form.interval.trim())
            .map_err(|err| {
                format!(
                    "{:?} is not a duration such as 6h or 1day: {err}",
                    form.interval
                )
            })
            .and_then(CheckInterval::try_from);
        let interval = match interval {
            Ok(interval) => interval,
            Err(err) => {
                return Outcome::error(StatusCode::UNPROCESSABLE_ENTITY, Outcome::sentence(&err))
                    .into_response();
            }
        };
        if let Err(err) = updates
            .store
            .set_update_settings(&UpdateSettings { interval })
            .await
        {
            tracing::error!(error = %err, "storing the update settings failed");
            return Outcome::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The update settings could not be stored.",
            )
            .into_response();
        }
        updates.checker.reschedule();
        Outcome::success(format!(
            "Updates are checked every {interval} from now on, counted from the last check."
        ))
        .into_response()
    }

    /// `POST /settings/updates/check`: runs a check now.
    pub async fn check_updates(Extension(settings): Extension<Settings>) -> Response {
        let Some(updates) = &settings.updates else {
            return (StatusCode::NOT_FOUND, "not found\n").into_response();
        };
        updates.checker.check_now();
        Outcome::success(
            "A check runs now. Its results appear on the stack pages and in the events.",
        )
        .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::UpdateCheckHandle;
    use crate::web::auth::tests::TestApp;
    use crate::web::stacks::tests::{FORM, body, encoded, post};

    #[tokio::test]
    async fn the_interval_is_saved_within_bounds_and_check_now_wakes_the_checker() {
        let mut app = TestApp::new().await;
        let (handle, mut asked) = UpdateCheckHandle::detached();
        app.app.settings = app
            .app
            .settings
            .clone()
            .with_updates(UpdatesSection::new(app.store.clone(), handle));
        let browser = app.sign_in().await;

        let html = body(app.get("/settings/updates", Some(&browser.cookie)).await).await;
        assert!(
            html.contains(r#"<input id="updates-interval" name="interval" value="6h""#),
            "{html}"
        );
        assert!(html.contains("not yet"), "{html}");
        assert!(
            html.contains(r#"href="/settings/updates" aria-current="page">image updates"#),
            "{html}"
        );

        let save = |interval: &str| {
            post(
                "/settings/updates",
                &browser,
                FORM,
                encoded(&[("interval", interval)]),
            )
        };
        let response = app.send(save("12h")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body(response).await.contains("checked every 12h"));
        assert_eq!(
            app.store.update_settings().await.unwrap().interval,
            CheckInterval::try_from(Duration::from_secs(12 * 3600)).unwrap()
        );
        assert_eq!(asked(), Some("reschedule"));

        for bad in ["30m", "8days", "often"] {
            let response = app.send(save(bad)).await;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
        assert_eq!(asked(), None);

        app.store
            .kv_set(
                UpdatesSection::LAST_PASS_KEY,
                &Timestamp::now().as_millis().to_string(),
            )
            .await
            .unwrap();
        let html = body(app.get("/settings/updates", Some(&browser.cookie)).await).await;
        assert!(!html.contains("not yet"), "{html}");
        assert!(html.contains(r#"value="12h""#), "{html}");

        let response = app
            .send(post("/settings/updates/check", &browser, FORM, ""))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(asked(), Some("check now"));
    }
}
