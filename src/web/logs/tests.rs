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
        stream: Some("/stacks/notes/logs/stream".into()),
        download: Some("/stacks/notes/logs/download".into()),
        stacks: Vec::new(),
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
            trace_id: None,
            span_id: None,
            received: None,
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
    let plan = LogsView::plan(&scope, &params, noon());
    LogsView::new(mount(scope), &params, noon(), plan, read)
}

/// The store query and the problem of `query`.
fn plan(query: &str) -> Plan {
    LogsView::plan(&LogScope::Stack("notes".into()), &params(query), noon())
}

#[test]
fn the_range_defaults_to_an_hour_and_takes_presets_durations_and_times() {
    let Plan { query, problem, .. } = LogsView::plan(
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

    let start = |q: &str| {
        let plan = LogsView::plan(&LogScope::Stack("n".into()), &params(q), noon());
        (plan.query, plan.problem)
    };
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
        unleveled: None,
        head: 4,
        ..Read::default()
    };
    // Not following, so the page links older ones.
    let view = view(
        LogScope::Stack("notes".into()),
        "service=&follow=off",
        Ok(read),
    );
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
        Some(format!("/stacks/notes/logs?follow=off&before={first}.1").as_str())
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

/// A page of `entries` (ids), the store's head at `head`.
fn read_of(ids: &[i64], head: i64, newer: bool) -> Result<Read, String> {
    Ok(Read {
        page: LogPage {
            entries: ids
                .iter()
                .map(|id| entry(*id, Duration::from_secs(60), "api", 1))
                .collect(),
            older: false,
            newer,
            scan_stopped: None,
        },
        head,
        ..Read::default()
    })
}

#[test]
fn the_newest_page_follows_from_past_its_rows_and_the_head() {
    let stack = || LogScope::Stack("notes".into());
    let view_of = |query: &str, read| view(stack(), query, read);

    // From the newest of the page's ids and the head, with the page's options.
    let view = view_of("level=warn&q=a+b", read_of(&[3, 7], 5, false));
    let follow = view.follow.as_ref().expect("the newest page follows");
    assert_eq!(
        view.stream(),
        Some("/stacks/notes/logs/stream?level=warn&q=a+b&from=7")
    );
    assert_eq!(follow.on, "/stacks/notes/logs?level=warn&q=a+b");
    assert_eq!(follow.off, "/stacks/notes/logs?level=warn&q=a+b&follow=off");
    let view = view_of("", read_of(&[3], 9, false));
    assert_eq!(view.stream(), Some("/stacks/notes/logs/stream?from=9"));

    // Following hides "older", which would skip the rows the list drops; off shows it.
    let mut read = read_of(&[3], 3, false);
    read.as_mut().unwrap().page.older = true;
    assert_eq!(view_of("", read).older, None);
    let mut read = read_of(&[3], 3, false);
    read.as_mut().unwrap().page.older = true;
    assert!(view_of("follow=off", read).older.is_some());
    let view = view_of("", read_of(&[], 0, false));
    assert!(view.empty);
    assert_eq!(view.stream(), Some("/stacks/notes/logs/stream?from=0"));

    // Switched off: the switch stays, the other links keep it off.
    let view = view_of("follow=off", read_of(&[3], 3, false));
    let follow = view.follow.as_ref().unwrap();
    assert!(!follow.following());
    assert_eq!(view.stream(), None);
    assert_eq!(follow.on, "/stacks/notes/logs");
    assert!(
        view.levels[1].href.ends_with("follow=off&level=error"),
        "{}",
        view.levels[1].href
    );

    // Paged back into history: no switch. "newer" and "newest" turn it back on.
    let view = view_of("follow=off&before=1.1", read_of(&[3], 3, true));
    assert!(view.follow.is_none());
    let key = Timestamp::from(noon() - Duration::from_secs(60)).as_millis();
    assert_eq!(
        view.newer.as_deref(),
        Some(format!("/stacks/notes/logs?after={key}.3").as_str())
    );
    assert_eq!(view.newest.as_deref(), Some("/stacks/notes/logs"));

    // Paged forward to the end of the range: it follows again.
    let view = view_of("after=1.1", read_of(&[3], 3, true));
    assert!(view.follow.is_none(), "there is more");
    let view = view_of("after=1.1", read_of(&[3], 4, false));
    assert_eq!(view.stream(), Some("/stacks/notes/logs/stream?from=4"));

    // Nothing to follow when the store cannot be read or the pattern does not compile.
    assert!(view_of("", Err("no".into())).follow.is_none());
    assert!(
        view_of("q=(&regex=on", read_of(&[], 0, false))
            .follow
            .is_none()
    );
}

#[test]
fn a_mount_without_a_stream_does_not_follow() {
    let params = params("");
    let plan = LogsView::plan(&LogScope::All { stack: None }, &params, noon());
    let mount = LogMount {
        stream: None,
        ..mount(LogScope::All { stack: None })
    };
    let view = LogsView::new(mount, &params, noon(), plan, read_of(&[1], 1, false));
    assert!(view.follow.is_none());
}

#[test]
fn followed_rows_render_as_the_page_does() {
    let rows = Rows {
        stack_column: false,
        dated: false,
        text: Some(TextFilter::substring("line")),
        ..Rows::new(vec!["api".into()], &[seen("api", &[1])], links(""))
    };
    let mut rows = rows;
    let first = entry(1, Duration::from_secs(1), "api", 1);
    assert_eq!(rows.row(&first).generation, None);
    // A new generation appears while following: rows name theirs from then on.
    let second = entry(2, Duration::ZERO, "api", 2);
    rows.learn(&second);
    let row = rows.row(&second);
    assert_eq!(row.generation, Some(2));
    let html = LogRecords {
        rows: vec![row],
        stack_column: false,
        notice: None,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<li class="log-record svc-0">"#), "{html}");
    assert!(html.contains("<mark>line</mark> 2"), "{html}");
    assert!(
        html.contains(r#"<span class="log-gen">g2</span>"#),
        "{html}"
    );
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
    let Plan { query, .. } = LogsView::plan(
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

#[test]
fn level_chips_pick_a_level_or_from_warn_up() {
    let levels = |q: &str| plan(q).query.levels;
    assert!(levels("").is_empty());
    assert!(levels("level=all").is_empty());
    assert_eq!(levels("level=error"), [Level::Error]);
    assert_eq!(levels("level=warn"), [Level::Warn, Level::Error]);
    assert_eq!(levels("level=warn%2B"), [Level::Warn, Level::Error]);
    assert_eq!(levels("level=info"), [Level::Info]);
    assert_eq!(levels("level=debug"), [Level::Trace, Level::Debug]);
    let bad = plan("level=loud");
    assert!(bad.query.levels.is_empty());
    assert!(bad.problem.unwrap().contains("`loud` is not a level"));

    let view = view(
        LogScope::Stack("notes".into()),
        "level=warn&service=api",
        Ok(Read {
            unleveled: Some(3),
            ..Read::default()
        }),
    );
    let chips: Vec<_> = view
        .levels
        .iter()
        .map(|c| (c.label.as_str(), c.class, c.current))
        .collect();
    assert_eq!(
        chips,
        [
            ("all", "lvl-all", false),
            ("error", "lvl-error", false),
            ("warn+", "lvl-warn", true),
            ("info", "lvl-info", false),
            ("debug", "lvl-debug", false),
        ]
    );
    assert_eq!(view.levels[0].href, "/stacks/notes/logs?service=api");
    assert_eq!(
        view.levels[1].href,
        "/stacks/notes/logs?service=api&level=error"
    );
    // The service chips and the form keep the level.
    assert_eq!(view.chips[0].href, "/stacks/notes/logs?level=warn");
    assert_eq!(
        view.keep,
        [("service", "api".to_owned()), ("level", "warn".to_owned())]
    );
    let note = view.unleveled.expect("text lines are hidden");
    assert_eq!(note.count, Some(3));
    assert!(note.says().starts_with("3 records without a level"));
    assert_eq!(note.all, "/stacks/notes/logs?service=api");
}

#[test]
fn the_hidden_count_is_read_only_when_it_is_cheap() {
    assert!(!plan("").counts_unleveled());
    assert!(plan("level=error").counts_unleveled());
    assert!(!plan("level=error&q=x").counts_unleveled());
    // Without a count the note still says text lines are hidden; with none hidden, no note.
    let uncounted = view(
        LogScope::Stack("notes".into()),
        "level=error&q=x",
        Ok(Read::default()),
    );
    assert_eq!(uncounted.unleveled.unwrap().count, None);
    let none = view(
        LogScope::Stack("notes".into()),
        "level=error",
        Ok(Read {
            unleveled: Some(0),
            ..Read::default()
        }),
    );
    assert!(none.unleveled.is_none());
    let all = view(LogScope::Stack("notes".into()), "", Ok(Read::default()));
    assert!(all.unleveled.is_none());
}

#[test]
fn grep_is_a_substring_unless_regex_is_on() {
    assert!(plan("").query.text.is_none());
    assert!(plan("q=").query.text.is_none(), "an empty q is no filter");
    // Spaces are searched for, not trimmed away.
    let spaces = plan("q=%20%20").query.text.unwrap();
    assert!(matches!(&spaces, TextFilter::Substring(s) if s == "  "));
    let substring = plan("q=a.c").query.text.unwrap();
    assert!(matches!(&substring, TextFilter::Substring(s) if s == "a.c"));
    let regex = plan("q=a.c&regex=on").query.text.unwrap();
    assert!(matches!(regex, TextFilter::Regex(_)));
    let invalid = plan("q=(unclosed&regex=on");
    assert!(invalid.query.text.is_none());
    assert!(
        invalid
            .invalid
            .unwrap()
            .starts_with("invalid regular expression")
    );
    // A pattern too large to compile is invalid too, not slow.
    assert!(plan(r"q=(%3F:\w{100}){100}&regex=on").invalid.is_some());
}

#[test]
fn every_link_keeps_level_and_grep() {
    let view = view(
        LogScope::Stack("notes".into()),
        "q=a%20b&regex=on&level=error&since=6h&follow=off",
        Ok(Read {
            page: LogPage {
                entries: vec![entry(1, Duration::from_secs(60), "api", 1)],
                older: true,
                newer: true,
                scan_stopped: None,
            },
            ..Read::default()
        }),
    );
    let all = "since=6h&level=error&q=a+b&regex=on&follow=off";
    assert_eq!(view.chips[0].href, format!("/stacks/notes/logs?{all}"));
    assert!(
        view.older
            .unwrap()
            .starts_with(&format!("/stacks/notes/logs?{all}&before="))
    );
    assert!(
        view.newer
            .unwrap()
            .starts_with("/stacks/notes/logs?since=6h&level=error&q=a+b&regex=on&after="),
        "newer turns following back on"
    );
    assert_eq!(view.newest, None);
    assert_eq!(
        view.presets[0].href,
        "/stacks/notes/logs?level=error&q=a+b&regex=on&follow=off&since=5m"
    );
    // The form edits q, regex and since; it keeps the level and the switch.
    assert_eq!(
        view.keep,
        [("level", "error".to_owned()), ("follow", "off".to_owned())]
    );
    assert_eq!(view.q, "a b");
    assert!(view.regex);
}

#[test]
fn matches_are_marked_in_the_message_as_runs() {
    let mut first = entry(1, Duration::from_secs(60), "api", 1);
    first.record.message = "<script>Error &amp; error</script>".into();
    let mut marker = entry(2, Duration::from_secs(30), "api", 1);
    marker.record.marker = true;
    marker.record.message = "dropped 3 error lines".into();
    let view = view(
        LogScope::Stack("notes".into()),
        "q=ERROR",
        Ok(Read {
            page: LogPage {
                entries: vec![first, marker, entry(3, Duration::from_secs(10), "api", 1)],
                ..LogPage::default()
            },
            ..Read::default()
        }),
    );
    let spans: Vec<_> = view.rows[0]
        .spans()
        .into_iter()
        .map(|s| (s.text, s.mark))
        .collect();
    assert_eq!(
        spans,
        [
            ("<script>", false),
            ("Error", true),
            (" &amp; ", false),
            ("error", true),
            ("</script>", false),
        ]
    );
    assert!(view.rows[1].marks.is_empty(), "notices are not marked");
    // A record that matched in its fields only is listed whole.
    assert_eq!(
        view.rows[2]
            .spans()
            .into_iter()
            .map(|s| (s.text, s.mark))
            .collect::<Vec<_>>(),
        [("line 3", false)]
    );
}

#[test]
fn an_invalid_pattern_is_beside_the_field_with_no_records_and_no_empty_state() {
    let view = view(
        LogScope::Stack("notes".into()),
        "q=(&regex=on",
        Ok(Read::default()),
    );
    assert!(view.invalid.is_some());
    assert!(view.rows.is_empty());
    assert!(!view.empty);
    assert!(view.cleared.is_none());
}

#[test]
fn an_empty_filtered_page_names_its_filters_and_clears_them() {
    let view = view(
        LogScope::Stack("notes".into()),
        "service=api&level=warn&q=boom",
        Ok(Read::default()),
    );
    assert!(view.empty);
    let (active, clear) = view.cleared.unwrap();
    assert_eq!(active, "level warn+ and grep “boom”");
    assert_eq!(clear, "/stacks/notes/logs?service=api");
    let unfiltered = super::LogsView::new(
        mount(LogScope::Stack("notes".into())),
        &params("service=api"),
        noon(),
        plan("service=api"),
        Ok(Read::default()),
    );
    assert!(unfiltered.empty && unfiltered.cleared.is_none());
}

#[test]
fn a_stopped_scan_says_how_far_back_and_searches_older_from_there() {
    let stopped_at = RecordKey {
        time: Timestamp::from(noon() - Duration::from_secs(3_000)),
        id: 41,
    };
    let read = |entries| Read {
        page: LogPage {
            entries,
            older: true,
            newer: false,
            scan_stopped: Some(stopped_at),
        },
        ..Read::default()
    };
    let view = view(
        LogScope::Stack("notes".into()),
        "q=needle",
        Ok(read(vec![entry(50, Duration::from_secs(60), "api", 1)])),
    );
    let stopped = view.stopped.as_ref().unwrap();
    assert!(stopped.older);
    assert_eq!(stopped.at, "2026-11-13T11:10:00Z");
    assert_eq!(
        stopped.href,
        format!(
            "/stacks/notes/logs?q=needle&before={}.41",
            stopped_at.time.as_millis()
        )
    );
    assert_eq!(stopped.cap().to_string(), "50,000");
    assert_eq!(view.older, None, "search older replaces older");

    // Reading newer records, it offers to search newer.
    let after = super::LogsView::new(
        mount(LogScope::Stack("notes".into())),
        &params("q=needle&after=1.1"),
        noon(),
        plan("q=needle&after=1.1"),
        Ok(read(Vec::new())),
    );
    let stopped = after.stopped.unwrap();
    assert!(!stopped.older);
    assert!(stopped.href.contains("&after="), "{}", stopped.href);
}

#[test]
fn the_footer_counts_errors_and_warnings_shown() {
    let mut error = entry(1, Duration::from_secs(60), "api", 1);
    error.record.level = Some(Level::Error);
    let mut warn = entry(2, Duration::from_secs(50), "api", 1);
    warn.record.level = Some(Level::Warn);
    let mut marker = entry(4, Duration::from_secs(30), "api", 1);
    marker.record.marker = true;
    let view = view(
        LogScope::Stack("notes".into()),
        "",
        Ok(Read {
            page: LogPage {
                entries: vec![
                    error,
                    warn,
                    entry(3, Duration::from_secs(40), "api", 1),
                    marker,
                ],
                ..LogPage::default()
            },
            ..Read::default()
        }),
    );
    assert_eq!((view.count(), view.errors(), view.warnings()), (3, 1, 1));
}

/// The links of the stack view with the options of `query`.
fn links(query: &str) -> Links {
    Links {
        action: "/stacks/notes/logs".into(),
        filters: params(query).filters(LogSource::Containers),
    }
}

fn field(key: &str, value: &str) -> FieldFilter {
    FieldFilter {
        key: key.into(),
        value: value.into(),
    }
}

#[test]
fn a_field_filter_splits_on_the_first_unencoded_equals() {
    assert_eq!("user.id=42".parse(), Ok(field("user.id", "42")));
    assert_eq!(
        "q=a=b".parse(),
        Ok(field("q", "a=b")),
        "the value keeps its `=`"
    );
    assert_eq!("a%3Db=c".parse(), Ok(field("a=b", "c")));
    assert_eq!("a%3db=c".parse(), Ok(field("a=b", "c")));
    assert_eq!("100%25=x".parse(), Ok(field("100%", "x")));
    assert_eq!(
        "50%off=x".parse(),
        Ok(field("50%off", "x")),
        "a stray % is kept"
    );
    assert_eq!("k=".parse(), Ok(field("k", "")));
    assert_eq!(field("a=b%", "c=d").to_string(), "a%3Db%25=c=d");
    assert!(
        "nothing"
            .parse::<FieldFilter>()
            .unwrap_err()
            .contains("`nothing` is not a field filter")
    );
}

proptest::proptest! {
    #[test]
    fn field_filters_round_trip_through_their_text_and_a_link(
        key in "[a-z.=%&+ #\u{e9}]{0,12}",
        value in "[a-z.=%&+ #\n\u{e9}]{0,12}",
        other in proptest::prelude::any::<String>(),
    ) {
        let filter = field(&key, &value);
        proptest::prop_assert_eq!(filter.to_string().parse::<FieldFilter>(), Ok(filter.clone()));
        let odd = field(&other, &other);
        proptest::prop_assert_eq!(odd.to_string().parse::<FieldFilter>(), Ok(odd.clone()));
        // Added to a link and read back from it, beside an option and another filter.
        let href = links("level=error").with_field(&odd);
        let href = Links { action: "/".into(), filters: params(href.split_once('?').unwrap().1).filters(LogSource::Containers) }
            .with_field(&filter);
        let (fields, problem) = params(href.split_once('?').unwrap().1).fields();
        proptest::prop_assert_eq!(problem, None);
        let mut expected = vec![odd];
        if !expected.contains(&filter) {
            expected.push(filter);
        }
        proptest::prop_assert_eq!(fields, expected);
    }
}

#[test]
fn field_filters_repeat_narrow_the_query_and_every_link_keeps_them() {
    let query = "field=user.id%3D42&level=error&field=span.name%3Dserve&field=user.id%3D42\
                 &field=broken&field=&follow=off";
    let planned = plan(query);
    assert_eq!(
        planned.query.fields,
        [
            ("user.id".to_owned(), "42".to_owned()),
            ("span.name".to_owned(), "serve".to_owned()),
        ],
        "each once, in order"
    );
    assert!(
        planned
            .problem
            .as_deref()
            .unwrap()
            .contains("`broken` is not a field filter")
    );
    assert!(!planned.counts_unleveled(), "a field filter scans");
    let view = view(LogScope::Stack("notes".into()), query, Ok(Read::default()));
    let fields = "field=user.id%3D42&field=span.name%3Dserve";
    assert_eq!(
        view.chips[0].href,
        format!("/stacks/notes/logs?level=error&{fields}&follow=off")
    );
    // The form keeps them as hidden fields.
    assert_eq!(
        view.keep,
        [
            ("level", "error".to_owned()),
            ("field", "user.id=42".to_owned()),
            ("field", "span.name=serve".to_owned()),
            ("follow", "off".to_owned()),
        ]
    );
    // A chip per filter, which removes that one.
    let chips: Vec<_> = view
        .fields
        .iter()
        .map(|c| (c.label.as_str(), c.remove.as_str()))
        .collect();
    assert_eq!(
        chips,
        [
            (
                "user.id=42",
                "/stacks/notes/logs?level=error&field=span.name%3Dserve&follow=off"
            ),
            (
                "span.name=serve",
                "/stacks/notes/logs?level=error&field=user.id%3D42&follow=off"
            ),
        ]
    );
    // Empty, the page names them and clears them with the other filters.
    let (active, clear) = view.cleared.unwrap();
    assert_eq!(
        active,
        "level error and field user.id=42 and field span.name=serve"
    );
    assert_eq!(clear, "/stacks/notes/logs?follow=off");
    // The follow stream applies them too.
    let view = super::LogsView::new(
        mount(LogScope::Stack("notes".into())),
        &params("field=k%3Dv"),
        noon(),
        plan("field=k%3Dv"),
        read_of(&[1], 1, false),
    );
    assert_eq!(
        view.stream(),
        Some("/stacks/notes/logs/stream?field=k%3Dv&from=1")
    );
}

#[test]
fn a_record_with_fields_opens_on_them_flattened_and_linked() {
    let mut json = entry(1, Duration::from_secs(60), "api", 2);
    json.record.level = Some(Level::Info);
    json.record.stream = Stream::Stderr;
    json.record.fields = Some(
        r#"{"target":"api::serve","span":{"name":"serve"},"spans":[{"id":7}],"a=b":"x y"}"#.into(),
    );
    let mut empty = entry(2, Duration::from_secs(50), "api", 2);
    empty.record.fields = Some("{}".into());
    let mut raw = entry(3, Duration::from_secs(40), "api", 2);
    raw.record.fields = Some("{\"cut".into());
    let mut marker = entry(4, Duration::from_secs(30), "api", 2);
    marker.record.marker = true;
    marker.record.fields = Some(r#"{"lost":3}"#.into());
    let view = view(
        LogScope::Stack("notes".into()),
        "level=info&follow=off&before=1.1",
        Ok(Read {
            page: LogPage {
                entries: vec![json, empty, raw, marker, entry(5, Duration::ZERO, "api", 2)],
                ..LogPage::default()
            },
            ..Read::default()
        }),
    );
    let row = &view.rows[0];
    assert_eq!(row.target.as_deref(), Some("api::serve"));
    let detail = row.detail.as_ref().expect("it has fields");
    assert_eq!(detail.stream, Stream::Stderr);
    assert_eq!(
        detail.generation,
        Some(2),
        "named even when the row does not"
    );
    assert!(!detail.open);
    let fields: Vec<_> = detail
        .fields
        .iter()
        .map(|f| (f.key.as_str(), f.value.as_str(), f.href.as_str()))
        .collect();
    assert_eq!(
        fields,
        [
            (
                "a=b",
                "x y",
                "/stacks/notes/logs?level=info&field=a%253Db%3Dx+y&follow=off"
            ),
            (
                "span.name",
                "serve",
                "/stacks/notes/logs?level=info&field=span.name%3Dserve&follow=off"
            ),
            (
                "spans.0.id",
                "7",
                "/stacks/notes/logs?level=info&field=spans.0.id%3D7&follow=off"
            ),
            (
                "target",
                "api::serve",
                "/stacks/notes/logs?level=info&field=target%3Dapi%3A%3Aserve&follow=off"
            ),
        ]
    );
    assert!(view.rows[1].detail.is_none(), "no fields, no expander");
    let raw = view.rows[2].detail.as_ref().unwrap();
    assert_eq!(raw.raw.as_deref(), Some("{\"cut"));
    assert!(raw.fields.is_empty());
    assert!(view.rows[3].detail.is_none(), "a notice does not open");
    assert!(view.rows[4].detail.is_none());
    assert_eq!(view.rows[4].target, None);
}

#[test]
fn grep_in_the_fields_opens_the_record_and_marks_the_value() {
    let mut json = entry(1, Duration::from_secs(60), "api", 1);
    json.record.fields = Some(r#"{"user":{"id":"u-42"},"path":"/x"}"#.into());
    let mut message_only = entry(2, Duration::from_secs(50), "api", 1);
    message_only.record.message = "user u-42 logged in".into();
    message_only.record.fields = Some(r#"{"path":"/x"}"#.into());
    let view = view(
        LogScope::Stack("notes".into()),
        "q=U-4",
        Ok(Read {
            page: LogPage {
                entries: vec![json, message_only],
                ..LogPage::default()
            },
            ..Read::default()
        }),
    );
    let detail = view.rows[0].detail.as_ref().unwrap();
    assert!(detail.open);
    let user = detail.fields.iter().find(|f| f.key == "user.id").unwrap();
    assert_eq!(
        user.spans()
            .into_iter()
            .map(|s| (s.text, s.mark))
            .collect::<Vec<_>>(),
        [("u-4", true), ("2", false)]
    );
    assert!(
        !view.rows[1].detail.as_ref().unwrap().open,
        "matched in the message"
    );
}

#[test]
fn opened_records_render_escaped_with_no_inline_style() {
    let mut json = entry(1, Duration::from_secs(60), "api", 1);
    json.record.fields =
        Some(r#"{"target":"<b>t</b>","<k>":"<script>\"x\"&amp;</script>","n":"a\nb"}"#.into());
    json.record.cut = true;
    let mut raw = entry(2, Duration::from_secs(50), "api", 1);
    raw.record.fields = Some("<i>not json".into());
    let rows = Rows {
        text: Some(TextFilter::substring("\"")),
        ..Rows::new(vec!["api".into()], &[seen("api", &[1])], links("q=%22"))
    };
    let html = LogRecords {
        rows: vec![
            rows.row(&json),
            rows.row(&raw),
            rows.row(&entry(3, Duration::ZERO, "api", 1)),
        ],
        stack_column: false,
        notice: None,
    }
    .render()
    .unwrap();
    assert!(!html.contains("<script>"), "{html}");
    assert!(
        !html.contains("<k>") && !html.contains("<b>") && !html.contains("<i>"),
        "{html}"
    );
    assert!(!html.contains("style="), "{html}");
    assert_eq!(html.matches("<details").count(), 2, "{html}");
    assert!(
        html.contains(r#"<span class="log-target">&#60;b&#62;t&#60;/b&#62;</span> line 1"#),
        "{html}"
    );
    assert!(html.contains("<dt>&#60;k&#62;</dt>"), "{html}");
    assert!(
        html.contains(
            r#"href="/stacks/notes/logs?q=%22&#38;field=%3Ck%3E%3D%3Cscript%3E%22x%22%26amp%3B%3C%2Fscript%3E""#
        ),
        "{html}"
    );
    assert!(
        html.contains(
            "&#60;script&#62;<mark>&#34;</mark>x<mark>&#34;</mark>&#38;amp;&#60;/script&#62;"
        ),
        "{html}"
    );
    assert!(html.contains("<dd><a href=\"/stacks/notes/logs?q=%22&#38;field=n%3Da%0Ab\" title=\"Only records with this value\">a\nb</a></dd>"), "{html}");
    assert!(
        html.contains(" · stdout · generation 1 · cut at the line length cap</p>"),
        "{html}"
    );
    assert!(html.contains("&#60;i&#62;not json</p>"), "{html}");
    // The record without fields is a plain row.
    assert!(
        html.contains(r#"<span class="log-msg">line 3</span></li>"#),
        "{html}"
    );
}

#[test]
fn the_source_switch_is_the_stack_scopes_and_each_source_keeps_its_own_options() {
    let stack = LogScope::Stack("notes".into());
    let all = LogScope::All { stack: None };
    assert_eq!(params("").source(&stack), (LogSource::Containers, None));
    assert_eq!(
        params("source=Deploy").source(&stack),
        (LogSource::Deploy, None)
    );
    // Across stacks there is no switch.
    assert_eq!(
        params("source=deploy").source(&all),
        (LogSource::Containers, None)
    );
    let (source, problem) = params("source=nope").source(&stack);
    assert_eq!(source, LogSource::Containers);
    assert!(problem.unwrap().contains("`nope` is not a log source"));
    assert!(plan("source=nope").problem.unwrap().contains("`nope`"));

    let given = params("source=deploy&service=api&level=warn&field=a%3Db&since=6h&q=x&follow=off");
    assert_eq!(
        given.filters(LogSource::Deploy),
        [
            ("source", "deploy".to_owned()),
            ("since", "6h".to_owned()),
            ("q", "x".to_owned()),
            ("follow", "off".to_owned()),
        ]
    );
    assert_eq!(
        given.filters(LogSource::Containers)[0],
        ("service", "api".to_owned())
    );
}

#[test]
fn across_stacks_the_range_defaults_to_15_minutes_and_hoster_picks_its_own_records() {
    let all = LogScope::All { stack: None };
    let Plan { query, .. } = LogsView::plan(&all, &params_scoped("", &all), noon());
    assert_eq!(
        query.range.start,
        Timestamp::from(noon() - Duration::from_secs(900))
    );
    // The stack tab keeps its hour.
    let stack = LogScope::Stack("notes".into());
    let Plan { query, .. } = LogsView::plan(&stack, &params_scoped("", &stack), noon());
    assert_eq!(
        query.range.start,
        Timestamp::from(noon() - Duration::from_secs(3_600))
    );

    let hoster = LogScope::All {
        stack: Some(LogScope::HOSTER.into()),
    };
    let Plan { query, .. } = LogsView::plan(&hoster, &params(""), noon());
    assert_eq!(query.scope, store::LogScope::Hoster);
    // The stack tab ignores a `stack` parameter: its path fixes it.
    let view = view(
        LogScope::Stack("notes".into()),
        "stack=blog",
        Ok(Read::default()),
    );
    assert!(view.stacks.is_empty());
    assert!(view.keep.is_empty());
    assert_eq!(view.chips[0].href, "/stacks/notes/logs");
}

fn params_scoped(query: &str, scope: &LogScope) -> LogParams {
    params(query).scoped(scope)
}

#[test]
fn the_record_count_says_when_it_stopped_at_its_cap() {
    assert_eq!(Total(1_234).to_string(), "1,234");
    let cap = u64::from(LogsView::COUNT_CAP);
    assert_eq!(Total(cap).to_string(), "100,000");
    assert_eq!(Total(cap + 1).to_string(), "100,000+");
}
