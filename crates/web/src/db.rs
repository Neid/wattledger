//! PostgreSQL storage: prices, settings, export plans and the command log.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::control::plan::{Plan, Status};
use crate::prices::PriceSlot;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    use anyhow::Context;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    MIGRATOR.run(&pool).await.context("running migrations")?;
    Ok(pool)
}

// ---- prices ------------------------------------------------------------------

/// Inserts or updates slots. Returns the number of rows written.
pub async fn upsert_prices(pool: &PgPool, slots: &[PriceSlot]) -> sqlx::Result<u64> {
    let starts: Vec<DateTime<Utc>> = slots.iter().map(|s| s.start).collect();
    let ends: Vec<DateTime<Utc>> = slots.iter().map(|s| s.end).collect();
    let dates: Vec<NaiveDate> = slots.iter().map(|s| s.business_date).collect();
    let prices: Vec<f64> = slots.iter().map(|s| s.price).collect();
    let done = sqlx::query(
        "insert into rce_price (slot_start, slot_end, business_date, price_pln_mwh)
         select * from unnest($1::timestamptz[], $2::timestamptz[], $3::date[], $4::float8[])
         on conflict (slot_start) do update
         set slot_end = excluded.slot_end,
             business_date = excluded.business_date,
             price_pln_mwh = excluded.price_pln_mwh,
             fetched_at = now()",
    )
    .bind(starts)
    .bind(ends)
    .bind(dates)
    .bind(prices)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Slots starting in `[start, end)`, in order.
pub async fn prices_between(
    pool: &PgPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> sqlx::Result<Vec<PriceSlot>> {
    let rows: Vec<(DateTime<Utc>, DateTime<Utc>, NaiveDate, f64)> = sqlx::query_as(
        "select slot_start, slot_end, business_date, price_pln_mwh from rce_price
         where slot_start >= $1 and slot_start < $2 order by slot_start",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(start, end, business_date, price)| PriceSlot {
            start,
            end,
            business_date,
            price,
        })
        .collect())
}

/// Prices keyed by slot start, for energy and projection math.
pub async fn price_map(
    pool: &PgPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> sqlx::Result<HashMap<DateTime<Utc>, f64>> {
    Ok(prices_between(pool, start, end)
        .await?
        .into_iter()
        .map(|s| (s.start, s.price))
        .collect())
}

/// Number of stored slots per business date in `[from, to]`.
pub async fn slot_counts(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<HashMap<NaiveDate, usize>> {
    let rows: Vec<(NaiveDate, i64)> = sqlx::query_as(
        "select business_date, count(*) from rce_price
         where business_date between $1 and $2 group by business_date",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(d, n)| (d, n as usize)).collect())
}

// ---- settings ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Settings {
    pub battery_capacity_kwh: f64,
    pub max_export_power_w: i32,
    pub min_soc: i32,
    pub home_load_w: i32,
    pub discharge_efficiency: f64,
    pub export_price_factor: f64,
}

pub async fn settings(pool: &PgPool) -> sqlx::Result<Settings> {
    sqlx::query_as(
        "select battery_capacity_kwh, max_export_power_w, min_soc, home_load_w,
                discharge_efficiency, export_price_factor
         from settings",
    )
    .fetch_one(pool)
    .await
}

/// Saves settings; the database checks the ranges.
pub async fn save_settings(pool: &PgPool, s: &Settings) -> sqlx::Result<()> {
    sqlx::query(
        "update settings set battery_capacity_kwh = $1, max_export_power_w = $2, min_soc = $3,
                home_load_w = $4, discharge_efficiency = $5, export_price_factor = $6,
                updated_at = now()",
    )
    .bind(s.battery_capacity_kwh)
    .bind(s.max_export_power_w)
    .bind(s.min_soc)
    .bind(s.home_load_w)
    .bind(s.discharge_efficiency)
    .bind(s.export_price_factor)
    .execute(pool)
    .await?;
    Ok(())
}

// ---- plans -----------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanRow {
    pub id: i64,
    #[allow(dead_code)] // kept for the record; not shown
    pub created_at: DateTime<Utc>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub target_soc: i32,
    pub power_w: i32,
    pub status: String,
    pub cancel_requested: bool,
    pub retry_after: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub dry_run: Option<bool>,
    pub saved_settings: Option<Json<BTreeMap<String, f64>>>,
    pub soc_at_start: Option<f64>,
    pub projected_kwh: Option<f64>,
    pub projected_pln: Option<f64>,
}

impl PlanRow {
    pub fn status(&self) -> Status {
        // The column has a check constraint with exactly these values.
        Status::parse(&self.status).unwrap_or(Status::Failed)
    }

    pub fn plan(&self) -> Plan {
        Plan {
            id: self.id,
            start: self.start_at,
            end: self.end_at,
            target_soc: self.target_soc,
            power_w: self.power_w,
            status: self.status(),
            cancel_requested: self.cancel_requested,
            retry_after: self.retry_after,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewPlan {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub target_soc: i32,
    pub power_w: i32,
    pub projected_kwh: Option<f64>,
    pub projected_pln: Option<f64>,
}

pub async fn insert_plan(pool: &PgPool, p: &NewPlan) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "insert into export_plan (start_at, end_at, target_soc, power_w, projected_kwh, projected_pln)
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(p.start)
    .bind(p.end)
    .bind(p.target_soc)
    .bind(p.power_w)
    .bind(p.projected_kwh)
    .bind(p.projected_pln)
    .fetch_one(pool)
    .await
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn plan(pool: &PgPool, id: i64) -> sqlx::Result<Option<PlanRow>> {
    sqlx::query_as("select id, created_at, start_at, end_at, target_soc, power_w, status, cancel_requested, retry_after, started_at, finished_at, note, dry_run, saved_settings, soc_at_start, projected_kwh, projected_pln from export_plan where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Scheduled and active plans, by start.
pub async fn open_plans(pool: &PgPool) -> sqlx::Result<Vec<PlanRow>> {
    sqlx::query_as(
        "select id, created_at, start_at, end_at, target_soc, power_w, status, cancel_requested, retry_after, started_at, finished_at, note, dry_run, saved_settings, soc_at_start, projected_kwh, projected_pln from export_plan
         where status in ('scheduled', 'active') order by start_at",
    )
    .fetch_all(pool)
    .await
}

/// Open plans first (by start), then the most recent closed ones.
pub async fn recent_plans(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<PlanRow>> {
    sqlx::query_as(
        "select id, created_at, start_at, end_at, target_soc, power_w, status, cancel_requested, retry_after, started_at, finished_at, note, dry_run, saved_settings, soc_at_start, projected_kwh, projected_pln from export_plan
         order by status in ('scheduled', 'active') desc,
                  case when status in ('scheduled', 'active') then start_at end asc,
                  start_at desc
         limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Asks the controller to stop a plan. False if it is no longer open.
pub async fn request_cancel(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "update export_plan set cancel_requested = true
         where id = $1 and status in ('scheduled', 'active')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Stores the settings to restore, once, before the first write.
pub async fn save_baseline(
    pool: &PgPool,
    id: i64,
    saved: &BTreeMap<String, f64>,
    soc: Option<f64>,
) -> sqlx::Result<()> {
    sqlx::query(
        "update export_plan set saved_settings = $2, soc_at_start = $3
         where id = $1 and saved_settings is null",
    )
    .bind(id)
    .bind(Json(saved))
    .bind(soc)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_active(pool: &PgPool, id: i64, dry_run: bool) -> sqlx::Result<()> {
    sqlx::query(
        "update export_plan set status = 'active', dry_run = $2, started_at = now(),
                retry_after = null, note = null
         where id = $1",
    )
    .bind(id)
    .bind(dry_run)
    .execute(pool)
    .await?;
    Ok(())
}

/// Leaves the status alone and tries again later.
pub async fn mark_retry(
    pool: &PgPool,
    id: i64,
    retry_after: DateTime<Utc>,
    note: &str,
) -> sqlx::Result<()> {
    sqlx::query("update export_plan set retry_after = $2, note = $3 where id = $1")
        .bind(id)
        .bind(retry_after)
        .bind(note)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_closed(pool: &PgPool, id: i64, status: Status, note: &str) -> sqlx::Result<()> {
    sqlx::query(
        "update export_plan set status = $2, note = $3, finished_at = now(), retry_after = null
         where id = $1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(note)
    .execute(pool)
    .await?;
    Ok(())
}

// ---- command log -------------------------------------------------------------------

pub async fn log_command(
    pool: &PgPool,
    id: &str,
    plan_id: Option<i64>,
    purpose: &str,
    request: &Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "insert into inverter_command (id, plan_id, purpose, request) values ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(plan_id)
    .bind(purpose)
    .bind(request)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn log_result(pool: &PgPool, id: &str, ok: bool, result: &Value) -> sqlx::Result<()> {
    sqlx::query("update inverter_command set ok = $2, result = $3 where id = $1")
        .bind(id)
        .bind(ok)
        .bind(result)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn slot(start: &str, price: f64) -> PriceSlot {
        let start = utc(start);
        PriceSlot {
            start,
            end: start + Duration::minutes(15),
            business_date: crate::time::local_date(start),
            price,
        }
    }

    fn new_plan(start: &str, hours: i64) -> NewPlan {
        NewPlan {
            start: utc(start),
            end: utc(start) + Duration::hours(hours),
            target_soc: 30,
            power_w: 4000,
            projected_kwh: Some(7.6),
            projected_pln: Some(3.8),
        }
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    async fn prices_upsert_and_query(pool: PgPool) {
        let slots = vec![
            slot("2026-11-05T17:00:00Z", 500.0),
            slot("2026-11-05T17:15:00Z", 510.0),
            slot("2026-11-05T23:00:00Z", 300.0), // next business day
        ];
        assert_eq!(upsert_prices(&pool, &slots).await.unwrap(), 3);
        // PSE may correct a price: the second upsert wins.
        upsert_prices(&pool, &[slot("2026-11-05T17:00:00Z", 499.0)])
            .await
            .unwrap();

        let got = prices_between(
            &pool,
            utc("2026-11-05T17:00:00Z"),
            utc("2026-11-05T23:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].price, 499.0);
        assert_eq!(got[1], slots[1]);

        let map = price_map(
            &pool,
            utc("2026-11-05T00:00:00Z"),
            utc("2026-11-06T00:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(map.len(), 3);

        let d = |day| NaiveDate::from_ymd_opt(2026, 11, day).unwrap();
        let counts = slot_counts(&pool, d(5), d(6)).await.unwrap();
        assert_eq!(counts[&d(5)], 2);
        assert_eq!(counts[&d(6)], 1);
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    async fn settings_defaults_and_checks(pool: PgPool) {
        let s = settings(&pool).await.unwrap();
        assert_eq!(s.battery_capacity_kwh, 16.1);
        assert_eq!(s.max_export_power_w, 4000);
        assert_eq!(s.min_soc, 10);

        let changed = Settings {
            max_export_power_w: 3500,
            ..s.clone()
        };
        save_settings(&pool, &changed).await.unwrap();
        assert_eq!(settings(&pool).await.unwrap(), changed);

        let bad = Settings {
            max_export_power_w: 50_000,
            ..s
        };
        assert!(save_settings(&pool, &bad).await.is_err());
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    async fn plan_lifecycle(pool: PgPool) {
        let id = insert_plan(&pool, &new_plan("2026-11-05T17:00:00Z", 4))
            .await
            .unwrap();
        let row = plan(&pool, id).await.unwrap().unwrap();
        assert_eq!(row.status(), Status::Scheduled);
        assert_eq!(row.plan().target_soc, 30);
        assert_eq!(open_plans(&pool).await.unwrap().len(), 1);

        let saved = BTreeMap::from([("work_mode".to_owned(), 2.0)]);
        save_baseline(&pool, id, &saved, Some(81.0)).await.unwrap();
        // The first baseline is kept: a retried start must not overwrite it
        // with values the export itself may already have written.
        save_baseline(
            &pool,
            id,
            &BTreeMap::from([("work_mode".to_owned(), 0.0)]),
            None,
        )
        .await
        .unwrap();
        let retry = utc("2026-11-05T17:01:00Z");
        mark_retry(&pool, id, retry, "bridge did not answer")
            .await
            .unwrap();
        let row = plan(&pool, id).await.unwrap().unwrap();
        assert_eq!(row.retry_after, Some(retry));
        assert_eq!(row.note.as_deref(), Some("bridge did not answer"));
        assert_eq!(row.saved_settings.as_ref().unwrap().0, saved);
        assert_eq!(row.soc_at_start, Some(81.0));

        mark_active(&pool, id, true).await.unwrap();
        let row = plan(&pool, id).await.unwrap().unwrap();
        assert_eq!(row.status(), Status::Active);
        assert_eq!(row.dry_run, Some(true));
        assert!(row.started_at.is_some());
        assert_eq!(row.retry_after, None, "a success clears the retry");

        assert!(request_cancel(&pool, id).await.unwrap());
        assert!(plan(&pool, id).await.unwrap().unwrap().cancel_requested);

        mark_closed(&pool, id, Status::Cancelled, "cancelled")
            .await
            .unwrap();
        let row = plan(&pool, id).await.unwrap().unwrap();
        assert_eq!(row.status(), Status::Cancelled);
        assert!(row.finished_at.is_some());
        assert!(open_plans(&pool).await.unwrap().is_empty());
        assert!(
            !request_cancel(&pool, id).await.unwrap(),
            "closed plans stay closed"
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    async fn recent_plans_put_open_ones_first(pool: PgPool) {
        let old = insert_plan(&pool, &new_plan("2026-11-01T17:00:00Z", 2))
            .await
            .unwrap();
        mark_closed(&pool, old, Status::Done, "ok").await.unwrap();
        let later = insert_plan(&pool, &new_plan("2026-11-07T17:00:00Z", 2))
            .await
            .unwrap();
        let sooner = insert_plan(&pool, &new_plan("2026-11-06T17:00:00Z", 2))
            .await
            .unwrap();
        let ids: Vec<i64> = recent_plans(&pool, 10)
            .await
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, [sooner, later, old]);
        assert_eq!(recent_plans(&pool, 1).await.unwrap().len(), 1);
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    async fn command_log(pool: PgPool) {
        let id = insert_plan(&pool, &new_plan("2026-11-05T17:00:00Z", 4))
            .await
            .unwrap();
        log_command(&pool, "c1", Some(id), "start", &json!({"write": []}))
            .await
            .unwrap();
        log_result(&pool, "c1", true, &json!({"ok": true}))
            .await
            .unwrap();
        let (ok, result): (Option<bool>, Option<Value>) =
            sqlx::query_as("select ok, result from inverter_command where id = 'c1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ok, Some(true));
        assert_eq!(result.unwrap()["ok"], true);
    }
}
