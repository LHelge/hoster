//! The admin UI's small rendered components (ADR-0054): the [`Tone`] every status maps into
//! (deployment states, health, [`CertState`] here; DNS verdicts and warnings where they are
//! defined), the [`Pill`] that shows one, and the server-drawn [`Meter`] bar. Each renders its
//! own markup in Askama templates (`HtmlSafe`, like [`Icon`]) and is coloured by a class whose
//! colours are the role tokens in `assets/hoster.css`, so no page names a palette colour and
//! nothing needs an inline `style=` under the CSP.

use std::fmt;
use std::time::Duration;

use askama::filters::{Html, escape};

use super::icons::Icon;
use crate::health::HealthStatus;
use crate::store::DeploymentState;

/// How a status reads at a glance; its `Display` is the CSS class suffix (`pill-ok`,
/// `meter-warn`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// Healthy, live, valid.
    Ok,
    /// Under way: deploying, issuing, renewing.
    Progress,
    /// Needs a look, but still working: degraded, expiring, a DNS mismatch.
    Warn,
    /// Broken: failed, rejected, crash looping.
    Fail,
    /// No judgement: unknown, not checked, superseded.
    #[default]
    Neutral,
}

impl Tone {
    /// The icon a pill of this tone shows.
    fn icon(self) -> Option<Icon> {
        match self {
            Tone::Ok => Some(Icon::Ok),
            Tone::Progress => Some(Icon::Pending),
            Tone::Warn => Some(Icon::Warning),
            Tone::Fail => Some(Icon::Failed),
            Tone::Neutral => None,
        }
    }

    /// A pill of this tone saying `text`.
    pub fn pill(self, text: impl fmt::Display) -> Pill {
        Pill {
            tone: self,
            text: text.to_string(),
            title: None,
        }
    }
}

impl fmt::Display for Tone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tone::Ok => "ok",
            Tone::Progress => "progress",
            Tone::Warn => "warn",
            Tone::Fail => "fail",
            Tone::Neutral => "neutral",
        })
    }
}

impl From<DeploymentState> for Tone {
    fn from(state: DeploymentState) -> Self {
        use DeploymentState as S;
        match state {
            S::Live => Tone::Ok,
            S::LiveWithWarnings => Tone::Warn,
            S::Failed | S::Rejected => Tone::Fail,
            S::Noop | S::Superseded => Tone::Neutral,
            S::Received
            | S::Queued
            | S::Validating
            | S::Pulling
            | S::Starting
            | S::HealthChecking
            | S::Replacing
            | S::Switching
            | S::Draining
            | S::CleaningUp
            | S::Aborting => Tone::Progress,
        }
    }
}

impl From<HealthStatus> for Tone {
    fn from(status: HealthStatus) -> Self {
        match status {
            HealthStatus::Healthy => Tone::Ok,
            HealthStatus::Unhealthy | HealthStatus::CrashLooping => Tone::Fail,
            HealthStatus::Unknown => Tone::Neutral,
        }
    }
}

/// Where a host's certificate stands, as the certificate cells show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertState {
    /// None is served yet, and no attempt has failed.
    Pending,
    /// None is served yet, and the last attempt failed.
    Failing,
    /// Served, with more than [`CertState::WARN_WITHIN`] left.
    Valid,
    /// Served, and expiring within [`CertState::WARN_WITHIN`].
    Expiring,
    /// Served, but no longer valid.
    Expired,
}

impl CertState {
    /// Certificates closer to expiry than this are flagged (renewal starts well before).
    pub const WARN_WITHIN: Duration = Duration::from_secs(14 * 24 * 60 * 60);

    /// A host without a certificate, failing when an attempt reported `error`.
    pub fn unissued(error: Option<&str>) -> Self {
        if error.is_some() {
            CertState::Failing
        } else {
            CertState::Pending
        }
    }

    /// Whether the certificate needs the operator's attention soon.
    pub fn is_flagged(self) -> bool {
        matches!(self, CertState::Expiring | CertState::Expired)
    }

    /// `text` as a pill in this state's tone; `None` for a valid certificate, whose cell is
    /// plain text.
    pub fn pill(self, text: impl fmt::Display) -> Option<Pill> {
        (self != CertState::Valid).then(|| Tone::from(self).pill(text))
    }
}

/// The state of a served certificate with `left` of its validity remaining.
impl From<Duration> for CertState {
    fn from(left: Duration) -> Self {
        if left.is_zero() {
            CertState::Expired
        } else if left < Self::WARN_WITHIN {
            CertState::Expiring
        } else {
            CertState::Valid
        }
    }
}

impl From<CertState> for Tone {
    fn from(state: CertState) -> Self {
        match state {
            CertState::Pending => Tone::Progress,
            CertState::Failing | CertState::Expired => Tone::Fail,
            CertState::Expiring => Tone::Warn,
            CertState::Valid => Tone::Ok,
        }
    }
}

/// A status pill: an icon and a short text in the tone's colour, on its tint. Build one with
/// [`Tone::pill`], or from a state that has a tone (`Pill::from(DeploymentState::Live)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    tone: Tone,
    text: String,
    /// A tooltip with the detail behind the status.
    title: Option<String>,
}

impl Pill {
    /// Texts longer than this many characters (a stack's failed-deploy flag) may wrap; shorter
    /// ones never break inside a table cell.
    const LONG: usize = 24;

    /// The same pill with a tooltip.
    pub fn title(mut self, title: impl fmt::Display) -> Self {
        self.title = Some(title.to_string());
        self
    }
}

impl From<DeploymentState> for Pill {
    fn from(state: DeploymentState) -> Self {
        Tone::from(state).pill(state)
    }
}

impl From<HealthStatus> for Pill {
    fn from(status: HealthStatus) -> Self {
        Tone::from(status).pill(status)
    }
}

/// The markup; the text and title are escaped here, since templates insert it unescaped.
impl fmt::Display for Pill {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let long = if self.text.chars().count() > Self::LONG {
            " pill-long"
        } else {
            ""
        };
        write!(f, r#"<span class="pill pill-{}{long}""#, self.tone)?;
        if let Some(title) = &self.title {
            write!(f, r#" title="{}""#, Escaped(title))?;
        }
        f.write_str(">")?;
        if let Some(icon) = self.tone.icon() {
            write!(f, "{icon}")?;
        }
        write!(f, "{}</span>", Escaped(&self.text))
    }
}

/// Every piece of user-derived text in the markup is escaped by `Display`.
impl askama::filters::HtmlSafe for Pill {}

/// A horizontal bar for a value out of a maximum (certificate days left, a route's share of the
/// slowest requests), drawn on the server as an SVG whose geometry is attributes: the CSP allows
/// no inline styles, so the filled width cannot be a CSS value. The bar stretches to the width
/// CSS gives `.meter`; the tone colours the fill.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    value: f64,
    max: f64,
    tone: Tone,
    /// What the bar shows, for assistive technology (`12 of 90 days left`).
    label: String,
}

impl Meter {
    /// The viewBox width: the fill is drawn in hundredths of the track.
    const WIDTH: f64 = 100.0;
    /// The viewBox and default rendered height, in px.
    const HEIGHT: u32 = 6;

    /// A meter of `value` out of `max`, labelled for screen readers. A value outside `0..=max`
    /// draws as an empty or full bar, and so does any value when `max` is not positive.
    pub fn new(value: f64, max: f64, tone: Tone, label: impl fmt::Display) -> Self {
        Self {
            value,
            max,
            tone,
            label: label.to_string(),
        }
    }

    /// The share of the track that is filled, from 0 to 1.
    fn fraction(&self) -> f64 {
        if self.max > 0.0 && self.value.is_finite() {
            (self.value / self.max).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// The filled width in viewBox units, to one decimal.
    fn fill_width(&self) -> f64 {
        (self.fraction() * Self::WIDTH * 10.0).round() / 10.0
    }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = (Self::WIDTH, Self::HEIGHT);
        write!(
            f,
            r#"<svg class="meter meter-{tone}" role="img" aria-label="{label}" width="{w}" height="{h}" viewBox="0 0 {w} {h}" preserveAspectRatio="none"><rect class="meter-track" width="{w}" height="{h}"></rect><rect class="meter-fill" width="{fill}" height="{h}"></rect></svg>"#,
            tone = self.tone,
            label = Escaped(&self.label),
            fill = self.fill_width(),
        )
    }
}

/// The label is escaped by `Display`; everything else is numbers.
impl askama::filters::HtmlSafe for Meter {}

/// A KPI tile: a small upper-case label, a large figure and a line of detail, the figure in the
/// tone's colour when the tone is not [`Tone::Neutral`]. Tiles go in a `<dl class="kpis">`;
/// each is a `<div>` holding the label as `<dt>` and the figure and detail as `<dd>`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kpi {
    label: String,
    value: String,
    sub: String,
    tone: Tone,
    /// A tooltip saying what the figure counts.
    title: Option<String>,
}

impl Kpi {
    /// A neutral tile.
    pub fn new(label: impl fmt::Display, value: impl fmt::Display, sub: impl fmt::Display) -> Self {
        Self {
            label: label.to_string(),
            value: value.to_string(),
            sub: sub.to_string(),
            tone: Tone::Neutral,
            title: None,
        }
    }

    /// The same tile in `tone`.
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// The same tile with a tooltip.
    pub fn title(mut self, title: impl fmt::Display) -> Self {
        self.title = Some(title.to_string());
        self
    }
}

/// The markup; every text is escaped here, since templates insert it unescaped.
impl fmt::Display for Kpi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, r#"<div class="kpi kpi-{}""#, self.tone)?;
        if let Some(title) = &self.title {
            write!(f, r#" title="{}""#, Escaped(title))?;
        }
        write!(
            f,
            r#"><dt class="kpi-label">{}</dt><dd class="kpi-value">{}</dd><dd class="kpi-sub">{}</dd></div>"#,
            Escaped(&self.label),
            Escaped(&self.value),
            Escaped(&self.sub),
        )
    }
}

impl askama::filters::HtmlSafe for Kpi {}

/// A count with its thousands grouped by commas, as the UI writes large numbers: `595,904`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count(pub u64);

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.0.to_string();
        for (i, digit) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                f.write_str(",")?;
            }
            write!(f, "{digit}")?;
        }
        Ok(())
    }
}

/// Text HTML-escaped the way Askama escapes template values.
struct Escaped<'a>(&'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match escape(self.0, Html) {
            Ok(safe) => write!(f, "{safe}"),
            Err(never) => match never {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_map_to_tones() {
        assert_eq!(Tone::from(DeploymentState::Live), Tone::Ok);
        assert_eq!(Tone::from(DeploymentState::LiveWithWarnings), Tone::Warn);
        assert_eq!(Tone::from(DeploymentState::Failed), Tone::Fail);
        assert_eq!(Tone::from(DeploymentState::Rejected), Tone::Fail);
        assert_eq!(Tone::from(DeploymentState::Pulling), Tone::Progress);
        assert_eq!(Tone::from(DeploymentState::Superseded), Tone::Neutral);
        assert_eq!(Tone::from(HealthStatus::Healthy), Tone::Ok);
        assert_eq!(Tone::from(HealthStatus::CrashLooping), Tone::Fail);
        assert_eq!(Tone::from(HealthStatus::Unknown), Tone::Neutral);
    }

    #[test]
    fn certificates_map_to_tones() {
        let day = Duration::from_secs(24 * 60 * 60);
        assert_eq!(CertState::from(Duration::ZERO), CertState::Expired);
        assert_eq!(CertState::from(3 * day), CertState::Expiring);
        assert_eq!(CertState::from(60 * day), CertState::Valid);
        assert_eq!(CertState::unissued(None), CertState::Pending);
        assert_eq!(CertState::unissued(Some("boom")), CertState::Failing);
        assert_eq!(Tone::from(CertState::Pending), Tone::Progress);
        assert_eq!(Tone::from(CertState::Failing), Tone::Fail);
        assert_eq!(Tone::from(CertState::Expiring), Tone::Warn);
        assert_eq!(Tone::from(CertState::Expired), Tone::Fail);
        assert_eq!(CertState::Valid.pill("60 d"), None);
        assert!(CertState::Expiring.is_flagged() && !CertState::Valid.is_flagged());
    }

    #[test]
    fn a_pill_renders_its_tone_icon_and_escaped_text() {
        assert_eq!(
            Pill::from(DeploymentState::Failed).to_string(),
            format!(
                r#"<span class="pill pill-fail">{}Failed</span>"#,
                Icon::Failed
            )
        );
        assert_eq!(
            Tone::Warn
                .pill("<b>3 d</b>")
                .title(r#"a "quote""#)
                .to_string(),
            format!(
                r#"<span class="pill pill-warn" title="a &#34;quote&#34;">{}&#60;b&#62;3 d&#60;/b&#62;</span>"#,
                Icon::Warning
            )
        );
        assert_eq!(
            Tone::Warn
                .pill("revision 1: deploy failed: boom")
                .to_string(),
            format!(
                r#"<span class="pill pill-warn pill-long">{}revision 1: deploy failed: boom</span>"#,
                Icon::Warning
            )
        );
        assert_eq!(
            Tone::Neutral.pill("unknown").to_string(),
            r#"<span class="pill pill-neutral">unknown</span>"#
        );
    }

    #[test]
    fn a_kpi_renders_escaped_with_its_tone() {
        assert_eq!(
            Kpi::new("p95", "1.2 s", "<slowest>")
                .tone(Tone::Warn)
                .title("over 5 min")
                .to_string(),
            r#"<div class="kpi kpi-warn" title="over 5 min"><dt class="kpi-label">p95</dt><dd class="kpi-value">1.2 s</dd><dd class="kpi-sub">&#60;slowest&#62;</dd></div>"#
        );
        assert!(
            Kpi::new("a", "b", "c")
                .to_string()
                .starts_with(r#"<div class="kpi kpi-neutral">"#)
        );
    }

    #[test]
    fn counts_group_thousands() {
        for (n, text) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (595_904, "595,904"),
            (12_345_678, "12,345,678"),
        ] {
            assert_eq!(Count(n).to_string(), text);
        }
    }

    #[test]
    fn a_meter_draws_its_fraction_as_attributes() {
        let meter = Meter::new(12.0, 90.0, Tone::Warn, "12 of 90 days left");
        assert_eq!(
            meter.to_string(),
            r#"<svg class="meter meter-warn" role="img" aria-label="12 of 90 days left" width="100" height="6" viewBox="0 0 100 6" preserveAspectRatio="none"><rect class="meter-track" width="100" height="6"></rect><rect class="meter-fill" width="13.3" height="6"></rect></svg>"#
        );
        assert!(!meter.to_string().contains("style="));
    }

    #[test]
    fn a_meter_clamps_to_its_track() {
        let fill = |value, max| Meter::new(value, max, Tone::Ok, "").fill_width();
        assert_eq!(fill(150.0, 100.0), 100.0);
        assert_eq!(fill(-5.0, 100.0), 0.0);
        assert_eq!(fill(5.0, 0.0), 0.0);
        assert_eq!(fill(f64::NAN, 10.0), 0.0);
        assert_eq!(fill(1.0, 3.0), 33.3);
        assert!(
            Meter::new(1.0, 2.0, Tone::Ok, "<x>")
                .to_string()
                .contains(r#"aria-label="&#60;x&#62;""#)
        );
    }
}
