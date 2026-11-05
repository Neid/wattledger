//! Server-rendered SVG column charts. Every bucket is a focusable hit area
//! carrying a `data-tip` that `app.js` shows as a tooltip; the same numbers
//! are in a table next to the chart.

use maud::{Markup, html};

const H: f64 = 220.0;
const LEFT: f64 = 52.0;
const RIGHT: f64 = 8.0;
const TOP: f64 = 10.0;
const BOTTOM: f64 = 24.0;
const MAX_BAR: f64 = 24.0;
const GAP: f64 = 2.0;
const RADIUS: f64 = 4.0;

pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

pub struct ColumnChart<'a> {
    /// Accessible name of the chart.
    pub title: &'a str,
    pub unit: &'a str,
    pub decimals: usize,
    pub labels: Vec<String>,
    /// One or two series (palette slots 1 and 2).
    pub series: Vec<Series>,
    /// Show every n-th x label.
    pub label_every: usize,
    /// Bucket to mark (e.g. the current price slot).
    pub highlight: Option<usize>,
    /// Drawing width; pick about the width it is shown at, so text stays
    /// close to its CSS size (the SVG scales evenly).
    pub width: f64,
}

/// Round tick values covering `[min(lo, 0), max(hi, 0)]`.
pub fn nice_ticks(lo: f64, hi: f64) -> Vec<f64> {
    let (lo, hi) = (lo.min(0.0), hi.max(0.0));
    if !(hi - lo).is_finite() || hi - lo < 1e-12 {
        return vec![0.0, 1.0];
    }
    let raw = (hi - lo) / 5.0;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw - 1e-12)
        .unwrap_or(10.0 * mag);
    let first = (lo / step + 1e-9).floor() as i64;
    let last = (hi / step - 1e-9).ceil() as i64;
    (first..=last)
        .map(|k| (k as f64 * step * 1e9).round() / 1e9)
        .collect()
}

fn fmt_tick(v: f64, ticks: &[f64]) -> String {
    let decimals: usize = (0..=3usize)
        .find(|d| {
            let f = 10f64.powi(*d as i32);
            ticks
                .iter()
                .all(|t| ((t * f) - (t * f).round()).abs() < 1e-6)
        })
        .unwrap_or(3);
    format!("{v:.decimals$}")
}

/// Column path with a rounded data end and a square baseline end.
fn bar_path(x: f64, w: f64, y0: f64, y1: f64) -> String {
    let h = (y0 - y1).abs();
    let r = RADIUS.min(w / 2.0).min(h);
    if y1 < y0 {
        format!(
            "M{x:.1},{y0:.1}V{:.1}Q{x:.1},{y1:.1} {:.1},{y1:.1}H{:.1}Q{:.1},{y1:.1} {:.1},{:.1}V{y0:.1}Z",
            y1 + r,
            x + r,
            x + w - r,
            x + w,
            x + w,
            y1 + r
        )
    } else {
        format!(
            "M{x:.1},{y0:.1}V{:.1}Q{x:.1},{y1:.1} {:.1},{y1:.1}H{:.1}Q{:.1},{y1:.1} {:.1},{:.1}V{y0:.1}Z",
            y1 - r,
            x + r,
            x + w - r,
            x + w,
            x + w,
            y1 - r
        )
    }
}

pub fn render(c: &ColumnChart) -> Markup {
    let w = c.width.max(200.0);
    let n = c.labels.len().max(1);
    let all = c.series.iter().flat_map(|s| s.values.iter().copied());
    let (lo, hi) = all.fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let ticks = nice_ticks(lo, hi);
    let (tmin, tmax) = (ticks[0], ticks[ticks.len() - 1]);
    let plot_h = H - TOP - BOTTOM;
    let y = |v: f64| TOP + (tmax - v) / (tmax - tmin) * plot_h;
    let y0 = y(0.0);
    let band = (w - LEFT - RIGHT) / n as f64;
    let k = c.series.len().max(1) as f64;
    let bw = ((band * 0.8 - GAP * (k - 1.0)) / k).clamp(1.0, MAX_BAR);
    let group_w = k * bw + (k - 1.0) * GAP;
    let every = c.label_every.max(1);
    let fmt = |v: f64| format!("{v:.*}", c.decimals);

    html! {
        figure.chart {
            svg viewBox=(format!("0 0 {w} {H}")) role="img" aria-label=(c.title) {
                @for t in &ticks {
                    line class=(if *t == 0.0 { "baseline" } else { "grid" })
                        x1=(LEFT) x2=(w - RIGHT) y1=(format!("{:.1}", y(*t))) y2=(format!("{:.1}", y(*t))) {}
                    text.tick x=(LEFT - 6.0) y=(format!("{:.1}", y(*t) + 4.0)) { (fmt_tick(*t, &ticks)) }
                }
                @if let Some(h) = c.highlight.filter(|h| *h < n) {
                    rect.now x=(format!("{:.1}", LEFT + h as f64 * band)) y=(TOP) width=(format!("{band:.1}")) height=(plot_h) {}
                }
                @for (i, label) in c.labels.iter().enumerate() {
                    @let x0 = LEFT + i as f64 * band + (band - group_w) / 2.0;
                    @for (j, s) in c.series.iter().enumerate() {
                        @let v = s.values.get(i).copied().unwrap_or(0.0);
                        @if v != 0.0 {
                            @let x = x0 + j as f64 * (bw + GAP);
                            path class=(format!("bar s{}{}", j + 1, if v < 0.0 { " neg" } else { "" }))
                                d=(bar_path(x, bw, y0, y(v))) {}
                        }
                    }
                    @if i % every == 0 {
                        text.xlabel x=(format!("{:.1}", LEFT + (i as f64 + 0.5) * band)) y=(H - 6.0) { (label) }
                    }
                }
                // Hit areas last, so they sit on top of the bars.
                @for (i, label) in c.labels.iter().enumerate() {
                    @let tip = std::iter::once(label.clone())
                        .chain(c.series.iter().map(|s| {
                            format!("{}: {} {}", s.name, fmt(s.values.get(i).copied().unwrap_or(0.0)), c.unit)
                        }))
                        .collect::<Vec<_>>()
                        .join("|");
                    rect.hit x=(format!("{:.1}", LEFT + i as f64 * band)) y=(TOP)
                        width=(format!("{band:.1}")) height=(plot_h) tabindex="0" data-tip=(tip) {}
                }
            }
            @if c.series.len() > 1 {
                figcaption.legend {
                    @for (j, s) in c.series.iter().enumerate() {
                        span { i class=(format!("swatch s{}", j + 1)) {} (s.name) }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks() {
        assert_eq!(nice_ticks(0.0, 7.3), [0.0, 2.0, 4.0, 6.0, 8.0]);
        assert_eq!(
            nice_ticks(-11.0, 990.0),
            [-250.0, 0.0, 250.0, 500.0, 750.0, 1000.0]
        );
        assert_eq!(nice_ticks(0.0, 0.0), [0.0, 1.0]);
        assert_eq!(nice_ticks(0.2, 0.9), [0.0, 0.2, 0.4, 0.6, 0.8, 1.0]);
        let t = nice_ticks(-3.0, -1.0);
        assert_eq!(t.last(), Some(&0.0));
        assert!(t[0] <= -3.0);
    }

    fn chart(series: Vec<Series>) -> String {
        render(&ColumnChart {
            title: "Grid",
            unit: "kWh",
            decimals: 1,
            labels: vec!["00:00".into(), "01:00".into(), "02:00".into()],
            series,
            label_every: 1,
            highlight: Some(1),
            width: 560.0,
        })
        .into_string()
    }

    #[test]
    fn one_hit_area_per_bucket_with_every_series_in_the_tip() {
        let svg = chart(vec![
            Series {
                name: "Import".into(),
                values: vec![1.0, 2.5, 0.0],
            },
            Series {
                name: "Export".into(),
                values: vec![0.0, 0.26, 3.0],
            },
        ]);
        assert!(svg.starts_with("<figure"), "{svg}");
        assert_eq!(svg.matches("class=\"hit\"").count(), 3);
        assert!(
            svg.contains("data-tip=\"01:00|Import: 2.5 kWh|Export: 0.3 kWh\""),
            "{svg}"
        );
        assert!(svg.contains("tabindex=\"0\""));
        assert_eq!(
            svg.matches("class=\"bar s1\"").count(),
            2,
            "zero values draw no bar"
        );
        assert_eq!(svg.matches("class=\"bar s2\"").count(), 2);
        assert!(svg.contains("class=\"legend\""), "two series need a legend");
        assert!(
            svg.contains("class=\"now\""),
            "highlighted bucket is marked"
        );
        assert!(svg.contains("aria-label=\"Grid\""));
    }

    #[test]
    fn single_series_has_no_legend() {
        let svg = chart(vec![Series {
            name: "Price".into(),
            values: vec![1.0, 2.0, 3.0],
        }]);
        assert!(!svg.contains("class=\"legend\""));
    }

    #[test]
    fn negative_values_hang_below_the_baseline() {
        let svg = chart(vec![Series {
            name: "Price".into(),
            values: vec![-10.0, 20.0, 30.0],
        }]);
        assert_eq!(svg.matches("class=\"bar s1 neg\"").count(), 1);
        assert!(svg.contains("class=\"baseline\""));
    }

    #[test]
    fn labels_are_escaped() {
        let svg = chart(vec![Series {
            name: "<b>".into(),
            values: vec![1.0, 1.0, 1.0],
        }]);
        assert!(!svg.contains("<b>"));
    }
}
