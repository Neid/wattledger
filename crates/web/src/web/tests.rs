//! Route tests: real database, fake VictoriaMetrics and inverter.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

use super::*;
use crate::control::bridge::{BoxFuture, Command, CommandResult, Inverter};
use crate::energy::{Counters, Metric};
use crate::live::LiveState;
use crate::prices::parse_rce;

const NOW: &str = "2026-11-05T12:00:00Z";

fn utc(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// Counters that grow by a fixed amount per slot.
struct FakeHistory {
    fail: bool,
}

impl EnergySource for FakeHistory {
    fn counters(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BoxFuture<'_, anyhow::Result<Counters>> {
        let fail = self.fail;
        Box::pin(async move {
            anyhow::ensure!(!fail, "VictoriaMetrics is down");
            let n = ((end - start).num_minutes() / 15) as usize;
            let per_slot = |m: Metric| match m {
                Metric::Pv => 0.5,
                Metric::Load => 0.25,
                Metric::Export => 0.2,
                Metric::Import => 0.05,
                Metric::Charge => 0.1,
                Metric::Discharge => 0.09,
            };
            let series = Metric::ALL
                .into_iter()
                .map(|m| {
                    (
                        m,
                        (0..=n)
                            .map(|i| Some(1000.0 + per_slot(m) * i as f64))
                            .collect(),
                    )
                })
                .collect();
            Ok(Counters { start, series })
        })
    }
}

#[derive(Default)]
struct FakeInverter {
    sent: Mutex<Vec<Command>>,
}

impl Inverter for FakeInverter {
    fn send(&self, cmd: Command) -> BoxFuture<'_, anyhow::Result<CommandResult>> {
        Box::pin(async move {
            self.sent.lock().unwrap().push(cmd.clone());
            let values: BTreeMap<String, f64> =
                cmd.read.iter().map(|k| (k.clone(), 4000.0)).collect();
            Ok(CommandResult {
                id: cmd.id,
                ok: true,
                dry_run: true,
                before: values.clone(),
                after: values,
                ..CommandResult::default()
            })
        })
    }
}

fn state(pool: PgPool, history_fails: bool) -> (AppState, Arc<FakeInverter>) {
    let inverter = Arc::new(FakeInverter::default());
    let mut live = LiveState::default();
    let now = utc(NOW);
    live.apply("wattledger", "wattledger/deye/battery_soc", b"80", now);
    live.apply("wattledger", "wattledger/deye/pv_power", b"2500", now);
    let state = AppState {
        pool,
        live: Arc::new(RwLock::new(live)),
        history: Arc::new(FakeHistory {
            fail: history_fails,
        }),
        inverter: inverter.clone(),
        now: Arc::new(move || now),
        today: Arc::default(),
    };
    (state, inverter)
}

fn app(pool: PgPool) -> Router {
    router(state(pool, false).0)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn post(
    app: &Router,
    uri: &str,
    form: &str,
) -> (StatusCode, HashMap<String, String>, String) {
    let res = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let headers = res
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn load_prices(pool: &PgPool) {
    let slots = parse_rce(include_bytes!("../../tests/fixtures/rce-2026-11-05.json")).unwrap();
    crate::db::upsert_prices(pool, &slots).await.unwrap();
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn live_page_embeds_the_flow_diagram(pool: PgPool) {
    let (status, body) = get(&app(pool), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("WATTLEDGER_LIVE_URL"), "{body}");
    assert!(body.contains("id=\"wl-flow\""));
    assert!(
        body.contains("aria-current=\"page\""),
        "the active tab is marked"
    );
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn live_api_has_values_and_today(pool: PgPool) {
    let (status, body) = get(&app(pool), "/api/live").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let devices = v["result"].as_array().unwrap();
    let pv = devices.iter().find(|d| d["ID"] == "deye_pv_power").unwrap();
    assert_eq!(pv["Data"], "2500");
    // 52 slots since local midnight (23:00 UTC) at 0.5 kWh each.
    let pv_today = devices
        .iter()
        .find(|d| d["ID"] == "deye_pv_energy")
        .unwrap();
    assert_eq!(pv_today["CounterToday"], "26.00 kWh");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn charts_show_totals_and_earnings(pool: PgPool) {
    load_prices(&pool).await;
    let (status, body) = get(&app(pool), "/charts?period=day&date=2026-11-05").await;
    assert_eq!(status, StatusCode::OK);
    // A full day: 96 slots.
    assert!(body.contains("48.0 kWh"), "production"); // 0.5 * 96
    assert!(body.contains("19.2 kWh"), "export"); // 0.2 * 96
    assert!(body.contains("60%"), "utilization (48 - 19.2) / 48");
    assert!(body.contains("90%"), "battery efficiency");
    assert!(body.contains("PLN"));
    assert_eq!(body.matches("<figure class=\"chart\"").count(), 5);
    assert!(
        body.contains("period=day&amp;date=2026-11-04"),
        "link to the previous day"
    );
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn charts_survive_a_history_outage(pool: PgPool) {
    let app = router(state(pool, true).0);
    let (status, body) = get(&app, "/charts?period=week").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("VictoriaMetrics is down"), "{body}");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn charts_reject_bad_periods(pool: PgPool) {
    let (status, _) = get(&app(pool), "/charts?period=decade").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn prices_page(pool: PgPool) {
    load_prices(&pool).await;
    let app = app(pool);
    let (status, body) = get(&app, "/prices").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Today"), "{body}");
    assert!(body.contains("0.990"), "989.70 PLN/MWh as PLN/kWh");
    assert!(body.contains("class=\"now\""), "current slot marked");
    assert!(body.contains("Tomorrow"));
    assert!(body.contains("not published yet"));

    let (_, body) = get(&app, "/prices?date=2026-11-01").await;
    assert!(body.contains("No prices"), "{body}");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn export_page_lists_plans_and_settings(pool: PgPool) {
    let (status, body) = get(&app(pool), "/export").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("name=\"target_soc\""));
    assert!(body.contains("value=\"4000\""), "max export power setting");
    assert!(body.contains("80%"), "live SOC");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn creating_and_cancelling_a_plan(pool: PgPool) {
    load_prices(&pool).await;
    let app = app(pool.clone());
    let (status, headers, _) = post(
        &app,
        "/export/plans",
        "start=2026-11-05T17%3A00&end=2026-11-05T22%3A00&target_soc=30&power_w=4000",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/export");
    let plans = crate::db::open_plans(&pool).await.unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].start_at, utc("2026-11-05T16:00:00Z"));
    assert!(plans[0].projected_pln.unwrap() > 0.0);

    let (_, body) = get(&app, "/export").await;
    assert!(body.contains("scheduled"));

    let (status, _, _) = post(&app, &format!("/export/plans/{}/cancel", plans[0].id), "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        crate::db::plan(&pool, plans[0].id)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn invalid_plans_are_explained(pool: PgPool) {
    let app = app(pool.clone());
    let (status, _, body) = post(
        &app,
        "/export/plans",
        "start=2026-11-05T17%3A00&end=2026-11-05T22%3A00&target_soc=5&power_w=4000",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "htmx only swaps 2xx responses");
    assert!(
        body.contains("stop SOC must be between 10% and 99%"),
        "{body}"
    );
    assert!(crate::db::open_plans(&pool).await.unwrap().is_empty());

    let (_, _, body) = post(
        &app,
        "/export/plans",
        "start=garbage&end=x&target_soc=30&power_w=4000",
    )
    .await;
    assert!(body.contains("start"), "{body}");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn preview_projects_income(pool: PgPool) {
    load_prices(&pool).await;
    let (status, _, body) = post(
        &app(pool),
        "/export/preview",
        "start=2026-11-05T17%3A00&end=2026-11-05T22%3A00&target_soc=30&power_w=4000",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // From 80% (live) to 30% of 16.1 kWh at 95% = 7.65 kWh, shared with a
    // 400 W home load: 4 / 4.4 of it is exported.
    assert!(body.contains("7.0 kWh"), "{body}");
    assert!(body.contains("PLN"));
    assert!(body.contains("Best start"), "{body}");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn settings_are_saved_and_checked(pool: PgPool) {
    let app = app(pool.clone());
    let form = "battery_capacity_kwh=16.1&max_export_power_w=3500&min_soc=15&home_load_w=300&discharge_efficiency=0.95&export_price_factor=1";
    let (status, _, _) = post(&app, "/settings", form).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let s = crate::db::settings(&pool).await.unwrap();
    assert_eq!(s.max_export_power_w, 3500);
    assert_eq!(s.min_soc, 15);

    let bad = form.replace("3500", "99999");
    let (status, _, body) = post(&app, "/settings", &bad).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("10000"), "{body}");
    assert_eq!(
        crate::db::settings(&pool).await.unwrap().max_export_power_w,
        3500
    );
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn reads_inverter_settings(pool: PgPool) {
    let (state, inverter) = state(pool, false);
    let app = router(state);
    let (status, _, body) = post(&app, "/export/inverter", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("max_sell_power"), "{body}");
    assert!(body.contains("dry run"), "{body}");
    let sent = inverter.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].write.is_empty(), "reading never writes");
    assert!(sent[0].expires > utc(NOW).timestamp() as u64);
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn assets(pool: PgPool) {
    let app = app(pool);
    let res = app
        .clone()
        .oneshot(
            Request::get("/assets/htmx.min.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    let (status, _) = get(&app, "/assets/nope.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
