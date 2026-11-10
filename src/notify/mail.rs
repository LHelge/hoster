//! The notification mail itself (ADR-0058 §10): an event turned into a subject, a plain-text
//! part and an HTML part that reads like the admin UI's event row.
//!
//! A [`Notice`] says what the mail is about (an event, a reminder of one, the hourly cap's
//! summary, or the test message); a [`Letterhead`] says who sends it (the instance name and the
//! admin origin the button links to). [`Mail::try_from`] renders the pair with the
//! `mail_notice.html` and `mail_notice.txt` templates, and [`Mail::into_message`] addresses the
//! result for a [`Provider`](super::Provider).
//!
//! Mail clients drop `<style>` blocks, web fonts, SVG and remote images, so the HTML uses a table
//! layout with inline styles only, the system font stacks, and the tone as a coloured cell. Its
//! colours are a copy of the light (Latte) tokens of `assets/hoster.css` in [`Tokens::LATTE`];
//! the copies are listed in `site/theme/static/css/theme.css`. There is no dark-mode variant:
//! clients that invert mail do it themselves.
//!
//! Every value from an event (messages carry image names and error text from outside) is
//! interpolated by Askama, which escapes it in the HTML part.

use std::time::SystemTime;

use askama::Template;
use reqwest::Url;

use super::{IdempotencyKey, MailAddress, Message};
use crate::instance::InstanceName;
use crate::store::{Event, Severity, Timestamp};

/// The longest subject, in characters, before it is cut with `…`.
const MAX_SUBJECT_CHARS: usize = 200;

/// The design tokens a mail uses: a copy of the Latte (light) values in `assets/hoster.css`
/// (ADR-0054). The four status tints are the status colour at 10 % over `surface`, worked out
/// here because mail clients do not know `color-mix`; the mail uses no accent tint.
///
/// Keep these in step with `assets/hoster.css`; `site/theme/static/css/theme.css` lists every
/// copy of the tokens.
#[derive(Debug)]
struct Tokens {
    bg: &'static str,
    surface: &'static str,
    border: &'static str,
    text: &'static str,
    muted: &'static str,
    accent: &'static str,
    on_accent: &'static str,
    ok: &'static str,
    progress: &'static str,
    warn: &'static str,
    fail: &'static str,
    ok_tint: &'static str,
    progress_tint: &'static str,
    warn_tint: &'static str,
    fail_tint: &'static str,
    sans: &'static str,
    mono: &'static str,
}

impl Tokens {
    /// Catppuccin Latte, with the hues darkened to 4.5:1 as in `assets/hoster.css`.
    const LATTE: Tokens = Tokens {
        bg: "#e6e9ef",
        surface: "#eff1f5",
        border: "#ccd0da",
        text: "#4c4f69",
        muted: "#5c5f77",
        accent: "#7420e3",
        on_accent: "#ffffff",
        ok: "#2c6e1d",
        progress: "#0a52e0",
        warn: "#a53d01",
        fail: "#ba0d33",
        ok_tint: "#dce4df",
        progress_tint: "#d8e1f3",
        warn_tint: "#e8dfdd",
        fail_tint: "#eadae2",
        // The admin UI's stacks without the vendored JetBrains Mono, which a mail cannot load.
        // No family needs quotes, so the stacks sit in a `style` attribute as they are.
        sans: "system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial, sans-serif",
        mono: "ui-monospace, SFMono-Regular, Menlo, Consolas, Liberation Mono, monospace",
    };
}

/// Who sends a notification: the instance name for the subject and footer (ADR-0058 §2), and
/// the admin origin (`https://<admin_host>`) its button links to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Letterhead {
    instance: InstanceName,
    admin: Url,
}

impl Letterhead {
    /// The letterhead of `instance`, linking to pages under `admin` (the admin origin).
    pub fn new(instance: InstanceName, admin: Url) -> Self {
        Self { instance, admin }
    }

    /// `path` (a path on the admin host, as an event's subject names one) as a link on the admin
    /// host; `None` for anything that would lead to another origin.
    fn page(&self, path: &str) -> Option<Url> {
        if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
            return None;
        }
        let url = self.admin.join(path).ok()?;
        (url.origin() == self.admin.origin()).then_some(url)
    }

    /// The overview of `stack` on the admin host.
    fn stack(&self, stack: &str) -> Url {
        let mut url = self.admin.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.clear().push("stacks").push(stack);
        }
        url
    }

    /// The Events page on the admin host (`/activity`; `/events` is the dashboard's stream).
    fn events(&self) -> Url {
        self.page("/activity").unwrap_or_else(|| self.admin.clone())
    }
}

/// What a notification mail is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice<'a> {
    /// An event as it was recorded: a problem or a recovery, as its severity says.
    Event(&'a Event),
    /// A problem that is still open a day after its last mail (ADR-0058 §8).
    Reminder(&'a Event),
    /// The hourly cap's summary: `more` events since it was reached went unmailed (§8).
    Summary { more: u32, at: Timestamp },
    /// The fixed message of Settings' Send test button (§5).
    Test { at: Timestamp },
}

/// A recorded event mails as itself.
impl<'a> From<&'a Event> for Notice<'a> {
    fn from(event: &'a Event) -> Self {
        Notice::Event(event)
    }
}

/// A rendered notification: the parts of a [`Message`] that do not depend on its addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    /// `[<instance name>] <the readable line>`, on one line.
    pub subject: String,
    pub text: String,
    pub html: String,
}

impl Mail {
    /// The mail as a message from `from` to `to`, sent under `idempotency_key`.
    pub fn into_message(
        self,
        from: MailAddress,
        to: MailAddress,
        idempotency_key: IdempotencyKey,
    ) -> Message {
        Message {
            from,
            to,
            subject: self.subject,
            text: self.text,
            html: self.html,
            idempotency_key,
        }
    }
}

/// Renders `notice` under `letterhead`.
impl TryFrom<(&Letterhead, Notice<'_>)> for Mail {
    type Error = askama::Error;

    fn try_from((letterhead, notice): (&Letterhead, Notice<'_>)) -> Result<Self, Self::Error> {
        let content = Content::new(letterhead, notice);
        Ok(Mail {
            subject: content.subject(),
            text: TextMail { m: &content }.render()?,
            html: HtmlMail {
                m: &content,
                t: &Tokens::LATTE,
            }
            .render()?,
        })
    }
}

/// The tone of a mail: the event row's dot, as a coloured cell and label colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailTone {
    Ok,
    Progress,
    Warn,
    Fail,
}

impl From<Severity> for MailTone {
    /// As the event feed's dot: informational events in blue (`src/web/events.rs`).
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Info => MailTone::Progress,
            Severity::Success => MailTone::Ok,
            Severity::Warning => MailTone::Warn,
            Severity::Error => MailTone::Fail,
        }
    }
}

impl MailTone {
    fn colour(self, t: &Tokens) -> &'static str {
        match self {
            MailTone::Ok => t.ok,
            MailTone::Progress => t.progress,
            MailTone::Warn => t.warn,
            MailTone::Fail => t.fail,
        }
    }

    fn tint(self, t: &Tokens) -> &'static str {
        match self {
            MailTone::Ok => t.ok_tint,
            MailTone::Progress => t.progress_tint,
            MailTone::Warn => t.warn_tint,
            MailTone::Fail => t.fail_tint,
        }
    }
}

/// What both parts of a mail show, worked out once from the notice.
#[derive(Debug)]
struct Content {
    instance: String,
    /// The admin host (with a port if it has one), for the footer.
    admin_host: String,
    tone: MailTone,
    /// The small label above the message: the severity, `still failing`, …
    label: String,
    /// The readable line, as the UI shows it.
    message: String,
    /// The short mono line: sha, digest, exit code, version.
    meta: Option<String>,
    /// A sentence under the event that explains the mail, for a reminder, summary or test.
    note: Option<&'static str>,
    /// `2026-11-10 14:03 UTC`.
    at: String,
    /// The button's target on the admin host, and its text.
    link: Url,
    link_label: String,
}

impl Content {
    fn new(letterhead: &Letterhead, notice: Notice<'_>) -> Self {
        let base = |tone, label: &str, message: String, at, link, link_label: String| Content {
            instance: letterhead.instance.to_string(),
            admin_host: letterhead.admin.authority().to_owned(),
            tone,
            label: label.to_owned(),
            message,
            meta: None,
            note: None,
            at: Self::when(at),
            link,
            link_label,
        };
        match notice {
            Notice::Event(event) => Self::of_event(letterhead, event, base),
            Notice::Reminder(event) => Content {
                label: "still failing".to_owned(),
                note: Some("This is still failing a day after the last mail about it."),
                ..Self::of_event(letterhead, event, base)
            },
            Notice::Summary { more, at } => Content {
                note: Some(
                    "hoster sends at most 20 notification mails an hour. \
                     The Events page has every event.",
                ),
                ..base(
                    MailTone::Warn,
                    "hourly cap",
                    match more {
                        1 => "1 more event since the last mail".to_owned(),
                        n => format!("{n} more events since the last mail"),
                    },
                    at,
                    letterhead.events(),
                    "Open the Events page".to_owned(),
                )
            },
            Notice::Test { at } => Content {
                note: Some(
                    "This is the test message from Settings → Notifications. \
                     Problems and recoveries will reach this address.",
                ),
                ..base(
                    MailTone::Ok,
                    "test",
                    "Notifications from hoster reach this address".to_owned(),
                    at,
                    letterhead
                        .page("/settings/notifications")
                        .unwrap_or_else(|| letterhead.admin.clone()),
                    "Open notification settings".to_owned(),
                )
            },
        }
    }

    /// The content of `event`, linked to the page it names (as its feed row is), else to its
    /// stack, else to the Events page.
    fn of_event(
        letterhead: &Letterhead,
        event: &Event,
        base: impl FnOnce(MailTone, &str, String, Timestamp, Url, String) -> Content,
    ) -> Content {
        let (link, link_label) = match (
            event
                .subject
                .as_deref()
                .and_then(|path| letterhead.page(path)),
            event.stack.as_deref(),
        ) {
            (Some(page), _) => (page, "Open in hoster".to_owned()),
            (None, Some(stack)) => (letterhead.stack(stack), format!("Open {stack}")),
            (None, None) => (letterhead.events(), "Open the Events page".to_owned()),
        };
        Content {
            meta: event
                .meta
                .as_deref()
                .filter(|meta| !meta.is_empty())
                .map(str::to_owned),
            ..base(
                MailTone::from(event.severity),
                event.severity.as_str(),
                event.message.clone(),
                event.at,
                link,
                link_label,
            )
        }
    }

    /// `at` to the minute, in UTC: `2026-11-10 14:03 UTC`.
    fn when(at: Timestamp) -> String {
        let rfc3339 = humantime::format_rfc3339_seconds(SystemTime::from(at)).to_string();
        // `2026-11-10T14:03:27Z`
        match rfc3339.get(..16) {
            Some(minute) => format!("{} UTC", minute.replacen('T', " ", 1)),
            None => rfc3339,
        }
    }

    /// `[<instance>] <message>` on one line: line breaks and control characters become
    /// spaces, and a very long line is cut.
    fn subject(&self) -> String {
        let line = self
            .message
            .split(|c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ");
        let subject = format!("[{}] {line}", self.instance);
        if subject.chars().count() > MAX_SUBJECT_CHARS {
            let cut: String = subject.chars().take(MAX_SUBJECT_CHARS - 1).collect();
            format!("{}…", cut.trim_end())
        } else {
            subject
        }
    }
}

/// The HTML part.
#[derive(Template)]
#[template(path = "mail_notice.html")]
struct HtmlMail<'a> {
    m: &'a Content,
    t: &'a Tokens,
}

impl HtmlMail<'_> {
    fn tone(&self) -> &'static str {
        self.m.tone.colour(self.t)
    }

    fn tint(&self) -> &'static str {
        self.m.tone.tint(self.t)
    }
}

/// The plain-text part, with the same content.
#[derive(Template)]
#[template(path = "mail_notice.txt")]
struct TextMail<'a> {
    m: &'a Content,
}

#[cfg(test)]
pub(crate) mod tests;
