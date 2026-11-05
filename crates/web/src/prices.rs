//! RCE (rynkowa cena energii) from PSE: the 15-minute market price that
//! net-billing export is settled at.

use std::collections::HashMap;

use anyhow::{Context, bail};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike, Utc};
use serde::Deserialize;

use crate::time::{self, SLOT};

#[derive(Debug, Clone, PartialEq)]
pub struct PriceSlot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub business_date: NaiveDate,
    /// PLN/MWh; may be negative.
    pub price: f64,
}

/// Parses one response of `GET /api/rce-pln?$filter=business_date eq '...'`.
pub fn parse_rce(json: &[u8]) -> anyhow::Result<Vec<PriceSlot>> {
    #[derive(Deserialize)]
    struct Response {
        value: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        /// End of the slot, UTC, "YYYY-MM-DD HH:MM:SS".
        dtime_utc: String,
        rce_pln: f64,
        business_date: NaiveDate,
    }

    let response: Response = serde_json::from_slice(json).context("parsing the PSE response")?;
    let mut slots = Vec::with_capacity(response.value.len());
    for row in response.value {
        let end = NaiveDateTime::parse_from_str(&row.dtime_utc, "%Y-%m-%d %H:%M:%S")
            .with_context(|| format!("bad dtime_utc {:?}", row.dtime_utc))?
            .and_utc();
        if end.minute() % 15 != 0 || end.second() != 0 {
            bail!("slot end {end} is not on the 15-minute grid");
        }
        if !row.rce_pln.is_finite() {
            bail!("price at {end} is not a number");
        }
        slots.push(PriceSlot {
            start: end - SLOT,
            end,
            business_date: row.business_date,
            price: row.rce_pln,
        });
    }
    slots.sort_by_key(|s| s.start);
    Ok(slots)
}

/// Query URL for one business day.
pub fn day_url(base: &str, date: NaiveDate) -> String {
    format!("{base}?$filter=business_date%20eq%20'{date}'")
}

/// Days whose prices are missing or incomplete: the last `backfill_days`,
/// today, and tomorrow once PSE may have published it (from 13:00 local;
/// it usually appears around 13:40).
pub fn dates_to_fetch(
    now: DateTime<Utc>,
    have: &HashMap<NaiveDate, usize>,
    backfill_days: u32,
) -> Vec<NaiveDate> {
    let today = time::local_date(now);
    let mut dates: Vec<NaiveDate> = (0..=backfill_days)
        .rev()
        .filter_map(|back| today.checked_sub_days(chrono::Days::new(u64::from(back))))
        .collect();
    if now.with_timezone(&time::TZ).hour() >= 13
        && let Some(tomorrow) = today.succ_opt()
    {
        dates.push(tomorrow);
    }
    dates.retain(|d| have.get(d).copied().unwrap_or(0) < time::slots_in_day(*d));
    dates
}

/// Net-billing value of exported energy: negative prices count as zero.
pub fn export_value_pln(kwh: f64, price_pln_mwh: f64, factor: f64) -> f64 {
    kwh * price_pln_mwh.max(0.0) / 1000.0 * factor
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOV5: &[u8] = include_bytes!("../tests/fixtures/rce-2026-11-05.json");
    const OCT25: &[u8] = include_bytes!("../tests/fixtures/rce-2026-10-25.json");

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn parses_an_ordinary_day() {
        let slots = parse_rce(NOV5).unwrap();
        assert_eq!(slots.len(), 96);
        assert_eq!(slots[0].start, utc("2026-11-04T23:00:00Z"));
        assert_eq!(slots[0].end, utc("2026-11-04T23:15:00Z"));
        assert_eq!(slots[0].business_date, d(2026, 11, 5));
        assert_eq!(slots[0].price, 989.70);
        assert_eq!(slots[95].end, utc("2026-11-05T23:00:00Z"));
        assert!(slots.windows(2).all(|w| w[0].end == w[1].start));
    }

    #[test]
    fn parses_the_long_dst_day() {
        let slots = parse_rce(OCT25).unwrap();
        assert_eq!(slots.len(), 100);
        assert_eq!(slots[0].start, utc("2026-10-24T22:00:00Z"));
        assert!(
            slots.iter().any(|s| s.price < 0.0),
            "the fixture has a negative price"
        );
        assert!(slots.windows(2).all(|w| w[0].end == w[1].start));
    }

    #[test]
    fn empty_day_is_no_slots() {
        assert!(parse_rce(br#"{"value":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_rce(b"<html>").is_err());
        let off_grid = br#"{"value":[{"dtime_utc":"2026-11-04 23:10:00","rce_pln":1.0,"business_date":"2026-11-05"}]}"#;
        assert!(
            parse_rce(off_grid).is_err(),
            "slot ends must be on the 15-minute grid"
        );
    }

    #[test]
    fn url_filters_by_business_date() {
        assert_eq!(
            day_url("https://api.raporty.pse.pl/api/rce-pln", d(2026, 11, 5)),
            "https://api.raporty.pse.pl/api/rce-pln?$filter=business_date%20eq%20'2026-11-05'"
        );
    }

    #[test]
    fn fetches_missing_days_and_tomorrow_after_publication() {
        let mut have = HashMap::new();
        have.insert(d(2026, 11, 3), 96);
        have.insert(d(2026, 11, 4), 50); // incomplete
        have.insert(d(2026, 11, 5), 96);

        // 12:00 local: tomorrow is not published yet.
        let noon = utc("2026-11-05T11:00:00Z");
        assert_eq!(
            dates_to_fetch(noon, &have, 3),
            [d(2026, 11, 2), d(2026, 11, 4)]
        );

        // 14:00 local: tomorrow too.
        let afternoon = utc("2026-11-05T13:00:00Z");
        assert_eq!(
            dates_to_fetch(afternoon, &have, 3),
            [d(2026, 11, 2), d(2026, 11, 4), d(2026, 11, 6)]
        );

        have.insert(d(2026, 11, 2), 96);
        have.insert(d(2026, 11, 4), 96);
        have.insert(d(2026, 11, 6), 96);
        assert!(dates_to_fetch(afternoon, &have, 3).is_empty());
    }

    #[test]
    fn dst_day_needs_100_slots() {
        let mut have = HashMap::new();
        have.insert(d(2026, 10, 25), 96);
        let now = utc("2026-10-25T10:00:00Z");
        assert_eq!(dates_to_fetch(now, &have, 0), [d(2026, 10, 25)]);
    }

    #[test]
    fn export_value() {
        assert!((export_value_pln(2.0, 500.0, 1.0) - 1.0).abs() < 1e-9);
        assert_eq!(export_value_pln(2.0, -11.26, 1.0), 0.0);
        assert!((export_value_pln(1.0, 1000.0, 1.23) - 1.23).abs() < 1e-9);
    }
}

/// Downloads a URL with curl (the binary stays free of a TLS stack).
pub async fn fetch(url: &str, timeout: std::time::Duration) -> anyhow::Result<Vec<u8>> {
    let output = tokio::process::Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--location"])
        .arg("--max-time")
        .arg(timeout.as_secs().max(1).to_string())
        .arg("--")
        .arg(url)
        .kill_on_drop(true)
        .output()
        .await
        .context("running curl")?;
    if !output.status.success() {
        bail!(
            "curl {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod fetch_tests {
    use std::time::Duration;

    use super::*;

    fn fixture_url() -> String {
        format!(
            "file://{}/tests/fixtures/rce-2026-11-05.json",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    #[tokio::test]
    async fn fetches_with_curl() {
        let body = fetch(&fixture_url(), Duration::from_secs(5)).await.unwrap();
        assert_eq!(parse_rce(&body).unwrap().len(), 96);
    }

    #[tokio::test]
    async fn reports_failures() {
        let err = fetch("file:///nonexistent/wattledger", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("curl"), "{err:#}");
    }
}
