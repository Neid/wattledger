//! HTML pages (maud). Plain server-rendered markup; htmx boosts links and
//! forms and fills in the export preview and inverter settings.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use super::PlanForm;
use super::charts::{self, ColumnChart, Series};
use crate::control::bridge::CommandResult;
use crate::control::plan::Status;
use crate::db::{PlanRow, Settings};
use crate::energy::{Bucket, Period, Totals};
use crate::prices::PriceSlot;
use crate::projection::Projection;
use crate::time::{self, SLOT, TZ};

const FLOW: &str = include_str!("../../../../domoticz/Energy.html");

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Live,
    Charts,
    Prices,
    Export,
}

fn layout(title: &str, tab: Option<Tab>, body: Markup) -> Markup {
    let link = |t: Tab, href: &str, label: &str| {
        html! { a href=(href) aria-current=[(tab == Some(t)).then_some("page")] { (label) } }
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "wattledger · " (title) }
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" defer {}
                script src="/assets/app.js" defer {}
            }
            body hx-boost="true" {
                header.top {
                    span.brand { "wattledger" }
                    nav.tabs {
                        (link(Tab::Live, "/", "Energy flow"))
                        (link(Tab::Charts, "/charts", "History"))
                        (link(Tab::Prices, "/prices", "RCE prices"))
                        (link(Tab::Export, "/export", "Export"))
                    }
                }
                main { (body) }
                div #tip role="tooltip" hidden {}
            }
        }
    }
}

pub fn error_page(msg: &str) -> Markup {
    layout("error", None, html! { p.error { (msg) } })
}

// ---- formatting ----------------------------------------------------------------------

fn local(t: DateTime<Utc>, fmt: &str) -> String {
    t.with_timezone(&TZ).format(fmt).to_string()
}

fn kwh(v: f64) -> String {
    format!("{v:.1} kWh")
}

fn pln(v: f64) -> String {
    format!("{v:.2} PLN")
}

fn pct(v: Option<f64>) -> String {
    v.map_or("–".into(), |v| format!("{:.0}%", v * 100.0))
}

fn tile(label: &str, value: &str, note: Option<&str>) -> Markup {
    html! {
        div.tile {
            span.k { (label) }
            span.n { (value) }
            @if let Some(note) = note { span.note { (note) } }
        }
    }
}

// ---- live ------------------------------------------------------------------------------

pub fn live_page() -> Markup {
    layout(
        "energy flow",
        Some(Tab::Live),
        html! {
            script { (PreEscaped("window.WATTLEDGER_LIVE_URL = '/api/live';")) }
            (PreEscaped(FLOW))
        },
    )
}

// ---- charts ----------------------------------------------------------------------------

fn period_title(period: Period, date: NaiveDate) -> String {
    match period {
        Period::Day => date.format("%a %-d %b %Y").to_string(),
        Period::Week => {
            let b = period.buckets(date);
            let first = time::local_date(b[0].start);
            let last = time::local_date(b[6].start);
            format!("{} – {}", first.format("%-d %b"), last.format("%-d %b %Y"))
        }
        Period::Month => date.format("%B %Y").to_string(),
        Period::Year => date.year().to_string(),
    }
}

fn period_name(p: Period) -> &'static str {
    match p {
        Period::Day => "day",
        Period::Week => "week",
        Period::Month => "month",
        Period::Year => "year",
    }
}

fn charts_href(p: Period, date: NaiveDate) -> String {
    format!("/charts?period={}&date={date}", period_name(p))
}

pub fn charts_page(
    period: Period,
    date: NaiveDate,
    buckets: &[Bucket],
    data: Result<Vec<Totals>, String>,
) -> Markup {
    let labels: Vec<String> = buckets.iter().map(|b| b.label.clone()).collect();
    let every = match period {
        Period::Day => 3,
        Period::Month => 3,
        Period::Week | Period::Year => 1,
    };
    let body = html! {
        div.filters {
            nav.seg {
                @for p in [Period::Day, Period::Week, Period::Month, Period::Year] {
                    a href=(charts_href(p, date)) aria-current=[(p == period).then_some("page")] { (period_name(p)) }
                }
            }
            a.btn href=(charts_href(period, period.shift(date, -1))) aria-label="previous" { "‹" }
            h1 { (period_title(period, date)) }
            a.btn href=(charts_href(period, period.shift(date, 1))) aria-label="next" { "›" }
            form method="get" action="/charts" {
                input type="hidden" name="period" value=(period_name(period));
                input type="date" name="date" value=(date.to_string()) onchange="this.form.requestSubmit()";
            }
        }
        @match &data {
            Err(e) => p.error { "History is unavailable: " (e) },
            Ok(per_bucket) => {
                @let sum = per_bucket.iter().fold(Totals::default(), |mut acc, b| { acc.merge(b); acc });
                @let earnings_note = (sum.unpriced_export > 0.05)
                    .then(|| format!("{} without a price", kwh(sum.unpriced_export)));
                div.tiles {
                    (tile("Production", &kwh(sum.pv), None))
                    (tile("Home use", &kwh(sum.load), None))
                    (tile("Grid import", &kwh(sum.import), None))
                    (tile("Grid export", &kwh(sum.export), None))
                    (tile("Battery charged", &kwh(sum.charge), None))
                    (tile("Battery discharged", &kwh(sum.discharge), None))
                    (tile("Inverter own use", &kwh(sum.own_use()), None))
                    (tile("Solar used at home", &pct(sum.utilization()), None))
                    (tile("Battery efficiency", &pct(sum.efficiency()), None))
                    (tile("Export earnings", &format!("~{}", pln(sum.earnings_pln)), earnings_note.as_deref()))
                }
                @let col = |f: &dyn Fn(&Totals) -> f64| per_bucket.iter().map(f).collect::<Vec<f64>>();
                div.grid2 {
                    (chart_card("Solar and home", "kWh", 1, &labels, every, vec![
                        Series { name: "Production".into(), values: col(&|t| t.pv) },
                        Series { name: "Home use".into(), values: col(&|t| t.load) },
                    ]))
                    (chart_card("Grid", "kWh", 1, &labels, every, vec![
                        Series { name: "Import".into(), values: col(&|t| t.import) },
                        Series { name: "Export".into(), values: col(&|t| t.export) },
                    ]))
                    (chart_card("Battery", "kWh", 1, &labels, every, vec![
                        Series { name: "Charged".into(), values: col(&|t| t.charge) },
                        Series { name: "Discharged".into(), values: col(&|t| t.discharge) },
                    ]))
                    (chart_card("Inverter own use", "kWh", 2, &labels, every, vec![
                        Series { name: "Own use".into(), values: col(&|t| t.own_use()) },
                    ]))
                    (chart_card("Export earnings at RCE", "PLN", 2, &labels, every, vec![
                        Series { name: "Earnings".into(), values: col(&|t| t.earnings_pln) },
                    ]))
                }
                details.table {
                    summary { "Table" }
                    table {
                        thead { tr {
                            th { "" } th { "Production" } th { "Home" } th { "Import" } th { "Export" }
                            th { "Charged" } th { "Discharged" } th { "Own use" } th { "Solar used" }
                            th { "Efficiency" } th { "Earnings" }
                        } }
                        tbody {
                            @for (b, t) in buckets.iter().zip(per_bucket) {
                                tr {
                                    th { (b.label) }
                                    td { (format!("{:.2}", t.pv)) } td { (format!("{:.2}", t.load)) }
                                    td { (format!("{:.2}", t.import)) } td { (format!("{:.2}", t.export)) }
                                    td { (format!("{:.2}", t.charge)) } td { (format!("{:.2}", t.discharge)) }
                                    td { (format!("{:.2}", t.own_use())) } td { (pct(t.utilization())) }
                                    td { (pct(t.efficiency())) } td { (format!("{:.2}", t.earnings_pln)) }
                                }
                            }
                        }
                    }
                    p.muted { "Energy in kWh, earnings in PLN. Earnings value each 15 minutes of export at that slot's RCE price (negative prices count as zero), times the export price factor from the Export tab." }
                }
            }
        }
    };
    layout("history", Some(Tab::Charts), body)
}

fn chart_card(
    title: &str,
    unit: &str,
    decimals: usize,
    labels: &[String],
    every: usize,
    series: Vec<Series>,
) -> Markup {
    html! {
        section.card {
            h2 { (title) span.unit { " · " (unit) } }
            (charts::render(&ColumnChart {
                title,
                unit,
                decimals,
                labels: labels.to_vec(),
                series,
                label_every: every,
                highlight: None,
                width: 560.0,
            }))
        }
    }
}

// ---- prices ------------------------------------------------------------------------------

pub struct PriceDay {
    pub date: NaiveDate,
    pub title: String,
    pub slots: Vec<PriceSlot>,
    pub now: Option<DateTime<Utc>>,
    pub tomorrow: bool,
}

fn per_kwh(pln_mwh: f64) -> String {
    format!("{:.3}", pln_mwh / 1000.0)
}

pub fn prices_page(today: NaiveDate, selected: Option<NaiveDate>, days: &[PriceDay]) -> Markup {
    let shown = selected.unwrap_or(today);
    let body = html! {
        div.filters {
            a.btn href=(format!("/prices?date={}", shown.pred_opt().unwrap_or(shown))) aria-label="previous day" { "‹" }
            h1 { "RCE prices" }
            a.btn href=(format!("/prices?date={}", shown.succ_opt().unwrap_or(shown))) aria-label="next day" { "›" }
            form method="get" action="/prices" {
                input type="date" name="date" value=(shown.to_string()) onchange="this.form.requestSubmit()";
            }
            @if selected.is_some() { a href="/prices" { "Today and tomorrow" } }
        }
        @for day in days {
            section.card {
                h2 {
                    @if !day.title.is_empty() { (day.title) " · " }
                    (day.date.format("%a %-d %b %Y"))
                }
                @if day.slots.is_empty() {
                    @if day.tomorrow {
                        p.muted { "Prices for this day are not published yet. PSE publishes the next day around 14:00; they are fetched automatically." }
                    } @else {
                        p.muted { "No prices stored for this day." }
                    }
                } @else {
                    (price_day(day))
                }
            }
        }
        p.muted { "RCE (rynkowa cena energii) from PSE, per 15 minutes, in PLN/kWh. Net-billing export is settled at this price; negative prices are worth nothing to the prosumer." }
    };
    layout("prices", Some(Tab::Prices), body)
}

fn price_day(day: &PriceDay) -> Markup {
    let prices: Vec<f64> = day.slots.iter().map(|s| s.price).collect();
    let min = prices.iter().copied().fold(f64::INFINITY, f64::min);
    let max = prices.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = prices.iter().sum::<f64>() / prices.len() as f64;
    // Most valuable hour to export: best four consecutive slots.
    let best_hour = (0..prices.len().saturating_sub(3))
        .max_by(|&a, &b| {
            let sa: f64 = prices[a..a + 4].iter().sum();
            let sb: f64 = prices[b..b + 4].iter().sum();
            sa.total_cmp(&sb)
        })
        .map(|i| {
            let avg: f64 = prices[i..i + 4].iter().sum::<f64>() / 4.0;
            (day.slots[i].start, avg)
        });
    let highlight = day
        .now
        .and_then(|now| day.slots.iter().position(|s| s.start <= now && now < s.end));
    let labels: Vec<String> = day.slots.iter().map(|s| local(s.start, "%H:%M")).collect();
    html! {
        div.tiles {
            (tile("Lowest", &format!("{} PLN/kWh", per_kwh(min)), None))
            (tile("Average", &format!("{} PLN/kWh", per_kwh(avg)), None))
            (tile("Highest", &format!("{} PLN/kWh", per_kwh(max)), None))
            @if let Some((start, avg)) = best_hour {
                (tile("Best hour to export", &local(start, "%H:%M"), Some(&format!("avg {} PLN/kWh", per_kwh(avg)))))
            }
            @if let Some(i) = highlight {
                (tile("Now", &format!("{} PLN/kWh", per_kwh(prices[i])), None))
            }
        }
        (charts::render(&ColumnChart {
            title: "RCE price per 15 minutes",
            unit: "PLN/kWh",
            decimals: 3,
            labels,
            series: vec![Series { name: "Price".into(), values: prices.iter().map(|p| p / 1000.0).collect() }],
            label_every: 8,
            highlight,
            width: 1100.0,
        }))
        details.table {
            summary { "Table" }
            table {
                thead { tr { th { "Slot" } th { "PLN/kWh" } th { "PLN/MWh" } } }
                tbody {
                    @for s in &day.slots {
                        tr {
                            th { (local(s.start, "%H:%M")) "–" (local(s.end, "%H:%M")) }
                            td { (per_kwh(s.price)) }
                            td { (format!("{:.2}", s.price)) }
                        }
                    }
                }
            }
        }
    }
}

// ---- export ------------------------------------------------------------------------------

pub struct ExportView<'a> {
    pub now: DateTime<Utc>,
    pub soc: Option<f64>,
    pub settings: &'a Settings,
    pub plans: &'a [PlanRow],
    /// The plan form as submitted, with the reason it was refused.
    pub plan_form: Option<(&'a PlanForm, &'a str)>,
    pub settings_error: Option<&'a str>,
}

const INPUT_TIME: &str = "%Y-%m-%dT%H:%M";

fn status_badge(row: &PlanRow) -> Markup {
    let status = row.status();
    let class = match status {
        Status::Active => "badge on",
        Status::Scheduled => "badge",
        Status::Done => "badge good",
        Status::Failed | Status::Missed => "badge bad",
        Status::Cancelled => "badge muted",
    };
    html! {
        span class=(class) { (status.as_str()) }
        @if row.dry_run == Some(true) { " " span.badge.muted { "dry run" } }
        @if row.cancel_requested && status.is_open() { " " span.badge.muted { "stopping" } }
    }
}

pub fn export_page(v: &ExportView) -> Markup {
    let s = v.settings;
    let start = time::floor_slot(v.now) + SLOT;
    let defaults = PlanForm {
        start: local(start, INPUT_TIME),
        end: local(start + Duration::hours(6), INPUT_TIME),
        target_soc: s.min_soc.max(20).to_string(),
        power_w: s.max_export_power_w.to_string(),
    };
    let (form, error) = match v.plan_form {
        Some((f, e)) => (f, Some(e)),
        None => (&defaults, None),
    };
    let body = html! {
        div.filters {
            h1 { "Export to the grid" }
            span.muted { "Battery now: " strong { (v.soc.map_or("–".into(), |s| format!("{s:.0}%"))) } }
        }
        div.grid2 {
            section.card {
                h2 { "New export" }
                p.muted { "From the start time the inverter sells from the battery at the export power until the battery reaches the stop SOC, or until the latest end. Then the inverter's previous settings are put back." }
                @if let Some(e) = error { p.error { (e) } }
                form #plan-form method="post" action="/export/plans" {
                    label { "Start" input type="datetime-local" name="start" value=(form.start) required; }
                    label { "Stop at battery" span.suffix { input type="number" name="target_soc" min=(s.min_soc) max="99" value=(form.target_soc) required; "%" } }
                    label { "Export power" span.suffix { input type="number" name="power_w" min="100" max=(s.max_export_power_w) step="100" value=(form.power_w) required; "W" } }
                    label { "Latest end" input type="datetime-local" name="end" value=(form.end) required; }
                    button type="submit" { "Schedule export" }
                }
            }
            section.card {
                h2 { "Projection" }
                div #preview hx-post="/export/preview" hx-include="#plan-form"
                    hx-trigger="load, change from:#plan-form, input changed delay:400ms from:#plan-form" {
                    p.muted { "Calculating…" }
                }
            }
        }
        section.card {
            h2 { "Plans" }
            @if v.plans.is_empty() {
                p.muted { "No exports planned yet." }
            } @else {
                table.plans {
                    thead { tr {
                        th { "#" } th { "Start" } th { "Latest end" } th { "Stop SOC" } th { "Power" }
                        th { "Projected" } th { "Status" } th { "Note" } th {}
                    } }
                    tbody {
                        @for row in v.plans {
                            tr {
                                td { (row.id) }
                                td { (local(row.start_at, "%a %-d %b %H:%M")) }
                                td { (local(row.end_at, "%a %-d %b %H:%M")) }
                                td { (row.target_soc) "%" }
                                td { (row.power_w) " W" }
                                td {
                                    @if let Some(k) = row.projected_kwh { (kwh(k)) }
                                    @if let Some(p) = row.projected_pln { br; "~" (pln(p)) }
                                }
                                td {
                                    (status_badge(row))
                                    @if let Some(t) = row.started_at {
                                        br; span.small.muted { "started " (local(t, "%H:%M"))
                                            @if let Some(soc) = row.soc_at_start { " at " (format!("{soc:.0}%")) } }
                                    }
                                    @if let Some(t) = row.finished_at {
                                        br; span.small.muted { "ended " (local(t, "%a %H:%M")) }
                                    }
                                }
                                td.note { (row.note.as_deref().unwrap_or("")) }
                                td {
                                    @if row.status().is_open() && !row.cancel_requested {
                                        form method="post" action=(format!("/export/plans/{}/cancel", row.id))
                                            hx-confirm=(if row.status() == Status::Active { "Stop this export and restore the inverter settings?" } else { "Cancel this plan?" }) {
                                            button.secondary type="submit" {
                                                (if row.status() == Status::Active { "Stop" } else { "Cancel" })
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        div.grid2 {
            section.card {
                h2 { "Inverter settings" }
                p.muted { "The values an export changes, read from the inverter through the bridge. Use this to check the register map against the inverter display." }
                div #inverter {
                    button.secondary hx-post="/export/inverter" hx-target="#inverter" hx-disabled-elt="this" { "Read inverter settings" }
                }
            }
            section.card {
                h2 { "Settings" }
                @if let Some(e) = v.settings_error { p.error { (e) } }
                form method="post" action="/settings" {
                    label { "Max export power" span.suffix { input type="number" name="max_export_power_w" min="100" max="10000" step="100" value=(s.max_export_power_w) required; "W" } }
                    p.muted.small { "Written to the inverter's max sell power (System settings) and time-of-use slot power during an export." }
                    label { "Battery capacity" span.suffix { input type="number" name="battery_capacity_kwh" min="0.1" max="100" step="0.1" value=(s.battery_capacity_kwh) required; "kWh" } }
                    label { "Lowest stop SOC" span.suffix { input type="number" name="min_soc" min="5" max="99" value=(s.min_soc) required; "%" } }
                    label { "Home load while exporting" span.suffix { input type="number" name="home_load_w" min="0" max="20000" step="50" value=(s.home_load_w) required; "W" } }
                    label { "Battery discharge efficiency" input type="number" name="discharge_efficiency" min="0.01" max="1" step="0.01" value=(s.discharge_efficiency) required; }
                    label { "Export price factor" input type="number" name="export_price_factor" min="0.01" max="5" step="0.01" value=(s.export_price_factor) required; }
                    p.muted.small { "Multiplies the RCE value of exported energy in projections and history (1 = plain RCE)." }
                    button type="submit" { "Save settings" }
                }
            }
        }
    };
    layout("export", Some(Tab::Export), body)
}

pub fn preview(
    soc: Result<f64, &str>,
    projection: Option<&Projection>,
    best: Option<&(DateTime<Utc>, Projection)>,
) -> Markup {
    html! {
        @match soc {
            Err(e) => p.error { (e) },
            Ok(soc) => {
                @match projection {
                    None => p.muted { "Nothing to export: the battery (" (format!("{soc:.0}%")) ") is at or below the stop SOC." },
                    Some(p) => {
                        div.tiles {
                            (tile("Exported", &format!("~{}", kwh(p.export_kwh)), None))
                            (tile("Stop SOC reached at", &local(p.end, "%a %H:%M"), Some("unless the latest end comes first")))
                            (tile("Income", &format!("~{}", pln(p.income_pln)),
                                p.missing_prices.then_some("some slots have no price yet")))
                        }
                        p.muted.small { "Assumes the battery is at " (format!("{soc:.0}%")) " (now) when the export starts." }
                    }
                }
                @if let Some((start, b)) = best {
                    p { "Best start with known prices: " strong { (local(*start, "%a %H:%M")) } ", ~" (pln(b.income_pln)) }
                }
            }
        }
    }
}

pub fn inverter_settings(result: Result<CommandResult, String>) -> Markup {
    html! {
        @match result {
            Err(e) => p.error { "Could not read the inverter: " (e) },
            Ok(r) => {
                @if r.dry_run {
                    p.badge.muted { "Bridge in dry run mode: exports are simulated and nothing is written." }
                }
                table {
                    tbody {
                        @for (k, v) in &r.before {
                            tr { th { code { (k) } } td { (v) } }
                        }
                    }
                }
                button.secondary hx-post="/export/inverter" hx-target="#inverter" hx-disabled-elt="this" { "Read again" }
            }
        }
    }
}
