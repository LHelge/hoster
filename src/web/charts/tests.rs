use super::*;

use StatusClass::{ClientError, Informational, NoResponse, Redirection, ServerError, Success};

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3_600);

fn counts(pairs: &[(StatusClass, u64)]) -> StatusCounts {
    pairs.iter().copied().collect()
}

fn ms(n: u64) -> Option<Duration> {
    Some(Duration::from_millis(n))
}

/// The SVG between the grid lines and the x axis line: what a chart draws for its data.
fn plot(chart: &str) -> &str {
    let grid = chart.rfind(r#"<line class="chart-grid""#).unwrap();
    let start = grid + chart[grid..].find("</line>").unwrap() + "</line>".len();
    let end = chart.find(r#"<line class="chart-axis""#).unwrap();
    &chart[start..end]
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
fn stacked_bars_draw_exact_svg_for_a_small_series() {
    let chart = StackedBars::new(
        Timeline::new(2, HOUR),
        [
            counts(&[(Success, 60), (ServerError, 30)]),
            counts(&[(Success, 45), (NoResponse, 1)]),
        ],
    )
    .of("notes");
    assert_eq!(
        chart.to_string(),
        concat!(
            r#"<div class="chart chart-bars" role="img" aria-label="Responses by status class for notes over the last 2h: 136 requests, 2xx 105, 3xx 0, 4xx 0, 5xx 30, other 1; busiest -2h to -1h with 90; most 5xx -2h to -1h with 30">"#,
            r#"<div class="chart-y" aria-hidden="true"><span>90</span><span>60</span><span>30</span><span>0</span></div>"#,
            r#"<svg class="chart-plot" width="600" height="180" viewBox="0 0 600 180" preserveAspectRatio="none">"#,
            r#"<line class="chart-grid" x1="0" y1="0" x2="600" y2="0" vector-effect="non-scaling-stroke"></line>"#,
            r#"<line class="chart-grid" x1="0" y1="60" x2="600" y2="60" vector-effect="non-scaling-stroke"></line>"#,
            r#"<line class="chart-grid" x1="0" y1="120" x2="600" y2="120" vector-effect="non-scaling-stroke"></line>"#,
            r#"<g class="chart-bar"><title>-2h to -1h: 90 requests, 2xx 60, 5xx 30</title>"#,
            r#"<rect class="chart-hit" x="0" y="0" width="300" height="180"></rect>"#,
            r#"<rect class="chart-seg chart-2xx" x="30" y="60" width="240" height="120" vector-effect="non-scaling-stroke"></rect>"#,
            r#"<rect class="chart-seg chart-5xx" x="30" y="0" width="240" height="60" vector-effect="non-scaling-stroke"></rect></g>"#,
            r#"<g class="chart-bar"><title>-1h to now: 46 requests, 2xx 45, other 1</title>"#,
            r#"<rect class="chart-hit" x="300" y="0" width="300" height="180"></rect>"#,
            r#"<rect class="chart-seg chart-2xx" x="330" y="90" width="240" height="90" vector-effect="non-scaling-stroke"></rect>"#,
            r#"<rect class="chart-seg chart-other" x="330" y="88" width="240" height="2" vector-effect="non-scaling-stroke"></rect></g>"#,
            r#"<line class="chart-axis" x1="0" y1="180" x2="600" y2="180" vector-effect="non-scaling-stroke"></line></svg>"#,
            r#"<div class="chart-x" aria-hidden="true"><span>-2h</span><span>-1h</span><span>now</span></div></div>"#,
        )
    );
    assert_eq!(
        chart.legend().to_string(),
        concat!(
            r#"<ul class="chart-legend">"#,
            r#"<li><span class="chart-key chart-key-box chart-2xx" aria-hidden="true"></span>2xx 105</li>"#,
            r#"<li><span class="chart-key chart-key-box chart-3xx" aria-hidden="true"></span>3xx 0</li>"#,
            r#"<li><span class="chart-key chart-key-box chart-4xx" aria-hidden="true"></span>4xx 0</li>"#,
            r#"<li><span class="chart-key chart-key-box chart-5xx" aria-hidden="true"></span>5xx 30</li>"#,
            r#"<li><span class="chart-key chart-key-box chart-other" aria-hidden="true"></span>other 1</li>"#,
            "</ul>",
        )
    );
}

#[test]
fn segments_stack_two_to_five_hundreds_bottom_to_top() {
    let chart = StackedBars::new(
        Timeline::new(1, HOUR),
        [counts(&[
            (Informational, 1),
            (ServerError, 2),
            (ClientError, 3),
            (Redirection, 4),
            (Success, 5),
        ])],
    )
    .to_string();
    let classes: Vec<&str> = chart
        .match_indices(r#"class="chart-seg "#)
        .map(|(at, m)| {
            let rest = &chart[at + m.len()..];
            &rest[..rest.find('"').unwrap()]
        })
        .collect();
    assert_eq!(
        classes,
        [
            "chart-2xx",
            "chart-3xx",
            "chart-4xx",
            "chart-5xx",
            "chart-other"
        ]
    );
    // Each segment starts where the one below it ends.
    let ys: Vec<f64> = attrs(&chart, "chart-seg chart-2xx", "y")
        .into_iter()
        .chain(attrs(&chart, "chart-seg chart-3xx", "y"))
        .map(|y| y.parse().unwrap())
        .collect();
    let heights: Vec<f64> = attrs(&chart, "chart-seg chart-3xx", "height")
        .into_iter()
        .map(|h| h.parse().unwrap())
        .collect();
    assert!((ys[1] + heights[0] - ys[0]).abs() < 1e-9);
}

#[test]
fn the_tallest_bar_reaches_the_plot_height_at_a_round_maximum() {
    // 90 is a round maximum (30 a step): its bar fills the plot, the others scale with it.
    let chart = StackedBars::new(
        Timeline::new(3, MINUTE),
        [30, 90, 45].map(|n| counts(&[(Success, n)])),
    )
    .to_string();
    assert_eq!(
        attrs(&chart, "chart-seg chart-2xx", "height"),
        ["60", "180", "90"]
    );
    assert_eq!(
        attrs(&chart, "chart-seg chart-2xx", "y"),
        ["120", "0", "90"]
    );
    // Otherwise the axis rounds up, and no bar goes past it.
    let chart = StackedBars::new(
        Timeline::new(2, MINUTE),
        [1_000, 873].map(|n| counts(&[(Success, n)])),
    )
    .to_string();
    assert!(chart.contains("<span>1.2k</span><span>800</span><span>400</span><span>0</span>"));
    assert_eq!(
        attrs(&chart, "chart-seg chart-2xx", "height"),
        ["150", "130.95"]
    );
}

#[test]
fn an_empty_series_draws_its_axes_without_bars() {
    for chart in [
        StackedBars::new(Timeline::from(Range::Day), []).to_string(),
        StackedBars::new(Timeline::new(3, MINUTE), [StatusCounts::default(); 3]).to_string(),
    ] {
        assert!(!chart.contains("NaN") && !chart.contains("inf"), "{chart}");
        assert!(!chart.contains("chart-seg"));
        assert!(chart.contains(r#"class="chart-axis""#));
        assert_eq!(chart.matches(r#"class="chart-grid""#).count(), 3);
        assert!(chart.contains("<span>3</span><span>2</span><span>1</span><span>0</span>"));
        assert!(chart.contains(": no requests\""));
        assert!(chart.contains(": no requests</title>"));
    }
    // A whole range's bars are there, empty: 24 for 24h.
    let day = StackedBars::new(Timeline::from(Range::Day), []).to_string();
    assert_eq!(day.matches(r#"class="chart-hit""#).count(), 24);
    assert!(day.contains("<span>-24h</span><span>-18h</span>"));
}

#[test]
fn a_single_spike_is_the_peak_in_the_label() {
    let mut bars = [StatusCounts::default(); 24];
    bars[14] = counts(&[(Success, 400), (ServerError, 1_203)]);
    let chart = StackedBars::new(Timeline::from(Range::Day), bars).to_string();
    assert!(chart.contains(
        "over the last 24h: 1,603 requests, 2xx 400, 3xx 0, 4xx 0, 5xx 1,203; busiest -10h to -9h with 1,603; most 5xx -10h to -9h with 1,203\""
    ));
    assert_eq!(chart.matches("chart-seg").count(), 2);
    assert!(chart.contains("<title>-10h to -9h: 1,603 requests, 2xx 400, 5xx 1,203</title>"));
}

#[test]
fn a_shorter_series_fills_the_newest_bars() {
    let chart = StackedBars::new(Timeline::new(3, MINUTE), [counts(&[(Success, 3)])]).to_string();
    assert_eq!(attrs(&chart, "chart-seg chart-2xx", "x"), ["420"]);
    let chart = StackedBars::new(
        Timeline::new(1, MINUTE),
        [counts(&[(Success, 3)]), counts(&[(Success, 6)])],
    );
    assert_eq!(chart.bars, [counts(&[(Success, 6)])]);
}

#[test]
fn the_subject_is_escaped() {
    let bars = StackedBars::new(Timeline::new(1, MINUTE), []).of(r#"<x> "y""#);
    assert!(
        bars.to_string()
            .contains(r#"aria-label="Responses by status class for &#60;x&#62; &#34;y&#34; over"#)
    );
    let lines = LineChart::new(Timeline::new(1, MINUTE), []).of("<x>");
    assert!(
        lines
            .to_string()
            .contains(r#"aria-label="Response time for &#60;x&#62; over"#)
    );
    for chart in [bars.to_string(), lines.to_string()] {
        assert!(!chart.contains("<x>") && !chart.contains("style="));
    }
}

#[test]
fn a_line_chart_draws_exact_svg_and_breaks_where_a_bucket_has_no_samples() {
    let chart = LineChart::new(
        Timeline::new(4, MINUTE),
        [
            Percentiles {
                p50: ms(40),
                p95: ms(300),
            },
            Percentiles {
                p50: ms(30),
                p95: ms(150),
            },
            Percentiles::default(),
            Percentiles {
                p50: ms(20),
                p95: ms(90),
            },
        ],
    );
    let svg = chart.to_string();
    assert!(svg.starts_with(concat!(
        r#"<div class="chart chart-lines" role="img" aria-label="Response time over the last 4m: p50 up to 40 ms (-4m to -3m), p95 up to 300 ms (-4m to -3m)">"#,
        r#"<div class="chart-y" aria-hidden="true"><span>300 ms</span><span>200 ms</span><span>100 ms</span><span>0</span></div>"#,
    )));
    assert_eq!(
        plot(&svg),
        concat!(
            r#"<g class="chart-bar"><title>-4m to -3m: p50 40 ms, p95 300 ms</title><rect class="chart-hit" x="0" y="0" width="150" height="180"></rect></g>"#,
            r#"<g class="chart-bar"><title>-3m to -2m: p50 30 ms, p95 150 ms</title><rect class="chart-hit" x="150" y="0" width="150" height="180"></rect></g>"#,
            r#"<g class="chart-bar"><title>-2m to -1m: no samples</title><rect class="chart-hit" x="300" y="0" width="150" height="180"></rect></g>"#,
            r#"<g class="chart-bar"><title>-1m to now: p50 20 ms, p95 90 ms</title><rect class="chart-hit" x="450" y="0" width="150" height="180"></rect></g>"#,
            r#"<polyline class="chart-line chart-p95" points="75,0 225,90 375,180 525,126" vector-effect="non-scaling-stroke"></polyline>"#,
            r#"<polyline class="chart-line chart-p50" points="75,156 225,162 375,180 525,168" vector-effect="non-scaling-stroke"></polyline>"#,
        )
    );
    assert!(svg.ends_with(r#"<div class="chart-x" aria-hidden="true"><span>-4m</span><span>-3m</span><span>-2m</span><span>-1m</span><span>now</span></div></div>"#));
    assert_eq!(
        chart.legend().to_string(),
        concat!(
            r#"<ul class="chart-legend">"#,
            r#"<li><span class="chart-key chart-key-line chart-p50" aria-hidden="true"></span>p50</li>"#,
            r#"<li><span class="chart-key chart-key-line chart-p95" aria-hidden="true"></span>p95</li>"#,
            "</ul>",
        )
    );
}

#[test]
fn a_line_chart_without_samples_draws_its_axes_and_no_lines() {
    let chart = LineChart::new(Timeline::from(Range::Hour), []).to_string();
    assert!(!chart.contains("NaN") && !chart.contains("inf"));
    assert!(!chart.contains("polyline"));
    assert!(chart.contains("over the last 1h: no samples\""));
    assert!(
        chart.contains("<span>0.3 ms</span><span>0.2 ms</span><span>0.1 ms</span><span>0</span>")
    );
    assert_eq!(chart.matches(r#"class="chart-hit""#).count(), 60);
}

#[test]
fn a_single_latency_spike_reaches_the_top_at_a_round_maximum() {
    let mut points = [Percentiles {
        p50: ms(10),
        p95: ms(20),
    }; 6];
    points[2].p95 = ms(600);
    let chart = LineChart::new(Timeline::from(Range::SixHours), points).to_string();
    let p95 = attrs(&chart, "chart-line chart-p95", "points");
    assert_eq!(p95.len(), 1);
    assert!(p95[0].split(' ').any(|xy| xy.ends_with(",0")));
    // Six points fill the newest six of 72 bars: the spike is the third, 20 to 15 min ago.
    assert!(chart.contains("p95 up to 600 ms (-20m to -15m)"));
    assert!(chart.contains("<span>600 ms</span><span>400 ms</span>"));
}

#[test]
fn a_sparkline_scales_from_zero_to_its_maximum() {
    assert_eq!(
        Sparkline::new([0, 5, 10], Tone::Warn).to_string(),
        r#"<svg class="spark spark-warn" aria-hidden="true" width="96" height="28" viewBox="0 0 96 28" preserveAspectRatio="none"><polyline points="0,26.5 48,14 96,1.5" vector-effect="non-scaling-stroke"></polyline></svg>"#
    );
    // Teal by default, muted when stopped.
    assert!(
        Sparkline::new([1], Tone::Ok)
            .to_string()
            .starts_with(r#"<svg class="spark spark-ok""#)
    );
    assert!(
        Sparkline::new([1], Tone::Neutral)
            .to_string()
            .contains("spark-neutral")
    );
}

#[test]
fn a_sparkline_without_data_is_a_flat_line() {
    let flat = r#"points="0,26.5 96,26.5""#;
    assert!(Sparkline::new([], Tone::Ok).to_string().contains(flat));
    assert!(
        Sparkline::new([0, 0, 0], Tone::Ok)
            .to_string()
            .contains(r#"points="0,26.5 48,26.5 96,26.5""#)
    );
    // One value is a level line at its height.
    assert!(
        Sparkline::new([7], Tone::Ok)
            .to_string()
            .contains(r#"points="0,1.5 96,1.5""#)
    );
}

#[test]
fn every_range_gets_round_axis_labels() {
    let ticks = |range| {
        Timeline::from(range)
            .ticks()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(ticks(Range::Hour), ["-1h", "-45m", "-30m", "-15m", "now"]);
    assert_eq!(ticks(Range::SixHours), ["-6h", "-4h", "-2h", "now"]);
    assert_eq!(ticks(Range::Day), ["-24h", "-18h", "-12h", "-6h", "now"]);
    assert_eq!(
        ticks(Range::Week),
        ["-7d", "-6d", "-5d", "-4d", "-3d", "-2d", "-24h", "now"]
    );
    assert_eq!(ticks(Range::ThirtyDays), ["-30d", "-20d", "-10d", "now"]);
    // A length no interval divides into round labels gets its ends only.
    assert_eq!(
        Timeline::new(7, Duration::from_secs(61))
            .ticks()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["-7m 7s", "now"]
    );
}

#[test]
fn bars_are_named_by_their_span() {
    let six = Timeline::from(Range::SixHours);
    let (from, to) = six.span_of(0);
    assert_eq!(
        (from.to_string(), to.to_string()),
        ("-6h".into(), "-5h 55m".into())
    );
    let week = Timeline::from(Range::Week);
    assert_eq!(week.span_of(55).0.to_string(), "-3h");
    assert_eq!(week.span_of(0).1.to_string(), "-6d 21h");
}

#[test]
fn scales_round_up_to_a_few_steps() {
    let top = |max, unit| Scale::new(max, unit).top();
    assert_eq!(top(90_000.0, 1.0), 90_000.0);
    assert_eq!(top(84_000.0, 1.0), 90_000.0);
    assert_eq!(top(91_000.0, 1.0), 120_000.0);
    assert_eq!(top(7.0, 1.0), 9.0);
    assert_eq!(top(1.0, 1.0), 3.0);
    assert_eq!(top(0.0, 1.0), 3.0);
    assert_eq!(top(f64::NAN, 1.0), 3.0);
    // No fractional steps of a whole unit: 7 would be 2.5 a step, 3 is the next whole one.
    assert_eq!(Scale::new(7.0, 1.0).step, 3.0);
    assert!((top(0.412, 0.000_1) - 0.45).abs() < 1e-12);
    let compact = |v| Compact(v).to_string();
    assert_eq!(compact(750.0), "750");
    assert_eq!(compact(2_500.0), "2.5k");
    assert_eq!(compact(90_000.0), "90k");
    assert_eq!(compact(1_200_000.0), "1.2M");
}

#[test]
fn coordinates_are_written_to_two_decimals() {
    assert_eq!(Num(180.0).to_string(), "180");
    assert_eq!(Num(12.5).to_string(), "12.5");
    assert_eq!(Num(1.0 / 3.0).to_string(), "0.33");
    assert_eq!(Num(-0.001).to_string(), "0");
}

#[test]
fn percentiles_come_from_a_histogram() {
    let mut latency = Histogram::default();
    assert_eq!(Percentiles::from(&latency), Percentiles::default());
    latency.record(Duration::from_millis(12));
    let points = Percentiles::from(&latency);
    assert!(points.p50.is_some() && points.p95.is_some());
}
