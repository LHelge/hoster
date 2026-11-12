use std::time::Duration;

use axum::http::StatusCode;

use super::*;
use crate::runtime::FakeRuntime;
use crate::stats::ResourceSampler;
use crate::store::Timestamp;
use crate::web::auth::tests::{TestApp, header};
use crate::web::stacks::tests::body;

const GIB: u64 = 1024 * 1024 * 1024;

fn sample(cpu: Option<f64>, memory: Option<u64>, disk: Option<u64>) -> HostSample {
    HostSample {
        at: Timestamp::now(),
        cpu,
        memory: memory.map(|used| Usage {
            used,
            total: 32 * GIB,
        }),
        disk: disk.map(|used| Usage {
            used,
            total: 1000 * GIB,
        }),
    }
}

/// A cache that recorded `samples`, oldest first, with no containers.
fn usage(samples: impl IntoIterator<Item = HostSample>) -> ResourceUsage {
    let usage = ResourceUsage::default();
    for sample in samples {
        usage.record(sample, None);
    }
    usage
}

fn card(usage: &ResourceUsage) -> String {
    HostCard::of("srv-01".parse().unwrap(), usage)
        .render()
        .unwrap()
}

/// The row of `label`, from its label to the end of its meter.
fn row<'a>(html: &'a str, label: &str) -> &'a str {
    let start = html
        .find(&format!(r#"<span class="host-meter-label">{label}</span>"#))
        .unwrap_or_else(|| panic!("no {label} row: {html}"));
    let rest = &html[start..];
    &rest[..rest.find("</svg>\n  </div>").unwrap()]
}

#[test]
fn the_card_shows_the_latest_figures_with_meters_toned_by_load() {
    let usage = usage([
        // The first tick has no CPU figure, and memory is unknown later on.
        sample(None, Some(10 * GIB), Some(400 * GIB)),
        sample(Some(23.0), None, Some(400 * GIB)),
        sample(Some(85.04), Some(11 * GIB + GIB / 5), Some(960 * GIB)),
    ]);
    let html = card(&usage);
    assert!(html.contains("HOST · srv-01"), "{html}");

    let cpu = row(&html, "cpu");
    assert!(
        cpu.contains(r#"<span class="host-meter-value">85%</span>"#),
        "{cpu}"
    );
    assert!(
        cpu.contains(
            r#"<svg class="meter meter-warn" role="img" aria-label="cpu 85% of all cores""#
        ),
        "{cpu}"
    );
    assert!(
        cpu.contains(r#"<rect class="meter-fill" width="85" "#),
        "{cpu}"
    );
    // Two known samples in tenths of a percent: 230 at the bottom-ish, 850 at the top.
    assert!(cpu.contains(r#"<svg class="spark spark-warn""#), "{cpu}");
    assert_eq!(
        Sparkline::new([230, 850], Tone::Warn).to_string(),
        cpu[cpu.find("<svg class=\"spark").unwrap()..]
            .split_inclusive("</svg>")
            .next()
            .unwrap()
    );

    let mem = row(&html, "mem");
    assert!(
        mem.contains(r#"<span class="host-meter-value">11.2 / 32.0 GiB</span>"#),
        "{mem}"
    );
    assert!(
        mem.contains(
            r#"<svg class="meter meter-ok" role="img" aria-label="mem 11.2 / 32.0 GiB used""#
        ),
        "{mem}"
    );
    assert!(
        mem.contains(r#"<rect class="meter-fill" width="35" "#),
        "{mem}"
    );
    // The unknown sample in between is left out of the line.
    assert!(
        mem.contains(&Sparkline::new([10 * GIB, 11 * GIB + GIB / 5], Tone::Ok).to_string()),
        "{mem}"
    );

    let disk = row(&html, "disk");
    assert!(
        disk.contains(r#"<span class="host-meter-value">960.0 / 1000.0 GiB</span>"#),
        "{disk}"
    );
    assert!(disk.contains(r#"<svg class="meter meter-fail""#), "{disk}");
    assert!(!disk.contains("spark"), "disk has no sparkline: {disk}");
    assert!(!html.contains("style="), "no inline styles under the CSP");
}

#[test]
fn unknown_figures_read_as_a_dash_over_an_empty_meter() {
    // Before the first tick, and a tick that read nothing.
    for usage in [usage([]), usage([sample(None, None, None)])] {
        let html = card(&usage);
        for label in ["cpu", "mem", "disk"] {
            let row = row(&html, label);
            assert!(
                row.contains(r#"<span class="host-meter-value muted">—</span>"#),
                "{row}"
            );
            assert!(
                row.contains(&format!(
                    r#"<svg class="meter meter-neutral" role="img" aria-label="{label} unknown""#
                )),
                "{row}"
            );
            assert!(
                row.contains(r#"<rect class="meter-fill" width="0" "#),
                "{row}"
            );
            assert!(!row.contains("spark"), "{row}");
        }
    }
}

#[test]
fn the_card_polls_for_itself_once_per_sampler_tick() {
    let html = card(&usage([]));
    assert!(
        html.starts_with(
            r#"<section class="host-card" id="host-card" aria-labelledby="host-card-title" hx-get="/shell/host" hx-trigger="every 10s" hx-swap="outerHTML">"#
        ),
        "{html}"
    );
    assert_eq!(
        ResourceSampler::<FakeRuntime>::INTERVAL,
        Duration::from_secs(10),
        "HostCard::POLL follows the sampler's interval"
    );
    assert_eq!(HostCard::POLL, "every 10s");
}

#[tokio::test]
async fn every_signed_in_page_shows_the_card_and_public_ones_do_not() {
    let app = TestApp::new().await;
    app.resources
        .record(sample(Some(42.0), Some(8 * GIB), Some(500 * GIB)), None);
    let browser = app.sign_in().await;
    for uri in [
        "/",
        "/stacks",
        "/certificates",
        "/settings/general",
        "/system",
        "/activity",
        "/stacks/notes/logs",
        "/stacks/no-such-stack",
    ] {
        let html = body(app.get(uri, Some(&browser.cookie)).await).await;
        assert_eq!(
            html.matches(r#"<section class="host-card" id="host-card""#)
                .count(),
            1,
            "{uri}: {html}"
        );
        assert!(
            html.contains(r#"<span class="host-meter-value">42%</span>"#),
            "{uri}: {html}"
        );
        // Between the navigation and the account block.
        let card = html.find("host-card").unwrap();
        assert!(html.find(r#"<ul class="nav">"#).unwrap() < card, "{uri}");
        assert!(
            card < html.find(r#"<div class="account">"#).unwrap(),
            "{uri}"
        );
    }

    let login = body(app.get("/auth/login", None).await).await;
    assert!(login.contains("<main class=\"public\">"), "{login}");
    assert!(!login.contains("host-card"), "{login}");
    // A failed login's page, for a signed-out browser.
    let denied = body(
        app.get("/auth/github/callback?error=access_denied&state=x", None)
            .await,
    )
    .await;
    assert!(denied.contains("<main class=\"public\">"), "{denied}");
    assert!(!denied.contains("host-card"), "{denied}");
}

#[tokio::test]
async fn the_fragment_is_the_card_alone_and_needs_a_session() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get(HostCard::PATH, Some(&browser.cookie)).await).await;
    assert!(html.starts_with(r#"<section class="host-card""#), "{html}");
    assert!(html.trim_end().ends_with("</section>"), "{html}");
    assert!(!html.contains("<html") && !html.contains("<nav"), "{html}");
    assert_eq!(html.matches("—").count(), 3, "nothing sampled yet: {html}");

    // A tick later, the poll sees it without anything else changing.
    app.resources
        .record(sample(Some(5.26), Some(GIB), Some(GIB)), None);
    let html = body(app.get(HostCard::PATH, Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<span class="host-meter-value">5.3%</span>"#),
        "{html}"
    );

    let response = app.get(HostCard::PATH, None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header(&response, "location"), "/auth/login");
}
