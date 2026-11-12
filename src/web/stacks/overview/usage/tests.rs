//! The usage cells rendered on their own, then the services table and its live stream through
//! the whole app against the fake runtime and a hand-driven sampler cache.

use std::time::Duration;

use axum::http::StatusCode;
use http_body_util::BodyExt as _;

use super::*;
use crate::runtime::ContainerId;
use crate::stats::{ContainerReading, HostSample};
use crate::store::Timestamp;
use crate::web::auth::tests::TestApp;
use crate::web::stacks::tests::{NOTES, body, notes_status, revision, start_container};

const MIB: u64 = 1024 * 1024;

/// [`NOTES`] with limits on api, which runs generation 2.
fn limited() -> String {
    format!("{NOTES}\n[services.api.limits]\nmemory = \"512MiB\"\ncpus = 1.5\npids = 64\n")
}

fn status(toml: &str) -> StackStatus {
    let mut status = notes_status();
    status.revision = revision("notes", 1, toml);
    status
}

fn sample(cpu: Option<f64>, memory: Option<u64>) -> ContainerSample {
    ContainerSample {
        at: Timestamp::now(),
        cpu,
        memory,
    }
}

fn usage(service: &str, generation: u32, samples: &[ContainerSample]) -> ContainerUsage {
    let cache = ResourceUsage::default();
    let id = ContainerId::from("x".to_owned());
    for sample in samples {
        cache.record(
            host(),
            Some(vec![reading(&id, service, generation, *sample)]),
        );
    }
    cache.stack("notes").remove(&id).unwrap()
}

fn reading(
    id: &ContainerId,
    service: &str,
    generation: u32,
    sample: ContainerSample,
) -> ContainerReading {
    ContainerReading {
        id: id.clone(),
        stack: "notes".into(),
        service: service.into(),
        generation: Some(generation),
        sample,
    }
}

fn host() -> HostSample {
    HostSample {
        at: Timestamp::now(),
        cpu: None,
        memory: None,
        disk: None,
    }
}

/// The cells of a container of `service` and `generation` with `samples`, in `status`.
fn cells(
    status: &StackStatus,
    service: &str,
    generation: u32,
    samples: &[ContainerSample],
) -> String {
    let limits = UsageLimits::of(status);
    let id = ContainerId::from("c0ffee".to_owned());
    let containers = if samples.is_empty() {
        BTreeMap::new()
    } else {
        BTreeMap::from([(id.clone(), usage(service, generation, samples))])
    };
    StackUsage::new(&limits, containers)
        .cells(&id, service, Some(generation))
        .render()
        .unwrap()
}

/// The cell with element id `id` in `html`.
fn cell<'a>(html: &'a str, id: &str) -> &'a str {
    let start = html
        .find(&format!(r#"id="{id}">"#))
        .unwrap_or_else(|| panic!("no cell {id} in {html}"));
    let end = start + html[start..].find("</td>").unwrap();
    &html[start..end]
}

#[test]
fn a_running_container_with_limits_shows_meters_against_them() {
    let html = cells(
        &status(&limited()),
        "api",
        2,
        &[
            sample(None, Some(300 * MIB)),
            sample(Some(20.0), Some(400 * MIB)),
            sample(Some(30.0), Some(420 * MIB)),
        ],
    );
    let cpu = cell(&html, "cpu-c0ffee");
    assert!(
        cpu.contains(r#"<span class="usage-figure"><span class="usage-value">30%</span> <span class="usage-limit">/ 150%</span></span>"#),
        "{cpu}"
    );
    assert!(
        cpu.contains(
            r#"<svg class="meter meter-ok" role="img" aria-label="cpu 30% of its 150% limit""#
        ),
        "{cpu}"
    );
    assert!(
        cpu.contains(r#"<rect class="meter-fill" width="20""#),
        "filled to 30/150: {cpu}"
    );
    // The sparkline of the hour leaves out the first, unknown figure (200 and 300 tenths).
    assert!(cpu.contains(r#"<svg class="spark spark-ok""#), "{cpu}");
    assert!(cpu.contains(r#"points="0,9.83 96,1.5""#), "{cpu}");

    // 420 of 512 MiB is past 80 %: a warning, with no sparkline beside the meter.
    let memory = cell(&html, "mem-c0ffee");
    assert!(
        memory.contains(
            r#"<span class="usage-value">420 MiB</span> <span class="usage-limit">/ 512 MiB</span>"#
        ),
        "{memory}"
    );
    assert!(
        memory.contains(r#"<svg class="meter meter-warn" role="img" aria-label="memory 420 MiB of its 512 MiB limit""#),
        "{memory}"
    );
    assert!(!memory.contains("spark"), "{memory}");
    assert!(!html.contains("style="), "{html}");

    // Past 95 % it is bad.
    let html = cells(
        &status(&limited()),
        "api",
        2,
        &[sample(Some(149.0), Some(500 * MIB))],
    );
    assert!(
        cell(&html, "cpu-c0ffee").contains("meter meter-fail"),
        "{html}"
    );
    assert!(
        cell(&html, "mem-c0ffee").contains("meter meter-fail"),
        "{html}"
    );
}

#[test]
fn without_limits_there_is_no_meter_and_the_lines_are_neutral() {
    let html = cells(
        &status(NOTES),
        "api",
        2,
        &[
            sample(Some(2.5), Some(3 * MIB)),
            sample(Some(0.4), Some(2 * MIB)),
        ],
    );
    let cpu = cell(&html, "cpu-c0ffee");
    assert!(
        cpu.contains(r#"<span class="usage-figure"><span class="usage-value">0.4%</span></span><svg class="spark spark-neutral""#),
        "{cpu}"
    );
    let memory = cell(&html, "mem-c0ffee");
    assert!(
        memory.contains(r#"<span class="usage-figure"><span class="usage-value">2 MiB</span></span><svg class="spark spark-neutral""#),
        "{memory}"
    );
    assert!(!html.contains("meter"), "{html}");
    assert!(!html.contains("usage-limit"), "{html}");
}

#[test]
fn unknown_figures_read_a_dash_over_an_empty_meter() {
    // Not sampled yet, against limits: a dash, the limit and an empty meter.
    let html = cells(&status(&limited()), "api", 2, &[]);
    let cpu = cell(&html, "cpu-c0ffee");
    assert!(
        cpu.contains(
            r#"<span class="usage-value muted">—</span> <span class="usage-limit">/ 150%</span>"#
        ),
        "{cpu}"
    );
    assert!(
        cpu.contains(r#"<svg class="meter meter-neutral" role="img" aria-label="cpu unknown""#),
        "{cpu}"
    );
    assert!(
        cpu.contains(r#"<rect class="meter-fill" width="0""#),
        "{cpu}"
    );
    assert!(!html.contains("spark"), "{html}");

    // Stopped after an hour of figures: a dash, and the line of what was known.
    let html = cells(
        &status(NOTES),
        "api",
        2,
        &[sample(Some(5.0), Some(8 * MIB)), sample(None, None)],
    );
    let memory = cell(&html, "mem-c0ffee");
    assert!(
        memory.contains(
            r#"<span class="usage-value muted">—</span></span><svg class="spark spark-neutral""#
        ),
        "{memory}"
    );
    assert!(!html.contains("meter"), "{html}");
}

#[test]
fn only_the_container_running_its_service_is_shown_against_the_limits() {
    let status = status(&limited());
    // A draining generation 1 and a starting generation 3 of api, and a container of a service
    // that runs nothing (storage), show their own figures without limits.
    for (service, generation) in [("api", 1), ("api", 3), ("storage", 1)] {
        let html = cells(
            &status,
            service,
            generation,
            &[sample(Some(12.0), Some(64 * MIB))],
        );
        assert!(
            html.contains(r#"<span class="usage-value">12%</span>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="usage-value">64 MiB</span>"#),
            "{html}"
        );
        assert!(!html.contains("meter"), "{service} g{generation}: {html}");
    }

    // The limits in effect are the committed revision's while a newer one is not live yet.
    let mut deploying = status.clone();
    deploying.committed = Some(revision("notes", 1, NOTES));
    let html = cells(&deploying, "api", 2, &[sample(Some(12.0), Some(64 * MIB))]);
    assert!(!html.contains("meter"), "{html}");
}

#[test]
fn partials_replace_each_cells_content_by_container_id() {
    let cache = ResourceUsage::default();
    let api = ContainerId::from("a1".to_owned());
    let old = ContainerId::from("b2".to_owned());
    let blog = ContainerId::from("c3".to_owned());
    let feed = UsageFeed::new(&status(&limited()), cache.clone());
    assert_eq!(feed.partials(), None, "the sampler has nothing yet");

    let mut other = reading(&blog, "web", 1, sample(Some(1.0), Some(MIB)));
    other.stack = "blog".into();
    cache.record(
        host(),
        Some(vec![
            reading(&api, "api", 2, sample(Some(42.0), Some(100 * MIB))),
            reading(&old, "api", 1, sample(None, None)),
            other,
        ]),
    );
    let html = feed.partials().unwrap();
    let starts: Vec<&str> = html
        .match_indices("<hx-partial")
        .map(|(at, _)| &html[at..at + html[at..].find('>').unwrap() + 1])
        .collect();
    assert_eq!(
        starts,
        [
            r##"<hx-partial hx-target="#cpu-a1" hx-swap="innerHTML">"##,
            r##"<hx-partial hx-target="#mem-a1" hx-swap="innerHTML">"##,
            r##"<hx-partial hx-target="#cpu-b2" hx-swap="innerHTML">"##,
            r##"<hx-partial hx-target="#mem-b2" hx-swap="innerHTML">"##,
        ],
        "only notes' containers: {html}"
    );
    assert_eq!(html.matches("</hx-partial>").count(), 4, "{html}");
    assert!(
        html.contains(
            r#"<span class="usage-value">42%</span> <span class="usage-limit">/ 150%</span>"#
        ),
        "{html}"
    );
    // The draining container: its own (unknown) figures, without the limits.
    let old_cells = &html[html.find("#cpu-b2").unwrap()..];
    assert!(!old_cells.contains("meter"), "{old_cells}");
    assert!(!html.contains("<td"), "the cells themselves stay: {html}");
}

#[test]
fn an_id_goes_into_the_element_ids_as_a_plain_selector() {
    let cells = UsageCells::new(
        &ContainerId::from("ab\"<c>.1".to_owned()),
        None,
        Limits::default(),
    );
    assert_eq!(cells.cpu_id(), "cpu-abc1");
    assert_eq!(cells.memory_id(), "mem-abc1");
}

/// The containers' cells on the page, and fresh ones over the stack's stream after a tick.
#[tokio::test]
async fn the_services_table_shows_usage_and_the_stream_sends_it_after_a_tick() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.stacks.stacks.lock().unwrap()[0] = status(&limited());
    let hour = Duration::from_secs(3_600);
    let draining = start_container(&app, "notes", "api", 1, &[], 3 * hour).await;
    let running = start_container(&app, "notes", "api", 2, &[], hour).await;
    let tick = |api: f64| {
        let labels = |id: &ContainerId, generation| {
            reading(id, "api", generation, sample(Some(api), Some(256 * MIB)))
        };
        app.resources.record(
            host(),
            Some(vec![labels(&running, 2), labels(&draining, 1)]),
        );
    };
    tick(15.0);

    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<th scope="col">State</th><th scope="col" class="num">CPU</th><th scope="col">Memory</th>"#),
        "{html}"
    );
    let cpu = cell(&html, &format!("cpu-{running}"));
    assert!(
        cpu.contains(
            r#"<span class="usage-value">15%</span> <span class="usage-limit">/ 150%</span>"#
        ),
        "{cpu}"
    );
    assert!(
        cell(&html, &format!("mem-{running}")).contains("meter meter-ok"),
        "{html}"
    );
    // The draining generation's muted row: its own figures, no meter.
    let old = cell(&html, &format!("mem-{draining}"));
    assert!(
        old.contains(r#"<span class="usage-value">256 MiB</span></span><svg class="spark"#),
        "{old}"
    );
    // storage runs no container.
    let storage = &html[html
        .find(r#"<span class="mono service-name">storage</span>"#)
        .unwrap()..];
    assert!(
        storage.contains(
            r#"<td class="num">—</td>
        <td>—</td>"#
        ),
        "{storage}"
    );
    assert!(!html.contains("style="), "{html}");

    let response = app.get("/stacks/notes/events", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body();
    let mut text = String::new();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(
        text.contains(r#"id="recent-events""#),
        "the card first: {text}"
    );

    // A tick sends the stack's cells, from the cache alone, and not the card.
    tick(120.0);
    text.clear();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(
        text.starts_with(&format!(
            r##"data: <hx-partial hx-target="#cpu-{draining}" hx-swap="innerHTML">"##
        )) || text.starts_with(&format!(
            r##"data: <hx-partial hx-target="#cpu-{running}" hx-swap="innerHTML">"##
        )),
        "{text}"
    );
    assert!(
        text.contains(
            r#"<span class="usage-value">120%</span> <span class="usage-limit">/ 150%</span>"#
        ),
        "{text}"
    );
    assert!(
        text.contains("meter meter-warn"),
        "120 of 150 is 80 %: {text}"
    );
    assert!(!text.contains("recent-events"), "{text}");
    assert_eq!(text.matches("<hx-partial").count(), 4, "{text}");
    // Every line of the message is the partials': nothing for the events card itself.
    assert!(
        text.lines()
            .filter(|line| !line.is_empty())
            .all(|line| line.starts_with("data: ")),
        "{text}"
    );
    let data: String = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect();
    let outside: String = data
        .split("</hx-partial>")
        .map(|part| part.split("<hx-partial").next().unwrap())
        .collect();
    assert_eq!(outside.trim(), "", "nothing outside the partials: {data}");
}

/// Reads `stream` into `text` until it contains `needle`.
async fn read_until(stream: &mut axum::body::Body, text: &mut String, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !text.contains(needle) {
            let frame = stream
                .frame()
                .await
                .expect("the stream ended early")
                .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {needle:?} in {text:?}"));
}
