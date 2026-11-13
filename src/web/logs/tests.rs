//! The view built from a store's answers, without a store; the handler tests in
//! `web/stacks/tests.rs` read a seeded one.

use std::time::UNIX_EPOCH;

use super::*;
use crate::store::{LogEntry, LogRecord};

/// 2026-11-13T12:00:00Z.
fn noon() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_794_571_200)
}

fn params(query: &str) -> LogParams {
    let uri: axum::http::Uri = format!("/?{query}").parse().unwrap();
    axum::extract::Query::<LogParams>::try_from_uri(&uri)
        .unwrap()
        .0
}

fn mount(scope: LogScope) -> LogMount {
    LogMount {
        scope,
        action: "/stacks/notes/logs".into(),
        services: vec!["api".into(), "storage".into()],
        live: Vec::new(),
    }
}

fn entry(id: i64, ago: Duration, service: &str, generation: u32) -> LogEntry {
    LogEntry {
        id,
        record: LogRecord {
            time: Timestamp::from(noon() - ago),
            stack: Some("notes".into()),
            service: service.into(),
            generation: Some(generation),
            stream: Stream::Stdout,
            level: None,
            message: format!("line {id}"),
            fields: None,
            cut: false,
            marker: false,
        },
    }
}

fn seen(service: &str, generations: &[u32]) -> ServiceSeen {
    ServiceSeen {
        stack: Some("notes".into()),
        service: service.into(),
        generations: generations.to_vec(),
    }
}

fn view(scope: LogScope, query: &str, read: Result<Read, String>) -> LogsView {
    let params = params(query);
    let (store_query, problem) = LogsView::query(&scope, &params, noon());
    LogsView::new(mount(scope), &params, noon(), &store_query, read, problem)
}

#[test]
fn the_range_defaults_to_an_hour_and_takes_presets_durations_and_times() {
    let (query, problem) = LogsView::query(
        &LogScope::Stack("notes".into()),
        &LogParams::default(),
        noon(),
    );
    assert_eq!(problem, None);
    assert_eq!(
        query.range.start,
        Timestamp::from(noon() - Duration::from_secs(3_600))
    );
    assert_eq!(
        query.range.end,
        Timestamp::from(noon()) + LogsView::END_SLACK
    );
    assert_eq!(query.scope, store::LogScope::Stack("notes".into()));
    assert_eq!(query.page, PageAt::Newest);
    assert_eq!(query.limit, LogsView::PAGE_SIZE);
    assert!(query.services.is_empty());

    let start = |q: &str| LogsView::query(&LogScope::Stack("n".into()), &params(q), noon());
    assert_eq!(
        start("since=7d").0.range.start,
        Timestamp::from(noon() - Duration::from_secs(7 * 86_400))
    );
    assert_eq!(
        start("since=2026-11-13T11:30:00Z").0.range.start,
        Timestamp::from(noon() - Duration::from_secs(1_800))
    );
    // A since that does not parse is reported and the default used.
    let (query, problem) = start("since=soon&service=api");
    assert!(problem.unwrap().contains("`soon` is not a start time"));
    assert_eq!(
        query.range.start,
        Timestamp::from(noon() - Duration::from_secs(3_600))
    );
    assert_eq!(query.services, ["api"]);
}

#[test]
fn the_page_cursor_is_a_record_key() {
    let page = |q: &str| params(q).page();
    let key = RecordKey {
        time: Timestamp::from_millis(1_000),
        id: 7,
    };
    assert_eq!(page("before=1000.7"), (PageAt::Before(key), None));
    assert_eq!(page("after=1000.7"), (PageAt::After(key), None));
    assert_eq!(page("before=1000.7&after=5.1"), (PageAt::Before(key), None));
    let (at, problem) = page("before=yesterday");
    assert_eq!(at, PageAt::Newest);
    assert!(problem.unwrap().contains("`yesterday`"));
}

#[test]
fn chips_list_the_revision_then_services_with_records_then_hoster() {
    let read = Read {
        seen: vec![
            seen("api", &[1]),
            seen("hoster", &[]),
            seen("worker", &[3]),
            seen("cache", &[1]),
        ],
        ..Read::default()
    };
    let view = view(
        LogScope::Stack("notes".into()),
        "service=worker&since=6h",
        Ok(read),
    );
    let chips: Vec<_> = view
        .chips
        .iter()
        .map(|c| (c.label.as_str(), c.class, c.current))
        .collect();
    assert_eq!(
        chips,
        [
            ("all", "svc-all", false),
            ("api", "svc-0", false),
            ("storage", "svc-1", false),
            ("cache", "svc-2", false),
            ("worker", "svc-3", true),
            ("hoster", "svc-hoster", false),
        ]
    );
    // Every link keeps the other options and drops the page cursor.
    assert_eq!(view.chips[0].href, "/stacks/notes/logs?since=6h");
    assert_eq!(
        view.chips[1].href,
        "/stacks/notes/logs?since=6h&service=api"
    );
    assert_eq!(
        view.presets.iter().map(|p| p.label).collect::<Vec<_>>(),
        LogParams::SINCE_PRESETS
    );
    assert!(view.presets[3].current);
    assert_eq!(
        view.presets[0].href,
        "/stacks/notes/logs?service=worker&since=5m"
    );
    assert_eq!(view.keep, [("service", "worker".to_owned())]);
    assert_eq!(view.since, "6h");
    assert!(view.empty);
}

#[test]
fn the_default_since_is_the_current_preset_and_stays_out_of_links() {
    let view = view(LogScope::Stack("notes".into()), "", Ok(Read::default()));
    assert!(view.chips[0].current);
    assert_eq!(view.chips[0].href, "/stacks/notes/logs");
    assert!(view.presets[2].current, "1h");
    assert_eq!(view.since, "");
    assert!(view.keep.is_empty());
}

#[test]
fn rows_show_the_generation_only_where_a_service_has_several() {
    let minute = Duration::from_secs(60);
    let mut marker = entry(4, minute, "api", 2);
    marker.record.marker = true;
    marker.record.message = "dropped 120 lines: rate cap".into();
    let mut stderr = entry(3, 2 * minute, "storage", 1);
    stderr.record.stream = Stream::Stderr;
    stderr.record.message = "\u{1b}[31mpanic\u{1b}[0m".into();
    let mut json = entry(2, 3 * minute, "api", 2);
    json.record.level = Some(Level::Warn);
    json.record.cut = true;
    let read = Read {
        seen: vec![seen("api", &[1, 2]), seen("storage", &[1])],
        page: LogPage {
            entries: vec![entry(1, 4 * minute, "api", 1), json, stderr, marker],
            older: true,
            newer: false,
            scan_stopped: None,
        },
        oldest: Some(Timestamp::from(noon() - Duration::from_secs(1_800))),
    };
    let view = view(LogScope::Stack("notes".into()), "service=", Ok(read));
    let rows: Vec<_> = view
        .rows
        .iter()
        .map(|r| (r.shown.as_str(), r.service.as_str(), r.generation, r.class))
        .collect();
    assert_eq!(
        rows,
        [
            ("11:56:00.000", "api", Some(1), "svc-0"),
            ("11:57:00.000", "api", Some(2), "svc-0"),
            ("11:58:00.000", "storage", None, "svc-1"),
            ("11:59:00.000", "api", Some(2), "svc-0"),
        ]
    );
    assert_eq!(view.rows[0].time, "2026-11-13T11:56:00.000Z");
    assert_eq!(view.rows[0].level, None);
    assert!(!view.rows[0].stderr);
    assert_eq!(view.rows[1].level, Some(Level::Warn));
    assert!(view.rows[1].cut);
    assert!(view.rows[2].stderr);
    assert_eq!(
        view.rows[2].message, "panic",
        "terminal escapes are dropped"
    );
    assert!(view.rows[3].marker);
    assert_eq!(view.count(), 3);
    // Older pages before the first row; this is the newest, so there is no newer one.
    let first = Timestamp::from(noon() - 4 * minute).as_millis();
    assert_eq!(
        view.older.as_deref(),
        Some(format!("/stacks/notes/logs?before={first}.1").as_str())
    );
    assert_eq!(view.newer, None);
    assert_eq!(view.newest, None);
    assert!(!view.empty);
    // The store starts half an hour ago, inside the hour asked for.
    assert_eq!(view.starts_at.as_deref(), Some("2026-11-13T11:30:00Z"));
}

#[test]
fn an_older_page_links_newer_and_newest_and_dates_rows_of_other_days() {
    let read = Read {
        page: LogPage {
            entries: vec![entry(9, Duration::from_secs(13 * 3_600), "api", 1)],
            older: false,
            newer: true,
            scan_stopped: None,
        },
        oldest: Some(Timestamp::from(noon() - Duration::from_secs(86_400))),
        ..Read::default()
    };
    let view = view(
        LogScope::Stack("notes".into()),
        "since=24h&before=1.1",
        Ok(read),
    );
    assert_eq!(view.rows[0].shown, "11-12 23:00:00.000");
    let key = Timestamp::from(noon() - Duration::from_secs(13 * 3_600)).as_millis();
    assert_eq!(
        view.newer.as_deref(),
        Some(format!("/stacks/notes/logs?since=24h&after={key}.9").as_str())
    );
    assert_eq!(view.newest.as_deref(), Some("/stacks/notes/logs?since=24h"));
    assert_eq!(view.older, None);
    assert_eq!(view.starts_at, None, "the store reaches the start");
    assert!(!view.empty);
}

#[test]
fn an_unreadable_store_is_said_and_is_not_an_empty_state() {
    let view = view(
        LogScope::Stack("notes".into()),
        "",
        Err("Cannot read the log store: disk I/O error".into()),
    );
    assert_eq!(
        view.unreadable.as_deref(),
        Some("Cannot read the log store: disk I/O error")
    );
    assert!(!view.empty);
    assert_eq!(view.chips.len(), 3, "all and the revision's services");
}

#[test]
fn across_stacks_rows_name_their_stack() {
    let mut own = entry(1, Duration::from_secs(60), "hoster", 0);
    own.record.stack = None;
    own.record.generation = None;
    let read = Read {
        page: LogPage {
            entries: vec![own, entry(2, Duration::from_secs(30), "api", 1)],
            ..LogPage::default()
        },
        ..Read::default()
    };
    let all = view(LogScope::All { stack: None }, "", Ok(read));
    assert!(all.stack_column);
    assert_eq!(all.rows[0].stack.as_deref(), Some("—"));
    assert_eq!(all.rows[0].class, "svc-hoster");
    assert_eq!(all.rows[1].stack.as_deref(), Some("notes"));
    let (query, _) = LogsView::query(
        &LogScope::All {
            stack: Some("notes".into()),
        },
        &LogParams::default(),
        noon(),
    );
    assert_eq!(query.scope, store::LogScope::Stack("notes".into()));
    let one = view(
        LogScope::All {
            stack: Some("notes".into()),
        },
        "",
        Ok(Read::default()),
    );
    assert!(!one.stack_column);
}

#[tokio::test]
async fn a_store_that_was_not_opened_is_unreadable() {
    let view = LogsView::load(
        &LogReader::default(),
        mount(LogScope::Stack("notes".into())),
        &LogParams::default(),
    )
    .await;
    assert_eq!(
        view.unreadable.as_deref(),
        Some("The log store is unavailable: it was not opened")
    );
}
