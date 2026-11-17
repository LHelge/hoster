use super::*;

const MINUTE: Duration = Duration::from_secs(60);
const NOW: MicroTimestamp = MicroTimestamp::from_micros(1_800_000_000_000_000);

fn trace(n: u8) -> TraceId {
    TraceId::from([n; 16])
}

/// Trace `n`, started `ago` before [`NOW`] and taking `ms` milliseconds.
fn point(n: u8, ago: Duration, ms: u64, status: TraceStatus) -> TracePoint {
    TracePoint {
        trace_id: trace(n),
        start: MicroTimestamp::from_micros(
            NOW.as_micros() - i64::try_from(ago.as_micros()).unwrap(),
        ),
        duration: Duration::from_millis(ms),
        status,
    }
}

fn summary(point: TracePoint, name: &str) -> TraceSummary {
    TraceSummary {
        trace_id: point.trace_id,
        start: point.start,
        name: name.into(),
        stack: "notes".into(),
        services: vec!["hoster".into()],
        spans: 2,
        duration: point.duration,
        status: point.status,
        proxy_only: true,
    }
}

fn points(points: Vec<TracePoint>) -> TracePoints {
    TracePoints {
        matching: points.len() as u64,
        slowest: points.iter().map(|p| p.duration).max().unwrap_or_default(),
        points,
    }
}

fn count(chart: &str, needle: &str) -> usize {
    chart.matches(needle).count()
}

#[test]
fn the_scatter_draws_exact_svg_with_the_listed_traces_linked_on_top() {
    let a = point(1, 15 * MINUTE, 1, TraceStatus::Ok);
    let b = point(2, 5 * MINUTE, 100, TraceStatus::Error);
    let c = point(3, MINUTE * 15 / 2, 10, TraceStatus::Ok);
    let d = point(4, MINUTE, 1_910, TraceStatus::Error);
    let mut chart = DurationScatter::new(NOW, 15 * MINUTE, points(vec![a, b, c, d]));
    chart.list(
        &summary(d, "POST /api/x"),
        "/t?trace=04&page=1".into(),
        true,
    );
    chart.list(&summary(c, "GET /a"), "/t?trace=03&page=1".into(), false);
    assert_eq!(
        chart.to_string(),
        concat!(
            r#"<div class="chart chart-scatter" role="group" aria-label="Trace duration of root spans over the last 15m, log scale: 4 traces, 2 listed below; slowest error POST /api/x, 1.91 s, 1m ago">"#,
            r#"<div class="chart-y" aria-hidden="true"><span>10 s</span><span>1 s</span><span>100 ms</span><span>10 ms</span><span>1 ms</span></div>"#,
            r#"<svg class="chart-plot" width="600" height="180">"#,
            r#"<line class="chart-grid" x1="0" y1="0%" x2="100%" y2="0%"></line>"#,
            r#"<line class="chart-grid" x1="0" y1="25%" x2="100%" y2="25%"></line>"#,
            r#"<line class="chart-grid" x1="0" y1="50%" x2="100%" y2="50%"></line>"#,
            r#"<line class="chart-grid" x1="0" y1="75%" x2="100%" y2="75%"></line>"#,
            r#"<g class="scatter-points" aria-hidden="true">"#,
            r#"<circle class="scatter-dot scatter-ok" cx="0%" cy="100%" r="3"></circle>"#,
            r#"<circle class="scatter-dot scatter-error" cx="66.67%" cy="50%" r="3"></circle>"#,
            "</g>",
            r#"<a class="scatter-link" href="/t?trace=03&#38;page=1" aria-label="Open trace GET /a, 10.0 ms"><title>Open trace GET /a, 10.0 ms</title>"#,
            r#"<circle class="scatter-hit" cx="50%" cy="75%" r="10"></circle>"#,
            r#"<circle class="scatter-mark" cx="50%" cy="75%" r="4"></circle></a>"#,
            r#"<a class="scatter-link scatter-current" href="/t?trace=04&#38;page=1" aria-label="Open trace POST /api/x, 1.91 s" aria-current="true"><title>Open trace POST /api/x, 1.91 s</title>"#,
            r#"<circle class="scatter-hit" cx="93.33%" cy="17.97%" r="10"></circle>"#,
            r#"<circle class="scatter-mark scatter-error" cx="93.33%" cy="17.97%" r="6"></circle></a>"#,
            r#"<line class="chart-axis" x1="0" y1="100%" x2="100%" y2="100%"></line></svg>"#,
            r#"<div class="chart-x" aria-hidden="true"><span>-15m</span><span>-10m</span><span>-5m</span><span>now</span></div></div>"#,
        )
    );
    assert_eq!(chart.heading(), "root spans · last 15m · log scale");
}

#[test]
fn the_legend_has_ok_error_and_the_listed_ring() {
    let chart = DurationScatter::new(NOW, 15 * MINUTE, TracePoints::default());
    assert_eq!(
        chart.legend().to_string(),
        concat!(
            r#"<ul class="chart-legend">"#,
            r#"<li><span class="chart-key chart-key-dot scatter-ok" aria-hidden="true"></span>ok</li>"#,
            r#"<li><span class="chart-key chart-key-dot scatter-error" aria-hidden="true"></span>error</li>"#,
            r#"<li><span class="chart-key chart-key-ring scatter-listed" aria-hidden="true"></span>listed below</li>"#,
            "</ul>",
        )
    );
}

#[test]
fn the_log_scale_spans_whole_decades_from_the_bottom_to_the_slowest_at_least_the_minimum() {
    let decades = |max: f64| LogScale::new(max, 1e-3, 3).decades;
    // 1 ms to 1 s at least, whatever the maximum.
    assert_eq!(decades(0.0), 3);
    assert_eq!(decades(f64::NAN), 3);
    assert_eq!(decades(0.000_5), 3);
    assert_eq!(decades(0.05), 3);
    assert_eq!(decades(1.0), 3);
    // The first power of ten at or above the slowest.
    assert_eq!(decades(1.000_1), 4);
    assert_eq!(decades(1.91), 4);
    assert_eq!(decades(10.0), 4);
    assert_eq!(decades(3_600.0), 7);
    // A hair below a power of ten from rounding is that power.
    assert_eq!(decades(0.1 * 10.0 * 1.000_000_000_01), 3);

    let scale = LogScale::new(1.91, 1e-3, 3);
    let ticks: Vec<String> = scale.ticks().map(DurationScatter::decade).collect();
    assert_eq!(ticks, ["10 s", "1 s", "100 ms", "10 ms", "1 ms"]);
    // A decade every quarter of the height; the bottom and below on the axis, the top and
    // above at the top.
    let close = |a: f64, b: f64| assert!((a - b).abs() < 1e-9, "{a} != {b}");
    close(scale.y(10.0), 0.0);
    close(scale.y(1.0), HEIGHT / 4.0);
    close(scale.y(0.1), HEIGHT / 2.0);
    close(scale.y(0.01), HEIGHT * 3.0 / 4.0);
    close(scale.y(0.001), HEIGHT);
    close(scale.y(0.000_1), HEIGHT);
    close(scale.y(0.0), HEIGHT);
    close(scale.y(100.0), 0.0);
    // Log, not linear: 316 ms is half a decade above 100 ms.
    close(scale.y(0.1 * 10f64.sqrt()), HEIGHT * 3.0 / 8.0);
}

#[test]
fn decades_read_as_milliseconds_below_a_second_and_seconds_from_it() {
    let labels: Vec<String> = LogScale::new(3_600.0, 1e-3, 3)
        .ticks()
        .map(DurationScatter::decade)
        .collect();
    assert_eq!(
        labels,
        [
            "10,000 s", "1,000 s", "100 s", "10 s", "1 s", "100 ms", "10 ms", "1 ms"
        ]
    );
}

#[test]
fn the_x_axis_has_four_labels_from_the_start_of_each_range_to_now() {
    let labels = |length: Duration| {
        DurationScatter::new(NOW, length, TracePoints::default())
            .to_string()
            .split(r#"<div class="chart-x" aria-hidden="true">"#)
            .nth(1)
            .unwrap()
            .to_owned()
    };
    let spans = |list: &[&str]| {
        let mut html: String = list.iter().map(|l| format!("<span>{l}</span>")).collect();
        html.push_str("</div></div>");
        html
    };
    assert_eq!(
        labels(5 * MINUTE),
        spans(&["-5m", "-3m 20s", "-1m 40s", "now"])
    );
    assert_eq!(labels(15 * MINUTE), spans(&["-15m", "-10m", "-5m", "now"]));
    assert_eq!(labels(60 * MINUTE), spans(&["-1h", "-40m", "-20m", "now"]));
    assert_eq!(labels(360 * MINUTE), spans(&["-6h", "-4h", "-2h", "now"]));
    assert_eq!(
        labels(1_440 * MINUTE),
        spans(&["-24h", "-16h", "-8h", "now"])
    );
}

#[test]
fn points_outside_the_range_lie_on_its_edges() {
    let early = point(1, 20 * MINUTE, 50, TraceStatus::Ok);
    // A clock slightly ahead: a start past now.
    let ahead = TracePoint {
        start: NOW + Duration::from_millis(500),
        ..point(2, MINUTE, 50, TraceStatus::Ok)
    };
    let chart = DurationScatter::new(NOW, 15 * MINUTE, points(vec![early, ahead])).to_string();
    assert!(chart.contains(r#"cx="0%""#), "{chart}");
    assert!(chart.contains(r#"cx="100%""#), "{chart}");
}

#[test]
fn sampled_points_are_drawn_as_given_and_the_chart_says_how_many_of_how_many() {
    let cap = TracePoints::CAP as usize;
    let sampled = TracePoints {
        points: (0..cap)
            .map(|i| {
                let status = if i % 100 == 0 {
                    TraceStatus::Error
                } else {
                    TraceStatus::Ok
                };
                let ago = 15 * MINUTE * u32::try_from(i).unwrap() / TracePoints::CAP;
                point((i % 250) as u8, ago, 1 + (i as u64 % 900), status)
            })
            .collect(),
        matching: 18_204,
        // A 30 s root sampling left out.
        slowest: Duration::from_secs(30),
    };
    let chart = DurationScatter::new(NOW, 15 * MINUTE, sampled);
    let html = chart.to_string();
    assert_eq!(count(&html, r#"<circle class="scatter-dot "#), cap);
    assert_eq!(count(&html, r#"class="scatter-dot scatter-error""#), 10);
    assert_eq!(count(&html, "<a "), 0);
    // Nothing is a link: one image. Its error is the slowest drawn, the only statuses known.
    assert!(html.starts_with(r#"<div class="chart chart-scatter" role="img" aria-label="Trace duration of root spans over the last 15m, log scale: 18,204 traces, 1,000 of them drawn; slowest error drawn 801 ms, "#), "{html}");
    // The axis reaches the slowest root of the range, which is not drawn: up to 100 s.
    assert!(
        html.contains(r#"<div class="chart-y" aria-hidden="true"><span>100 s</span><span>10 s</span><span>1 s</span>"#),
        "{html}"
    );
    let ok_only = TracePoints {
        points: vec![point(1, MINUTE, 40, TraceStatus::Ok)],
        matching: 2,
        slowest: Duration::from_millis(40),
    };
    let label = DurationScatter::new(NOW, 15 * MINUTE, ok_only).label();
    assert!(
        label.ends_with(": 2 traces, 1 of them drawn; no errors drawn"),
        "{label}"
    );
    assert_eq!(
        chart.heading(),
        "root spans · last 15m · log scale · 1,000 of 18,204 drawn"
    );
}

#[test]
fn an_empty_range_is_an_empty_plot_from_1_ms_to_1_s() {
    let chart = DurationScatter::new(NOW, 15 * MINUTE, TracePoints::default());
    let html = chart.to_string();
    assert!(html.starts_with(concat!(
        r#"<div class="chart chart-scatter" role="img" aria-label="Trace duration of root spans over the last 15m, log scale: no traces">"#,
        r#"<div class="chart-y" aria-hidden="true"><span>1 s</span><span>100 ms</span><span>10 ms</span><span>1 ms</span></div>"#,
    )));
    assert!(
        html.contains(
            r#"<g class="scatter-points" aria-hidden="true"></g><line class="chart-axis""#
        )
    );
    assert_eq!(chart.heading(), "root spans · last 15m · log scale");
}

#[test]
fn the_label_says_when_nothing_failed_and_names_an_unlisted_slowest_error_by_time_alone() {
    let one = DurationScatter::new(
        NOW,
        15 * MINUTE,
        points(vec![point(1, MINUTE, 40, TraceStatus::Ok)]),
    );
    assert!(
        one.label().ends_with(": 1 trace; no errors"),
        "{}",
        one.label()
    );
    let slow = point(
        2,
        3 * MINUTE + Duration::from_secs(12),
        2_400,
        TraceStatus::Error,
    );
    let quick = point(3, MINUTE, 300, TraceStatus::Error);
    let mut chart = DurationScatter::new(NOW, 15 * MINUTE, points(vec![slow, quick]));
    chart.list(&summary(quick, "GET /quick"), "/t?trace=03".into(), false);
    assert!(
        chart
            .label()
            .ends_with(": 2 traces, 1 listed below; slowest error 2.40 s, 3m 12s ago"),
        "{}",
        chart.label()
    );
    // A listed trace is drawn once, as its link, not also as a background dot.
    let html = chart.to_string();
    assert_eq!(count(&html, r#"<circle class="scatter-dot "#), 1);
    assert_eq!(count(&html, r#"<a class="scatter-link""#), 1);
}
