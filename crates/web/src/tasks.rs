//! Background work: MQTT (live values and command answers), the price job
//! and the export controller.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, NaiveDate, Utc};
use rumqttc::{AsyncClient, Event, EventLoop, Packet, QoS};
use sqlx::PgPool;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{MqttConfig, PricesConfig};
use crate::control::bridge::MqttInverter;
use crate::control::run;
use crate::db;
use crate::live::LiveState;
use crate::prices;

const CURL_TIMEOUT: StdDuration = StdDuration::from_secs(30);

/// Fetches every missing day once. Returns how many days were stored;
/// a day that fails is logged and left for the next pass.
pub async fn fetch_missing<F, Fut>(
    pool: &PgPool,
    now: DateTime<Utc>,
    backfill_days: u32,
    fetch_day: F,
) -> anyhow::Result<usize>
where
    F: Fn(NaiveDate) -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<u8>>>,
{
    let today = crate::time::local_date(now);
    let from = today - chrono::Duration::days(i64::from(backfill_days));
    let have = db::slot_counts(pool, from, today + chrono::Duration::days(1)).await?;
    let mut stored = 0;
    for date in prices::dates_to_fetch(now, &have, backfill_days) {
        let slots = match fetch_day(date)
            .await
            .and_then(|body| prices::parse_rce(&body))
        {
            Ok(slots) => slots,
            Err(e) => {
                warn!(%date, error = %format!("{e:#}"), "cannot fetch RCE prices");
                continue;
            }
        };
        let slots: Vec<_> = slots
            .into_iter()
            .filter(|s| s.business_date == date)
            .collect();
        if slots.is_empty() {
            continue;
        }
        db::upsert_prices(pool, &slots).await?;
        info!(%date, slots = slots.len(), "RCE prices stored");
        stored += 1;
    }
    Ok(stored)
}

/// Looks for missing prices every `refresh` until shutdown.
pub async fn price_loop(pool: PgPool, cfg: PricesConfig, mut stop: watch::Receiver<bool>) {
    loop {
        let base = cfg.url.clone();
        let pass = fetch_missing(&pool, Utc::now(), cfg.backfill_days, |date| {
            let url = prices::day_url(&base, date);
            async move { prices::fetch(&url, CURL_TIMEOUT).await }
        });
        if let Err(e) = pass.await {
            warn!(error = %format!("{e:#}"), "price job failed");
        }
        tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            () = tokio::time::sleep(cfg.refresh) => {}
        }
    }
}

/// Runs export plans every `interval` until shutdown.
pub async fn control_loop(
    pool: PgPool,
    inverter: Arc<MqttInverter>,
    live: Arc<RwLock<LiveState>>,
    interval: StdDuration,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            _ = ticker.tick() => {}
        }
        let now = Utc::now();
        let soc = live.read().expect("not poisoned").soc(now);
        if let Err(e) = run::tick(&pool, inverter.as_ref(), now, soc).await {
            warn!(error = %format!("{e:#}"), "export controller pass failed");
        }
    }
}

/// Drives the MQTT connection: live states into `live`, command answers to
/// the waiting sender. Subscribes again after every reconnect.
pub async fn mqtt_loop(
    mut eventloop: EventLoop,
    client: AsyncClient,
    cfg: MqttConfig,
    live: Arc<RwLock<LiveState>>,
    inverter: Arc<MqttInverter>,
) {
    let states = format!("{}/+/+", cfg.topic_prefix);
    let results = inverter.result_topic();
    let mut backoff = StdDuration::from_secs(1);
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                info!(host = %cfg.host, "MQTT connected");
                backoff = StdDuration::from_secs(1);
                for topic in [&states, &results] {
                    if let Err(e) = client.try_subscribe(topic.as_str(), QoS::AtLeastOnce) {
                        warn!(topic = %topic, error = %e, "cannot subscribe");
                    }
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                if p.topic == results {
                    inverter.resolve(&p.payload);
                } else {
                    live.write().expect("not poisoned").apply(
                        &cfg.topic_prefix,
                        &p.topic,
                        &p.payload,
                        Utc::now(),
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, retry_in = ?backoff, "MQTT connection failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(StdDuration::from_secs(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOV5: &[u8] = include_bytes!("../tests/fixtures/rce-2026-11-05.json");

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap()
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn stores_what_it_can(pool: PgPool) {
        let now: DateTime<Utc> = "2026-11-05T10:00:00Z".parse().unwrap();
        let asked = std::sync::Mutex::new(Vec::new());
        let stored = fetch_missing(&pool, now, 2, |date| {
            asked.lock().unwrap().push(date);
            async move {
                match date.day0() {
                    4 => Ok(NOV5.to_vec()),               // the 5th
                    3 => Ok(br#"{"value":[]}"#.to_vec()), // not published
                    _ => anyhow::bail!("PSE is down"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(*asked.lock().unwrap(), [d(3), d(4), d(5)]);
        assert_eq!(stored, 1);
        let counts = db::slot_counts(&pool, d(1), d(6)).await.unwrap();
        assert_eq!(counts.get(&d(5)), Some(&96));

        // Complete days are not asked for again.
        let asked_again = std::sync::Mutex::new(Vec::new());
        fetch_missing(&pool, now, 2, |date| {
            asked_again.lock().unwrap().push(date);
            async { anyhow::bail!("down") }
        })
        .await
        .unwrap();
        assert_eq!(*asked_again.lock().unwrap(), [d(3), d(4)]);
    }

    use chrono::Datelike;
}
