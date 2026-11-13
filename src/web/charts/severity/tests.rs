use super::*;

const MINUTE: Duration = Duration::from_secs(60);

/// 2026-11-13T19:30:00Z.
const T0: i64 = 1_794_598_200_000;

fn at(offset: Duration) -> Timestamp {
    Timestamp::from_millis(T0) + offset
}

/// The range a preset's page reads: its length back from now, and the page's second of slack.
fn last(length: Duration, now: Timestamp) -> Range<Timestamp> {
    (now - length)..(now + Duration::from_secs(1))
}

fn counts(info: u64, warn: u64, error: u64, unleveled: u64) -> SeverityCounts {
    SeverityCounts {
        info,
        warn,
        error,
        unleveled,
    }
}

/// The value of attribute `name` on every element of class `class`.
fn attrs<'a>(chart: &'a str, class: &str, name: &str) -> Vec<&'a str> {
    chart
        .match_indices(&format!(r#"class="{class}""#))
        .map(|(at, _)| {
            let rest = &chart[at..];
            let value = &rest[rest.find(&format!(r#" {name}=""#)).unwrap() + name.len() + 3..];
            &value[..value.find('"').unwrap()]
        })
        .collect()
}

#[test]
fn every_preset_gets_its_own_bucket_and_about_thirty_bars() {
    // Mid-bucket, so the oldest bar is a partial one.
    let now = at(Duration::from_millis(3_456_789));
    for (length, bucket) in Buckets::PLANS {
        let buckets = Buckets::covering(last(length, now));
        assert_eq!(buckets.bucket(), bucket, "{length:?}");
        let whole = u32::try_from(length.as_millis() / bucket.as_millis()).unwrap();
        assert!(
            (24..=36).contains(&whole),
            "{length:?} gives {whole} whole bars"
        );
        assert_eq!(buckets.bars, whole as usize + 1, "{length:?}");
        assert_eq!(buckets.length, length);
        assert!(buckets.start_of(0) <= now - length);
        assert!(buckets.start_of(buckets.bars) > now);
        assert_eq!(buckets.start_of(0).floor(bucket), buckets.start_of(0));
    }
}

#[test]
fn other_ranges_get_the_shortest_bucket_that_fits_and_long_ones_stay_bounded() {
    let now = at(Duration::ZERO);
    let bucket = |length: Duration| Buckets::covering(last(length, now)).bucket();
    assert_eq!(bucket(Duration::from_secs(90)), Duration::from_secs(10));
    assert_eq!(bucket(2 * 60 * MINUTE), 15 * MINUTE);
    assert_eq!(bucket(3 * 24 * 60 * MINUTE), 6 * 60 * MINUTE);
    let month = Buckets::covering(last(30 * 24 * 60 * MINUTE, now));
    assert_eq!(month.bucket().as_secs() % (6 * 3_600), 0);
    assert!(month.bars <= 40, "{}", month.bars);
    // A range ending before it starts (a *since* in the future) is one bar.
    assert_eq!(Buckets::covering(now..now).bars, 1);
}

#[test]
fn bars_stack_from_no_level_up_to_error_with_exact_svg() {
    let buckets = Buckets::covering(at(Duration::ZERO)..at(MINUTE));
    assert_eq!(
        (buckets.bars, buckets.bucket()),
        (6, Duration::from_secs(10))
    );
    let chart = SeverityBars::new(
        buckets,
        [
            (at(Duration::ZERO), counts(2, 1, 1, 2)),
            (at(Duration::from_secs(50)), counts(1, 0, 0, 0)),
        ],
        &[],
    )
    .of("notes")
    .to_string();
    assert!(chart.starts_with(concat!(
        r#"<div class="chart chart-bars chart-levels" role="img" aria-label="Log records by severity for notes over the last 1m in 10s buckets: 7 records, error 1, warn 1, debug/info 3, no level 2; most errors 19:30:00 to 19:30:10 with 1">"#,
        r#"<div class="chart-y" aria-hidden="true"><span>6</span><span>4</span><span>2</span><span>0</span></div>"#,
    )), "{chart}");
    assert!(chart.contains(concat!(
        r#"<g class="chart-bar"><title>19:30:00 to 19:30:10: 6 records, error 1, warn 1, debug/info 2, no level 2</title>"#,
        r#"<rect class="chart-hit" x="0" y="0" width="100" height="180" vector-effect="non-scaling-stroke"></rect>"#,
        r#"<rect class="chart-seg chart-lvl-none" x="10" y="120" width="80" height="60" vector-effect="non-scaling-stroke"></rect>"#,
        r#"<rect class="chart-seg chart-lvl-info" x="10" y="60" width="80" height="60" vector-effect="non-scaling-stroke"></rect>"#,
        r#"<rect class="chart-seg chart-lvl-warn" x="10" y="30" width="80" height="30" vector-effect="non-scaling-stroke"></rect>"#,
        r#"<rect class="chart-seg chart-lvl-error" x="10" y="0" width="80" height="30" vector-effect="non-scaling-stroke"></rect></g>"#,
        r#"<g class="chart-bar"><title>19:30:10 to 19:30:20: no records</title>"#,
    )));
    assert!(chart.contains(r#"<title>19:30:50 to now: 1 record, debug/info 1</title>"#));
    assert!(chart.ends_with(concat!(
        r#"<div class="chart-x" aria-hidden="true"><span>19:30:00</span><span>19:30:20</span>"#,
        r#"<span>19:30:40</span><span>now</span></div></div>"#,
    )));
}

#[test]
fn an_empty_range_keeps_its_axis_and_says_so() {
    let buckets = Buckets::covering(last(60 * MINUTE, at(Duration::ZERO)));
    let chart = SeverityBars::new(buckets, [], &[]);
    let html = chart.to_string();
    assert!(html.contains(
        r#"aria-label="Log records by severity over the last 1h in 2m buckets: no records""#
    ));
    assert!(!html.contains("chart-seg"));
    assert_eq!(attrs(&html, "chart-hit", "x").len(), buckets.bars);
    assert!(html.contains("<span>1</span>"), "a unit scale: {html}");
    assert_eq!(
        chart.legend().to_string().matches("<li>").count(),
        4,
        "every segment is in the legend"
    );
    assert!(chart.legend().to_string().contains("no level 0"));
}

#[test]
fn counts_outside_the_buckets_are_left_out() {
    let buckets = Buckets::covering(at(Duration::ZERO)..at(MINUTE));
    let chart = SeverityBars::new(
        buckets,
        [
            (at(Duration::ZERO) - MINUTE, counts(5, 0, 0, 0)),
            (at(Duration::from_secs(5)), counts(5, 0, 0, 0)),
            (at(2 * MINUTE), counts(5, 0, 0, 0)),
        ],
        &[],
    );
    assert!(chart.to_string().contains("no records\">"));
}

#[test]
fn bars_link_to_their_bucket_and_make_the_chart_a_group() {
    let buckets = Buckets::covering(at(Duration::ZERO)..at(Duration::from_secs(30)));
    let chart = SeverityBars::new(buckets, [], &[])
        .linked(|range, newest| {
            format!(
                "/logs?since={}&to={}&newest={newest}",
                range.start.as_millis() - T0,
                range.end.as_millis() - T0
            )
        })
        .to_string();
    assert!(chart.contains(r#"role="group""#));
    assert_eq!(
        attrs(&chart, "chart-bar", "href"),
        [
            "/logs?since=0&#38;to=10000&#38;newest=false",
            "/logs?since=10000&#38;to=20000&#38;newest=false",
            "/logs?since=20000&#38;to=30000&#38;newest=true",
        ]
    );
    assert_eq!(chart.matches("</a>").count(), 3);
    assert!(!chart.contains("<g class=\"chart-bar\">"));
}

#[test]
fn a_level_filter_draws_only_its_segments() {
    let buckets = Buckets::covering(at(Duration::ZERO)..at(Duration::from_secs(20)));
    let chart = SeverityBars::new(
        buckets,
        [(at(Duration::ZERO), counts(0, 2, 1, 0))],
        &[Level::Warn, Level::Error],
    );
    let legend = chart.legend().to_string();
    assert!(legend.contains("error 1") && legend.contains("warn 2"));
    assert!(!legend.contains("debug/info") && !legend.contains("no level"));
    assert!(
        chart
            .to_string()
            .contains(": 3 records, error 1, warn 2; most errors 19:30:00 to 19:30:10 with 1")
    );
}

#[test]
fn the_label_names_the_peak_error_bucket_and_an_unfiltered_chart() {
    let buckets = Buckets::covering(last(24 * 60 * MINUTE, at(Duration::ZERO)));
    let mut bars: Vec<(Timestamp, SeverityCounts)> = (0..buckets.bars)
        .map(|i| (buckets.start_of(i), counts(100, 3, 1, 10)))
        .collect();
    bars[5].1.error = 40;
    bars[9].1.error = 40;
    let chart = SeverityBars::new(buckets, bars, &[]).unfiltered();
    assert!(chart.is_unfiltered());
    assert_eq!(chart.bucket_label(), "1h");
    let html = chart.to_string();
    // The newest of equal peaks; dates over a day.
    assert!(
        html.contains("over the last 24h in 1h buckets, not narrowed by grep or field filters: "),
        "{html}"
    );
    assert!(
        html.contains("; most errors 11-13 04:00 to 11-13 05:00 with 40\""),
        "{html}"
    );
    assert!(html.contains("<span>11-12 19:00</span>"), "{html}");

    let quiet = SeverityBars::new(buckets, [(buckets.start_of(0), counts(1, 0, 0, 0))], &[]);
    assert!(
        quiet
            .to_string()
            .contains("debug/info 1, no level 0; no errors\"")
    );
}
