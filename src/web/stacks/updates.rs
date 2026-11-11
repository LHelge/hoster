//! Updates of third-party images on the stack pages (ADR-0061 §9): the overview's Updates
//! card, and "Update" / "Update all", which open the usual preview before anything is applied.

use std::time::SystemTime;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Form};
use serde::Deserialize;

use super::apply::UpdateView;
use super::list::Ago;
use super::{Outcome, StackView, Stacks};
use crate::orchestrator::{StackUpdates, UpdateChoice, UpdateOffer};
use crate::store::{ShortDigest, UpdateKind};
use crate::web::auth::Session;
use crate::web::pages::Page;
use crate::web::ui::{Pill, Tone};

/// The overview's Updates card: what the update checker found for the stack, and why it
/// could not check some services.
#[derive(Debug)]
pub(super) struct UpdatesCard {
    pub(super) preview_url: String,
    pub(super) offers: Vec<OfferRow>,
    /// The choices "Update all" sends: every offer but majors. Empty when only majors are on
    /// offer.
    pub(super) all: String,
    pub(super) errors: Vec<(String, String)>,
    /// "checked 2 h ago", or that no check ran yet.
    pub(super) checked: String,
}

/// One offer on the card.
#[derive(Debug)]
pub(super) struct OfferRow {
    /// The service, or the group's services joined.
    pub(super) services: String,
    /// What changes: `v2.3.1 → v2.4.0`, or that the tag points at a new image.
    pub(super) change: String,
    pub(super) kind: Pill,
    pub(super) choice: String,
    pub(super) major: bool,
    /// "found 3 h ago".
    pub(super) found: String,
}

impl UpdatesCard {
    /// The card of `stack`, or `None` when there is nothing to show: no update, no error.
    pub(super) fn new(stack: &str, updates: &StackUpdates, now: SystemTime) -> Option<Self> {
        if updates.offers.is_empty() && updates.errors.is_empty() {
            return None;
        }
        Some(Self {
            preview_url: format!("{}/updates/preview", StackView::url(stack)),
            offers: updates
                .offers
                .iter()
                .map(|offer| OfferRow::new(offer, now))
                .collect(),
            all: Choices::of(updates.applicable()),
            errors: updates.errors.clone(),
            checked: updates.checked_at.map_or_else(
                || "not checked yet".to_owned(),
                |at| format!("checked {}", Ago::since(at, now)),
            ),
        })
    }
}

impl OfferRow {
    fn new(offer: &UpdateOffer, now: SystemTime) -> Self {
        let change = match (offer.kind, &offer.digest) {
            (UpdateKind::Digest, Some(digest)) => format!(
                "tag {} points at a new image ({})",
                offer.from,
                ShortDigest(digest.as_ref())
            ),
            (UpdateKind::Digest, None) => format!("tag {} points at a new image", offer.from),
            _ => format!("{} → {}", offer.from, offer.to),
        };
        let tone = match offer.kind {
            UpdateKind::Major => Tone::Warn,
            _ => Tone::Progress,
        };
        let label = match offer.kind {
            UpdateKind::Digest => "moved tag",
            kind => kind.as_str(),
        };
        Self {
            services: offer.services.join(" + "),
            change,
            kind: tone.pill(label),
            choice: offer.choice().to_string(),
            major: offer.kind == UpdateKind::Major,
            found: format!("found {}", Ago::since(offer.first_seen, now)),
        }
    }
}

/// The choices a form carries, as one comma-separated field: `server:minor,db:digest`.
/// Service names hold no commas.
struct Choices;

impl Choices {
    fn of<'a>(offers: impl Iterator<Item = &'a UpdateOffer>) -> String {
        offers
            .map(|o| o.choice().to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    fn parse(field: &str) -> Result<Vec<UpdateChoice>, Outcome> {
        let choices = field
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::parse)
            .collect::<Result<Vec<UpdateChoice>, String>>()
            .map_err(|e| Outcome::error(StatusCode::UNPROCESSABLE_ENTITY, e))?;
        if choices.is_empty() {
            return Err(Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose an update to apply.",
            ));
        }
        Ok(choices)
    }
}

/// The "Update" and "Update all" forms.
#[derive(Deserialize)]
pub struct UpdatePreviewForm {
    updates: String,
}

/// The confirmation of an update preview.
#[derive(Deserialize)]
pub struct UpdateForm {
    updates: String,
    revision: u32,
}

impl Stacks {
    /// `POST /stacks/{name}/updates/preview`: what applying the chosen updates would do.
    pub async fn update_preview(
        Extension(stacks): Extension<Stacks>,
        Path(name): Path<String>,
        Form(form): Form<UpdatePreviewForm>,
    ) -> Response {
        let choices = match Choices::parse(&form.updates) {
            Ok(choices) => choices,
            Err(outcome) => return outcome.into_response(),
        };
        match stacks.0.update_preview(&name, &choices).await {
            Ok(update) => Page(UpdateView::new(update, form.updates)).into_response(),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `POST /stacks/{name}/updates`: applies the reviewed updates to the reviewed revision as
    /// a new revision and redeploys what changes (ADR-0061 §9, ADR-0049).
    pub async fn update(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
        Form(form): Form<UpdateForm>,
    ) -> Response {
        let choices = match Choices::parse(&form.updates) {
            Ok(choices) => choices,
            Err(outcome) => return outcome.into_response(),
        };
        match stacks
            .0
            .update(&name, &choices, form.revision, &session.login().into())
            .await
        {
            Ok(outcome) => Self::applied(outcome),
            Err(err) => Outcome::from(err).into_response(),
        }
    }
}

#[cfg(test)]
mod tests;
