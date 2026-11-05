//! HTTP: pages, fragments and the live API.

pub mod charts;
mod pages;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use maud::Markup;
use serde::Deserialize;
use sqlx::PgPool;
use tracing::warn;

use crate::control::bridge::{BoxFuture, Command, CommandResult, Inverter};
use crate::control::plan::{self, Limits};
use crate::db::{self, NewPlan, Settings};
use crate::energy::{Counters, Period, Totals, aggregate};
use crate::live::LiveState;
use crate::projection::{self, ExportParams, Projection};
use crate::time::{self, SLOT};

/// History of the energy counters (VictoriaMetrics in production).
pub trait EnergySource: Send + Sync {
    fn counters(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BoxFuture<'_, anyhow::Result<Counters>>;
}

impl EnergySource for crate::vm::Vm {
    fn counters(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BoxFuture<'_, anyhow::Result<Counters>> {
        Box::pin(crate::vm::Vm::counters(self, start, end))
    }
}

/// Today's totals for the live page, refreshed at most this often.
const TODAY_MAX_AGE: Duration = Duration::seconds(60);
/// How long a read command from the export page stays valid.
const READ_TTL: Duration = Duration::seconds(30);

/// Totals since local midnight: when computed, for which date.
type TodayCache = Arc<Mutex<Option<(DateTime<Utc>, NaiveDate, Totals)>>>;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub live: Arc<RwLock<LiveState>>,
    pub history: Arc<dyn EnergySource>,
    pub inverter: Arc<dyn Inverter>,
    pub now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Cached totals since local midnight: (computed at, local date, totals).
    pub today: TodayCache,
}

impl AppState {
    fn now(&self) -> DateTime<Utc> {
        (self.now)()
    }

    fn live(&self) -> LiveState {
        self.live.read().expect("not poisoned").clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(live_page))
        .route("/api/live", get(live_api))
        .route("/charts", get(charts_page))
        .route("/prices", get(prices_page))
        .route("/export", get(export_page))
        .route("/export/preview", post(export_preview))
        .route("/export/plans", post(create_plan))
        .route("/export/plans/{id}/cancel", post(cancel_plan))
        .route("/export/inverter", post(read_inverter))
        .route("/settings", post(save_settings))
        .route("/assets/{file}", get(asset))
        .with_state(state)
}

/// Errors become a 500 page; the details go to the log.
struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        warn!(error = %format!("{:#}", self.0), "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::error_page(&format!("{:#}", self.0)),
        )
            .into_response()
    }
}

type Page = Result<Markup, AppError>;

// ---- live ------------------------------------------------------------------------

async fn live_page() -> Markup {
    pages::live_page()
}

async fn live_api(State(st): State<AppState>) -> Json<serde_json::Value> {
    let now = st.now();
    let today = today_totals(&st, now).await;
    Json(st.live().domoticz_json(now, today.as_ref()))
}

async fn today_totals(st: &AppState, now: DateTime<Utc>) -> Option<Totals> {
    let date = time::local_date(now);
    if let Some((at, d, totals)) = st.today.lock().expect("not poisoned").as_ref()
        && *d == date
        && now - *at < TODAY_MAX_AGE
    {
        return Some(totals.clone());
    }
    let (start, _) = time::day_bounds(date);
    // Up to the end of the running slot.
    let floor = time::floor_slot(now);
    let end = if floor == now { now } else { floor + SLOT };
    let counters = match st.history.counters(start, end).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "cannot read today's totals");
            return None;
        }
    };
    let bucket = crate::energy::Bucket {
        label: String::new(),
        start,
        end,
    };
    // Earnings are not shown on the live page.
    let totals = aggregate(&counters, &HashMap::new(), &[bucket], 1.0).remove(0);
    *st.today.lock().expect("not poisoned") = Some((now, date, totals.clone()));
    Some(totals)
}

// ---- charts ------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChartsQuery {
    period: Option<Period>,
    date: Option<NaiveDate>,
}

async fn charts_page(State(st): State<AppState>, Query(q): Query<ChartsQuery>) -> Page {
    let period = q.period.unwrap_or(Period::Day);
    let date = q.date.unwrap_or_else(|| time::local_date(st.now()));
    let buckets = period.buckets(date);
    let (start, end) = period.range(date);
    let settings = db::settings(&st.pool).await?;
    let data = match st.history.counters(start, end).await {
        Ok(counters) => {
            let prices = db::price_map(&st.pool, start, end).await?;
            Ok(aggregate(
                &counters,
                &prices,
                &buckets,
                settings.export_price_factor,
            ))
        }
        Err(e) => Err(format!("{e:#}")),
    };
    Ok(pages::charts_page(period, date, &buckets, data))
}

// ---- prices ------------------------------------------------------------------------

#[derive(Deserialize)]
struct PricesQuery {
    date: Option<NaiveDate>,
}

async fn prices_page(State(st): State<AppState>, Query(q): Query<PricesQuery>) -> Page {
    let now = st.now();
    let today = time::local_date(now);
    let mut days = Vec::new();
    let dates: Vec<(NaiveDate, &str)> = match q.date {
        Some(d) => vec![(d, "")],
        None => vec![
            (today, "Today"),
            (today.succ_opt().unwrap_or(today), "Tomorrow"),
        ],
    };
    for (date, title) in dates {
        let (start, end) = time::day_bounds(date);
        let slots = db::prices_between(&st.pool, start, end).await?;
        days.push(pages::PriceDay {
            date,
            title: title.to_owned(),
            slots,
            now: (start <= now && now < end).then_some(now),
            tomorrow: date > today,
        });
    }
    Ok(pages::prices_page(today, q.date, &days))
}

// ---- export ------------------------------------------------------------------------

async fn export_page(State(st): State<AppState>) -> Page {
    render_export(&st, None, None).await
}

async fn render_export(
    st: &AppState,
    plan_form: Option<(&PlanForm, &str)>,
    settings_error: Option<&str>,
) -> Page {
    let now = st.now();
    let settings = db::settings(&st.pool).await?;
    let plans = db::recent_plans(&st.pool, 20).await?;
    let soc = st.live().soc(now);
    Ok(pages::export_page(&pages::ExportView {
        now,
        soc,
        settings: &settings,
        plans: &plans,
        plan_form,
        settings_error,
    }))
}

/// The plan form as typed; parsed by hand so mistakes get a message.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanForm {
    pub start: String,
    pub end: String,
    pub target_soc: String,
    pub power_w: String,
}

struct ParsedPlan {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    target_soc: i32,
    power_w: i32,
}

fn parse_local(field: &str, value: &str) -> Result<DateTime<Utc>, String> {
    let t = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| format!("{field}: enter a date and time"))?;
    time::from_local(t).ok_or_else(|| format!("{field}: that time does not exist (clock change)"))
}

impl PlanForm {
    fn parse(&self) -> Result<ParsedPlan, String> {
        Ok(ParsedPlan {
            start: parse_local("start", &self.start)?,
            end: parse_local("latest end", &self.end)?,
            target_soc: self
                .target_soc
                .trim()
                .parse()
                .map_err(|_| "stop SOC: enter a whole number".to_owned())?,
            power_w: self
                .power_w
                .trim()
                .parse()
                .map_err(|_| "export power: enter a whole number of watts".to_owned())?,
        })
    }
}

fn export_params(p: &ParsedPlan, soc: f64, s: &Settings) -> ExportParams {
    ExportParams {
        start: p.start,
        soc_start: soc,
        target_soc: f64::from(p.target_soc),
        capacity_kwh: s.battery_capacity_kwh,
        power_w: f64::from(p.power_w),
        home_load_w: f64::from(s.home_load_w),
        efficiency: s.discharge_efficiency,
        price_factor: s.export_price_factor,
    }
}

/// Prices from `from` to the last published slot.
async fn upcoming_prices(
    st: &AppState,
    from: DateTime<Utc>,
) -> anyhow::Result<(HashMap<DateTime<Utc>, f64>, Option<DateTime<Utc>>)> {
    let until = from + Duration::days(3);
    let slots = db::prices_between(&st.pool, time::floor_slot(from), until).await?;
    let last = slots.last().map(|s| s.end);
    Ok((
        slots.into_iter().map(|s| (s.start, s.price)).collect(),
        last,
    ))
}

async fn export_preview(State(st): State<AppState>, Form(form): Form<PlanForm>) -> Page {
    let now = st.now();
    let parsed = match form.parse() {
        Ok(p) => p,
        Err(e) => return Ok(pages::preview(Err(&e), None, None)),
    };
    let settings = db::settings(&st.pool).await?;
    let Some(soc) = st.live().soc(now) else {
        return Ok(pages::preview(
            Err("no live battery SOC; is the bridge running?"),
            None,
            None,
        ));
    };
    let params = export_params(&parsed, soc, &settings);
    let (prices, last) = upcoming_prices(&st, now.min(parsed.start)).await?;
    let projection = projection::project(&params, &prices);
    let best = last.and_then(|until| projection::best_start(&params, &prices, now, until));
    Ok(pages::preview(Ok(soc), projection.as_ref(), best.as_ref()))
}

async fn create_plan(
    State(st): State<AppState>,
    Form(form): Form<PlanForm>,
) -> Result<Response, AppError> {
    let now = st.now();
    let parsed = match form.parse() {
        Ok(p) => p,
        Err(e) => {
            return Ok(render_export(&st, Some((&form, &e)), None)
                .await?
                .into_response());
        }
    };
    let settings = db::settings(&st.pool).await?;
    let existing: Vec<_> = db::open_plans(&st.pool)
        .await?
        .iter()
        .map(db::PlanRow::plan)
        .collect();
    let limits = Limits {
        min_soc: settings.min_soc,
        max_power_w: settings.max_export_power_w,
    };
    if let Err(e) = plan::validate(
        now,
        parsed.start,
        parsed.end,
        parsed.target_soc,
        parsed.power_w,
        limits,
        &existing,
    ) {
        return Ok(render_export(&st, Some((&form, &e)), None)
            .await?
            .into_response());
    }
    let projection: Option<Projection> = match st.live().soc(now) {
        Some(soc) => {
            let (prices, _) = upcoming_prices(&st, parsed.start).await?;
            projection::project(&export_params(&parsed, soc, &settings), &prices)
        }
        None => None,
    };
    db::insert_plan(
        &st.pool,
        &NewPlan {
            start: parsed.start,
            end: parsed.end,
            target_soc: parsed.target_soc,
            power_w: parsed.power_w,
            projected_kwh: projection.as_ref().map(|p| p.export_kwh),
            projected_pln: projection.as_ref().map(|p| p.income_pln),
        },
    )
    .await?;
    Ok(Redirect::to("/export").into_response())
}

async fn cancel_plan(
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Redirect, AppError> {
    db::request_cancel(&st.pool, id).await?;
    Ok(Redirect::to("/export"))
}

async fn read_inverter(State(st): State<AppState>) -> Page {
    let now = st.now();
    let cmd = Command {
        id: format!("read-{}", now.timestamp_millis()),
        expires: (now + READ_TTL).timestamp().max(0) as u64,
        write: Vec::new(),
        read: plan::export_keys(),
    };
    db::log_command(
        &st.pool,
        &cmd.id,
        None,
        "read",
        &serde_json::to_value(&cmd)?,
    )
    .await?;
    let id = cmd.id.clone();
    let result: Result<CommandResult, String> = match st.inverter.send(cmd).await {
        Ok(r) => {
            db::log_result(&st.pool, &id, r.ok, &serde_json::to_value(&r)?).await?;
            if r.ok {
                Ok(r)
            } else {
                Err(r.error.unwrap_or_default())
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            db::log_result(&st.pool, &id, false, &serde_json::json!({ "error": msg })).await?;
            Err(msg)
        }
    };
    Ok(pages::inverter_settings(result))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SettingsForm {
    pub battery_capacity_kwh: String,
    pub max_export_power_w: String,
    pub min_soc: String,
    pub home_load_w: String,
    pub discharge_efficiency: String,
    pub export_price_factor: String,
}

impl SettingsForm {
    fn parse(&self) -> Result<Settings, String> {
        fn num<T: std::str::FromStr + PartialOrd + std::fmt::Display>(
            name: &str,
            v: &str,
            min: T,
            max: T,
        ) -> Result<T, String> {
            let x: T = v
                .trim()
                .parse()
                .map_err(|_| format!("{name}: enter a number"))?;
            if x < min || x > max {
                return Err(format!("{name} must be between {min} and {max}"));
            }
            Ok(x)
        }
        let s = Settings {
            battery_capacity_kwh: num("battery capacity", &self.battery_capacity_kwh, 0.1, 100.0)?,
            max_export_power_w: num("max export power", &self.max_export_power_w, 100, 10_000)?,
            min_soc: num("lowest stop SOC", &self.min_soc, 5, 99)?,
            home_load_w: num("home load", &self.home_load_w, 0, 20_000)?,
            discharge_efficiency: num(
                "discharge efficiency",
                &self.discharge_efficiency,
                0.01,
                1.0,
            )?,
            export_price_factor: num("export price factor", &self.export_price_factor, 0.01, 5.0)?,
        };
        if s.max_export_power_w % plan::POWER_STEP_W != 0 {
            return Err(format!(
                "max export power must be a multiple of {} W",
                plan::POWER_STEP_W
            ));
        }
        Ok(s)
    }
}

async fn save_settings(
    State(st): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, AppError> {
    match form.parse() {
        Ok(s) => {
            db::save_settings(&st.pool, &s).await?;
            Ok(Redirect::to("/export").into_response())
        }
        Err(e) => Ok(render_export(&st, None, Some(&e)).await?.into_response()),
    }
}

// ---- assets ------------------------------------------------------------------------

async fn asset(Path(file): Path<String>) -> Response {
    let (body, mime): (&'static [u8], &str) = match file.as_str() {
        "htmx.min.js" => (
            include_bytes!("../../assets/htmx.min.js"),
            "text/javascript",
        ),
        "app.js" => (include_bytes!("../../assets/app.js"), "text/javascript"),
        "app.css" => (include_bytes!("../../assets/app.css"), "text/css"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}
