//! Latest sensor values from MQTT (`<prefix>/<device>/<key>`), shaped for the
//! energy flow page.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use crate::energy::{Metric, Totals};
use crate::time::TZ;

/// Values older than this are ignored for decisions.
pub const MAX_AGE: Duration = Duration::minutes(2);
pub const SOC: &str = "deye_battery_soc";

#[derive(Debug, Default, Clone)]
pub struct LiveState {
    values: HashMap<String, (f64, DateTime<Utc>)>,
}

impl LiveState {
    /// Records a state message. Returns false for topics that are not sensor
    /// states (status, commands, other prefixes) or non-numeric payloads.
    pub fn apply(&mut self, prefix: &str, topic: &str, payload: &[u8], now: DateTime<Utc>) -> bool {
        let Some(rest) = topic.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) else {
            return false;
        };
        let mut parts = rest.split('/');
        let (Some(device), Some(key), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        if device == "command" || device.is_empty() || key.is_empty() {
            return false;
        }
        let Some(value) = std::str::from_utf8(payload)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
        else {
            return false;
        };
        self.values.insert(format!("{device}_{key}"), (value, now));
        true
    }

    pub fn fresh(&self, id: &str, now: DateTime<Utc>) -> Option<f64> {
        let (value, at) = self.values.get(id)?;
        (now - *at <= MAX_AGE).then_some(*value)
    }

    pub fn soc(&self, now: DateTime<Utc>) -> Option<f64> {
        self.fresh(SOC, now)
    }

    /// The Domoticz `getdevices` shape `Energy.html` reads: `ID`, `Data`,
    /// `LastUpdate` and, for energy counters, `CounterToday`.
    pub fn domoticz_json(&self, now: DateTime<Utc>, today: Option<&Totals>) -> Value {
        let mut devices: HashMap<&str, serde_json::Map<String, Value>> = HashMap::new();
        for (id, (value, at)) in &self.values {
            let mut d = serde_json::Map::new();
            d.insert("ID".into(), json!(id));
            d.insert("Data".into(), json!(value.to_string()));
            d.insert("LastUpdate".into(), json!(local_stamp(*at)));
            devices.insert(id.as_str(), d);
        }
        if let Some(today) = today {
            for m in Metric::ALL {
                let d = devices.entry(m.series()).or_insert_with(|| {
                    let mut d = serde_json::Map::new();
                    d.insert("ID".into(), json!(m.series()));
                    d.insert("Data".into(), json!(""));
                    d
                });
                d.insert(
                    "CounterToday".into(),
                    json!(format!("{:.2} kWh", today.get(m))),
                );
            }
        }
        let mut result: Vec<Value> = devices.into_values().map(Value::Object).collect();
        result.sort_by(|a, b| a["ID"].as_str().cmp(&b["ID"].as_str()));
        json!({ "ServerTime": local_stamp(now), "result": result })
    }
}

fn local_stamp(t: DateTime<Utc>) -> String {
    t.with_timezone(&TZ).format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-11-05T12:00:00Z";

    #[test]
    fn records_sensor_states_only() {
        let now = utc(NOW);
        let mut live = LiveState::default();
        assert!(live.apply("wattledger", "wattledger/deye/pv_power", b"1234.5", now));
        assert!(live.apply("wattledger", "wattledger/dtsu/grid_power", b"-250", now));
        assert!(!live.apply("wattledger", "wattledger/status", b"online", now));
        assert!(!live.apply("wattledger", "wattledger/command/deye/result", b"{}", now));
        assert!(!live.apply("wattledger", "wattledger/command/deye", b"1", now));
        assert!(!live.apply("wattledger", "other/deye/pv_power", b"1", now));
        assert!(!live.apply("wattledger", "wattledger/deye/pv_power", b"n/a", now));
        assert_eq!(live.fresh("deye_pv_power", now), Some(1234.5));
        assert_eq!(live.fresh("dtsu_grid_power", now), Some(-250.0));
    }

    #[test]
    fn stale_values_are_not_fresh() {
        let mut live = LiveState::default();
        live.apply("wattledger", "wattledger/deye/battery_soc", b"64", utc(NOW));
        assert_eq!(live.soc(utc(NOW) + Duration::seconds(119)), Some(64.0));
        assert_eq!(live.soc(utc(NOW) + Duration::seconds(121)), None);
    }

    #[test]
    fn domoticz_shape() {
        let now = utc(NOW);
        let mut live = LiveState::default();
        live.apply(
            "wattledger",
            "wattledger/deye/pv_power",
            b"1500",
            now - Duration::seconds(5),
        );
        let today = Totals {
            pv: 12.34,
            export: 1.5,
            ..Totals::default()
        };
        let v = live.domoticz_json(now, Some(&today));
        assert_eq!(v["ServerTime"], "2026-11-05 13:00:00");
        let devices = v["result"].as_array().unwrap();
        let find = |id: &str| devices.iter().find(|d| d["ID"] == id).cloned();
        let pv = find("deye_pv_power").unwrap();
        assert_eq!(pv["Data"], "1500");
        assert_eq!(pv["LastUpdate"], "2026-11-05 12:59:55");
        assert_eq!(find("deye_pv_energy").unwrap()["CounterToday"], "12.34 kWh");
        assert_eq!(
            find("dtsu_grid_export_energy").unwrap()["CounterToday"],
            "1.50 kWh"
        );
        assert_eq!(
            find("deye_load_energy").unwrap()["CounterToday"],
            "0.00 kWh"
        );

        let without = live.domoticz_json(now, None);
        let devices = without["result"].as_array().unwrap();
        assert!(devices.iter().all(|d| d.get("CounterToday").is_none()));
    }

    #[test]
    fn every_metric_has_a_counter_today() {
        let v = LiveState::default().domoticz_json(utc(NOW), Some(&Totals::default()));
        let ids: Vec<&str> = v["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["ID"].as_str().unwrap())
            .collect();
        for m in Metric::ALL {
            assert!(ids.contains(&m.series()), "{}", m.series());
        }
    }
}
