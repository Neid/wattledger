//! Feeds a Domoticz "P1 Smart Meter" device over MQTT (`domoticz/in`).
//!
//! Domoticz's Energy Dashboard shows grid export only for a P1 Smart Meter
//! device, and MQTT discovery cannot create one. So a virtual P1 device is
//! created by hand in Domoticz, and the bridge updates it by idx from the
//! grid meter's import/export counters and its signed power.

use serde_json::json;

use crate::config::DomoticzP1Config;
use crate::reading::Reading;

/// The `domoticz/in` payload for this reading, or None when it is from
/// another device or any of the three values is missing or rejected.
pub fn payload(cfg: &DomoticzP1Config, reading: &Reading) -> Option<String> {
    if reading.map.device.id != cfg.device {
        return None;
    }
    let value = |key: &str| {
        let i = reading.map.sensor_index(key)?;
        reading.outcomes.get(i)?.value()
    };
    let svalue = svalue(
        value(&cfg.import_energy)?,
        value(&cfg.export_energy)?,
        value(&cfg.power)?,
    );
    Some(json!({ "idx": cfg.idx, "nvalue": 0, "svalue": svalue }).to_string())
}

/// `USAGE1;USAGE2;RETURN1;RETURN2;CONS;PROD`: counters in Wh (all on tariff
/// 1), current import and export in W. `power_w` is + import / - export.
fn svalue(import_kwh: f64, export_kwh: f64, power_w: f64) -> String {
    let wh = |kwh: f64| (kwh * 1000.0).round().max(0.0) as u64;
    let w = power_w.round();
    format!(
        "{};0;{};0;{};{}",
        wh(import_kwh),
        wh(export_kwh),
        w.max(0.0) as u64,
        (-w).max(0.0) as u64
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use registers::{DeviceMap, Outcome};
    use serde_json::Value;

    use super::*;

    fn cfg() -> DomoticzP1Config {
        DomoticzP1Config {
            idx: 77,
            device: "dtsu".into(),
            import_energy: "grid_import_energy".into(),
            export_energy: "grid_export_energy".into(),
            power: "grid_power".into(),
            topic: "domoticz/in".into(),
        }
    }

    fn reading(values: &[(&str, f64)]) -> Reading {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/devices/dtsu666.toml");
        let map = Arc::new(DeviceMap::from_toml(&std::fs::read_to_string(path).unwrap()).unwrap());
        let outcomes = map
            .sensors
            .iter()
            .map(|s| match values.iter().find(|(k, _)| *k == s.key) {
                Some(&(_, v)) => Outcome::Value(v),
                None => Outcome::Unavailable,
            })
            .collect();
        Reading {
            bus: "grid-meter".into(),
            slave: 1,
            timestamp: SystemTime::now(),
            blocks: Vec::new(),
            outcomes,
            map,
        }
    }

    #[test]
    fn importing() {
        assert_eq!(svalue(90.98, 0.02, 97.4), "90980;0;20;0;97;0");
    }

    #[test]
    fn exporting() {
        assert_eq!(svalue(91.0, 1.5, -1234.6), "91000;0;1500;0;0;1235");
    }

    #[test]
    fn negative_zero_power_is_zero() {
        assert_eq!(svalue(1.0, 0.0, -0.2), "1000;0;0;0;0;0");
    }

    #[test]
    fn payload_from_reading() {
        let r = reading(&[
            ("grid_import_energy", 90.98),
            ("grid_export_energy", 0.02),
            ("grid_power", -250.0),
        ]);
        let v: Value = serde_json::from_str(&payload(&cfg(), &r).unwrap()).unwrap();
        assert_eq!(v["idx"], 77);
        assert_eq!(v["nvalue"], 0);
        assert_eq!(v["svalue"], "90980;0;20;0;0;250");
    }

    #[test]
    fn missing_value_skips_update() {
        let r = reading(&[("grid_import_energy", 90.98), ("grid_power", 10.0)]);
        assert_eq!(payload(&cfg(), &r), None);
    }

    #[test]
    fn other_device_is_ignored() {
        let mut c = cfg();
        c.device = "deye".into();
        let r = reading(&[
            ("grid_import_energy", 1.0),
            ("grid_export_energy", 1.0),
            ("grid_power", 1.0),
        ]);
        assert_eq!(payload(&c, &r), None);
    }
}
