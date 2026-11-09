//! The `/apply` page (ADR-0027, ADR-0049), laid out as the design's "New stack" (epic 8bf): the
//! stack file in an editor on the left, and on the right an aside that `POST /apply/preview`
//! fills with the **Plan** (what appears `+`, changes `~`, goes away `−` or stays `=`), the
//! **Start order** of the services it (re)deploys (ADR-0056: start waves by `depends_on`, then
//! the stop-first replacements one at a time), the **Pre-flight** checks the preview passed
//! with its warnings (`!`), and the confirm button. The
//! diff of the stack file goes into a full-width card under both, as an out-of-band swap, so the
//! aside can stay in view while the editor scrolls. When the file fails a check, the aside shows
//! the problems in a fail-tinted Pre-flight card and the diff is cleared.
//!
//! Everything here comes from the preview and the two files: the plan's routes are those of the
//! active revision and the new file, compared. "Re-resolve tags" (ADR-0025) uses the same plan
//! list for its pins and redeploys.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use askama::Template;
use axum::response::{IntoResponse, Response};
use similar::{ChangeTag, TextDiff};

use super::{Outcome, Problem, StackView};
use crate::orchestrator::{ApplyPreview, PinChange, Wave};
use crate::stack::Stack;
use crate::web::pages::{Layout, Page};

/// `GET /apply`.
#[derive(Template)]
#[template(path = "apply.html")]
pub struct ApplyPage {
    pub(super) layout: Layout,
}

/// How a plan line changes something, drawn as its sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Appears: a new stack, service, route or pin.
    Add,
    /// Changes: a redeploy, a replaced revision, a moved pin.
    Change,
    /// Goes away: a service or route the new file drops.
    Remove,
    /// Stays as it is.
    Keep,
}

impl Mark {
    /// The CSS class suffix: `plan-add`, `plan-change`, `plan-remove` or `plan-keep`.
    fn kind(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Change => "change",
            Self::Remove => "remove",
            Self::Keep => "keep",
        }
    }

    /// What the sign says, for screen readers: the sign itself is hidden from them.
    fn label(self) -> &'static str {
        match self {
            Self::Add => "Adds",
            Self::Change => "Changes",
            Self::Remove => "Removes",
            Self::Keep => "Keeps",
        }
    }
}

impl fmt::Display for Mark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Add => "+",
            Self::Change => "~",
            Self::Remove => "\u{2212}",
            Self::Keep => "=",
        })
    }
}

/// One line of a plan: `<verb> <subject> <note>`, the subject optionally a link.
#[derive(Debug, PartialEq, Eq)]
struct Step {
    mark: Mark,
    verb: &'static str,
    subject: String,
    href: Option<String>,
    note: Option<String>,
}

impl Step {
    fn new(mark: Mark, verb: &'static str, subject: impl Into<String>) -> Self {
        Self {
            mark,
            verb,
            subject: subject.into(),
            href: None,
            note: None,
        }
    }

    fn linked(mut self, href: String) -> Self {
        self.href = Some(href);
        self
    }

    fn noted(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// The services an apply redeploys. A service only `after` has is new (every service is,
    /// for a new stack); one only `before` has leaves the stack. Without both service sets
    /// (a re-resolve, or an active revision that no longer parses), each one is a redeploy.
    fn services(
        changed: &[String],
        before: Option<&BTreeSet<String>>,
        after: Option<&BTreeSet<String>>,
    ) -> Vec<Self> {
        if changed.is_empty() {
            return vec![
                Self::new(Mark::Keep, "redeploy", "no service")
                    .noted("every service already runs this configuration"),
            ];
        }
        changed
            .iter()
            .map(|service| {
                let known = |set: Option<&BTreeSet<String>>| set.map(|s| s.contains(service));
                // A changed service the file lacks can only be a removal: every other change
                // comes from the file. Check it first, so one that runs but is missing from
                // the active revision as well still reads as a removal.
                match (known(before), known(after)) {
                    (_, Some(false)) => {
                        Self::new(Mark::Remove, "remove", service).noted("not in the file")
                    }
                    (Some(false), _) => {
                        Self::new(Mark::Add, "deploy", service).noted("new service")
                    }
                    _ => Self::new(Mark::Change, "redeploy", service),
                }
            })
            .collect()
    }

    /// The routes that appear or go away between the `before` and `after` route sets.
    fn routes(before: &BTreeSet<String>, after: &BTreeSet<String>) -> Vec<Self> {
        let added = after
            .difference(before)
            .map(|route| Self::new(Mark::Add, "route", route.clone()));
        let removed = before
            .difference(after)
            .map(|route| Self::new(Mark::Remove, "route", route.clone()));
        added.chain(removed).collect()
    }
}

/// One line of the start order: `start db, cache`, `replace db`, with what it waits for.
#[derive(Debug, PartialEq, Eq)]
struct OrderLine {
    verb: &'static str,
    services: Vec<String>,
    note: Option<String>,
}

impl From<&Wave> for OrderLine {
    fn from(wave: &Wave) -> Self {
        let names = |names: &[crate::stack::Name]| {
            names.iter().map(ToString::to_string).collect::<Vec<_>>()
        };
        match wave {
            Wave::Start(services) => {
                let waits: Vec<String> = services
                    .iter()
                    .filter(|(_, after)| !after.is_empty())
                    .map(|(service, after)| format!("{service} after {}", names(after).join(", ")))
                    .collect();
                Self {
                    verb: "start",
                    services: services.keys().map(ToString::to_string).collect(),
                    note: (!waits.is_empty()).then(|| waits.join("; ")),
                }
            }
            Wave::Replace(service) => Self {
                verb: "replace",
                services: vec![service.to_string()],
                note: Some("stop-first: down until the new one passes".to_owned()),
            },
            Wave::HeldBack { service, behind } => Self {
                verb: "start",
                services: vec![service.to_string()],
                note: Some(format!(
                    "held back until {} {}",
                    names(behind).join(", "),
                    if behind.len() == 1 {
                        "is replaced and passes"
                    } else {
                        "are replaced and pass"
                    }
                )),
            },
        }
    }
}

/// What a stack file declares that the plan compares: its services and routes.
#[derive(Debug, Default)]
struct Shape {
    services: BTreeSet<String>,
    /// `host/prefix → service:port`.
    routes: BTreeSet<String>,
}

impl From<&Stack> for Shape {
    fn from(stack: &Stack) -> Self {
        Self {
            services: stack.services.keys().map(ToString::to_string).collect(),
            routes: stack
                .services
                .iter()
                .flat_map(|(name, service)| {
                    service.routes.iter().map(move |route| {
                        format!("{}{} → {name}:{}", route.host, route.prefix, route.port)
                    })
                })
                .collect(),
        }
    }
}

impl FromStr for Shape {
    type Err = <Stack as FromStr>::Err;

    /// The shape of a stack file; an error when it does not parse.
    fn from_str(toml: &str) -> Result<Self, Self::Err> {
        toml.parse::<Stack>().map(|stack| Self::from(&stack))
    }
}

/// `n` things, with the noun in the plural unless `n` is 1: "1 service", "2 services".
struct Counted(usize, &'static str);

impl fmt::Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(n, noun) = self;
        write!(f, "{n} {noun}{}", if *n == 1 { "" } else { "s" })
    }
}

/// What applying a file would do, with the file itself for the confirm form.
#[derive(Template)]
#[template(path = "apply_preview.html")]
pub(super) struct PreviewView {
    stack: String,
    /// What the apply does, in plan order: the stack, its services, its routes.
    steps: Vec<Step>,
    /// The order the redeploy starts the services in.
    order: Vec<OrderLine>,
    /// The pre-flight checks the file passed.
    checks: Vec<String>,
    warnings: Vec<Problem>,
    pins: Vec<PinView>,
    changed: Vec<String>,
    /// What the diff compares the file to: "revision N" or "a new stack".
    against: String,
    diff: FileDiff,
    toml: String,
}

impl PreviewView {
    pub(super) fn new(preview: ApplyPreview, toml: String) -> Self {
        let current = preview.current.as_ref();
        let old = current.map_or("", |r| r.toml.as_str());
        let before = match current {
            Some(_) => old.parse::<Shape>().ok(),
            None => Some(Shape::default()),
        };
        let after = toml.parse::<Shape>().ok();

        let lines: Vec<&str> = toml.lines().collect();
        let warnings: Vec<Problem> = preview
            .warnings
            .iter()
            .map(|w| {
                let mut problem = Problem::from(w);
                problem.quote(&lines);
                problem
            })
            .collect();

        let stack_url = StackView::url(&preview.stack);
        let mut steps = vec![match current {
            Some(revision) => {
                let hash = revision.hash.to_string();
                Step::new(
                    Mark::Change,
                    "replace",
                    format!("{} revision {}", preview.stack, revision.revision),
                )
                .linked(stack_url)
                .noted(hash.get(..12).unwrap_or(&hash).to_owned())
            }
            None => Step::new(Mark::Add, "create stack", preview.stack.clone()),
        }];
        steps.extend(Step::services(
            &preview.changed,
            before.as_ref().map(|s| &s.services),
            after.as_ref().map(|s| &s.services),
        ));
        if let (Some(before), Some(after)) = (&before, &after) {
            steps.extend(Step::routes(&before.routes, &after.routes));
        }

        let pins: Vec<PinView> = preview.pins.iter().map(PinView::from).collect();
        let mut checks = Vec::new();
        match &after {
            Some(after) => {
                checks.push(format!(
                    "stack file valid · {}",
                    Counted(after.services.len(), "service")
                ));
                if !after.routes.is_empty() {
                    checks.push(format!(
                        "{}, none taken by hoster or another stack",
                        Counted(after.routes.len(), "route")
                    ));
                }
            }
            None => checks.push("stack file valid".to_owned()),
        }
        if !pins.is_empty() {
            checks.push(format!(
                "{} resolved to a digest",
                Counted(pins.len(), "third-party tag")
            ));
        }

        Self {
            steps,
            order: preview.start_order.iter().map(OrderLine::from).collect(),
            checks,
            warnings,
            pins,
            changed: preview.changed,
            against: current.map_or_else(
                || "a new stack".to_owned(),
                |r| format!("against revision {}", r.revision),
            ),
            diff: FileDiff::between(old, &toml),
            stack: preview.stack,
            toml,
        }
    }
}

/// Which step of applying failed: the check (`POST /apply/preview`) or the apply itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attempt {
    Check,
    Apply,
}

/// A check or apply that failed, for the aside: the problems in a fail-tinted card. A failed
/// check also clears the diff of an earlier check, which no longer matches the editor.
#[derive(Template)]
#[template(path = "apply_failed.html")]
pub(super) struct ApplyFailed {
    attempt: Attempt,
    outcome: Outcome,
}

impl ApplyFailed {
    pub(super) fn new(attempt: Attempt, outcome: Outcome) -> Self {
        Self { attempt, outcome }
    }

    fn title(&self) -> &'static str {
        match self.attempt {
            Attempt::Check => "Pre-flight",
            Attempt::Apply => "Not applied",
        }
    }

    fn clears_diff(&self) -> bool {
        self.attempt == Attempt::Check
    }
}

impl IntoResponse for ApplyFailed {
    fn into_response(self) -> Response {
        (self.outcome.status, Page(self)).into_response()
    }
}

/// A third-party service's pinned digest, before and after.
#[derive(Debug)]
struct PinView {
    service: String,
    before: Option<String>,
    after: String,
    changes: bool,
}

impl PinView {
    fn mark(&self) -> Mark {
        match (&self.before, self.changes) {
            (None, _) => Mark::Add,
            (Some(_), true) => Mark::Change,
            (Some(_), false) => Mark::Keep,
        }
    }

    fn verb(&self) -> &'static str {
        if self.mark() == Mark::Change {
            "re-pin"
        } else {
            "pin"
        }
    }
}

impl From<&PinChange> for PinView {
    fn from(pin: &PinChange) -> Self {
        Self {
            service: pin.service.clone(),
            before: pin.before.clone(),
            after: pin.after.clone(),
            changes: pin.before.as_deref() != Some(pin.after.as_str()),
        }
    }
}

/// What "re-resolve tags" would do: the pins before and after, and what moves.
#[derive(Template)]
#[template(path = "reresolve_preview.html")]
pub(super) struct ReresolveView {
    stack: String,
    confirm_url: String,
    /// The revision being re-resolved, which the confirmation names.
    revision: u32,
    pins: Vec<PinView>,
    /// The services the confirmation redeploys.
    steps: Vec<Step>,
    changed: Vec<String>,
    /// Whether any digest differs from the active revision's.
    moves: bool,
}

impl ReresolveView {
    pub(super) fn new(preview: ApplyPreview, revision: u32) -> Self {
        let pins: Vec<PinView> = preview.pins.iter().map(PinView::from).collect();
        Self {
            confirm_url: format!("{}/reresolve", StackView::url(&preview.stack)),
            moves: pins.iter().any(|p| p.changes),
            pins,
            steps: Step::services(&preview.changed, None, None),
            changed: preview.changed,
            stack: preview.stack,
            revision,
        }
    }
}

/// A line diff between the active revision and the new file, in hunks with context.
#[derive(Debug, Default)]
struct FileDiff {
    hunks: Vec<Vec<DiffLine>>,
}

#[derive(Debug, PartialEq, Eq)]
struct DiffLine {
    /// `add`, `remove` or `same`: the CSS class suffix.
    kind: &'static str,
    /// `+`, `-` or a space.
    sign: char,
    old: Option<usize>,
    new: Option<usize>,
    text: String,
}

impl FileDiff {
    /// Unchanged lines shown around each change.
    const CONTEXT: usize = 3;

    fn between(old: &str, new: &str) -> Self {
        let diff = TextDiff::from_lines(old, new);
        let hunks = diff
            .grouped_ops(Self::CONTEXT)
            .iter()
            .map(|group| {
                group
                    .iter()
                    .flat_map(|op| diff.iter_changes(op))
                    .map(|change| {
                        let (kind, sign) = match change.tag() {
                            ChangeTag::Insert => ("add", '+'),
                            ChangeTag::Delete => ("remove", '-'),
                            ChangeTag::Equal => ("same", ' '),
                        };
                        DiffLine {
                            kind,
                            sign,
                            old: change.old_index().map(|i| i + 1),
                            new: change.new_index().map(|i| i + 1),
                            text: change.value().trim_end_matches(['\n', '\r']).to_owned(),
                        }
                    })
                    .collect()
            })
            .collect();
        Self { hunks }
    }

    fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    #[test]
    fn services_are_new_redeployed_or_removed() {
        let before = names(&["api", "old"]);
        let after = names(&["api", "web"]);
        let changed = ["api".into(), "web".into(), "old".into()];
        let steps = Step::services(&changed, Some(&before), Some(&after));
        let marks: Vec<_> = steps.iter().map(|s| (s.mark, s.verb)).collect();
        assert_eq!(
            marks,
            [
                (Mark::Change, "redeploy"),
                (Mark::Add, "deploy"),
                (Mark::Remove, "remove"),
            ]
        );
        // A changed service in neither file runs but leaves the stack: a removal.
        let gone = Step::services(&["stale".into()], Some(&before), Some(&after));
        assert_eq!((gone[0].mark, gone[0].verb), (Mark::Remove, "remove"));
        // Without the service sets, every change is a redeploy.
        assert!(
            Step::services(&changed, None, None)
                .iter()
                .all(|s| s.mark == Mark::Change)
        );
        // Nothing changes: one line that says so.
        let none = Step::services(&[], Some(&before), Some(&after));
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].mark, Mark::Keep);
    }

    #[test]
    fn routes_that_appear_or_go_away() {
        let before = names(&["a.example/ → api:80", "b.example/ → api:80"]);
        let after = names(&["a.example/ → api:80", "c.example/ → web:80"]);
        let steps = Step::routes(&before, &after);
        assert_eq!(
            steps,
            [
                Step::new(Mark::Add, "route", "c.example/ → web:80"),
                Step::new(Mark::Remove, "route", "b.example/ → api:80"),
            ]
        );
    }

    #[test]
    fn file_diff_has_hunks_with_line_numbers_and_context() {
        let old = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
        let new = old
            .replace("line 2\n", "line two\n")
            .replace("line 18\n", "");
        let diff = FileDiff::between(&old, &new);
        assert_eq!(diff.hunks.len(), 2, "{diff:?}");
        let first = &diff.hunks[0];
        assert_eq!(
            first
                .iter()
                .filter(|l| l.kind != "same")
                .map(|l| (l.sign, l.old, l.new, l.text.as_str()))
                .collect::<Vec<_>>(),
            [
                ('-', Some(2), None, "line 2"),
                ('+', None, Some(2), "line two"),
            ]
        );
        assert_eq!(
            first.last().unwrap().text,
            "line 5",
            "three lines of context"
        );
        assert!(FileDiff::between(&old, &old).is_empty());
        // A new stack: every line is added.
        let created = FileDiff::between("", "a\nb\n");
        assert!(created.hunks[0].iter().all(|l| l.sign == '+'));
    }

    #[test]
    fn order_lines_say_what_each_service_waits_for() {
        let name = |n: &str| n.parse::<crate::stack::Name>().unwrap();
        let line = |verb, services: &[&str], note: Option<&str>| OrderLine {
            verb,
            services: services.iter().map(|&s| s.to_owned()).collect(),
            note: note.map(ToOwned::to_owned),
        };
        let table = [
            (
                Wave::Start(BTreeMap::from([
                    (name("cache"), Vec::new()),
                    (name("db"), Vec::new()),
                ])),
                line("start", &["cache", "db"], None),
            ),
            (
                Wave::Start(BTreeMap::from([
                    (name("app"), vec![name("cache"), name("db")]),
                    (name("mail"), Vec::new()),
                    (name("worker"), vec![name("db")]),
                ])),
                line(
                    "start",
                    &["app", "mail", "worker"],
                    Some("app after cache, db; worker after db"),
                ),
            ),
            (
                Wave::Replace(name("db")),
                line(
                    "replace",
                    &["db"],
                    Some("stop-first: down until the new one passes"),
                ),
            ),
            (
                Wave::HeldBack {
                    service: name("app"),
                    behind: vec![name("db")],
                },
                line(
                    "start",
                    &["app"],
                    Some("held back until db is replaced and passes"),
                ),
            ),
            (
                Wave::HeldBack {
                    service: name("app"),
                    behind: vec![name("cache"), name("db")],
                },
                line(
                    "start",
                    &["app"],
                    Some("held back until cache, db are replaced and pass"),
                ),
            ),
        ];
        for (wave, expected) in table {
            assert_eq!(OrderLine::from(&wave), expected, "{wave:?}");
        }
    }

    #[test]
    fn counted_pluralises() {
        assert_eq!(Counted(1, "service").to_string(), "1 service");
        assert_eq!(Counted(3, "route").to_string(), "3 routes");
    }
}
