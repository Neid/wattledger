//! Home Assistant MQTT discovery, shaped for Domoticz's
//! "MQTT Auto Discovery Client Gateway" (hardware/MQTTAutoDiscover.cpp).

use registers::{DeviceMap, Sensor};
use serde_json::{Map, Value, json};

use crate::config::MqttConfig;

/// Stable forever: Domoticz uses it as the device's database key.
pub fn unique_id(map: &DeviceMap, sensor: &Sensor) -> String {
    format!("{}_{}", map.device.id, sensor.key)
}

pub fn state_topic(cfg: &MqttConfig, map: &DeviceMap, sensor: &Sensor) -> String {
    format!("{}/{}/{}", cfg.topic_prefix, map.device.id, sensor.key)
}

pub fn availability_topic(cfg: &MqttConfig) -> String {
    format!("{}/status", cfg.topic_prefix)
}

/// Where Home Assistant (and Domoticz) announce that they (re)started.
pub fn birth_topic(cfg: &MqttConfig) -> String {
    format!("{}/status", cfg.discovery_prefix)
}

/// Domoticz takes the object id from this topic, so it is the unique_id.
pub fn config_topic(cfg: &MqttConfig, map: &DeviceMap, sensor: &Sensor) -> String {
    format!(
        "{}/sensor/{}/config",
        cfg.discovery_prefix,
        unique_id(map, sensor)
    )
}

pub fn is_published(cfg: &MqttConfig, sensor: &Sensor) -> bool {
    sensor.mqtt && (sensor.verified || cfg.publish_unverified)
}

pub fn config_payload(cfg: &MqttConfig, map: &DeviceMap, sensor: &Sensor) -> String {
    let mut device = Map::new();
    device.insert(
        "identifiers".into(),
        json!([format!("wattledger_{}", map.device.id)]),
    );
    device.insert("name".into(), json!(map.device.name));
    if let Some(m) = &map.device.manufacturer {
        device.insert("manufacturer".into(), json!(m));
    }
    if let Some(m) = &map.device.model {
        device.insert("model".into(), json!(m));
    }

    let mut payload = Map::new();
    payload.insert("name".into(), json!(sensor.name));
    payload.insert("unique_id".into(), json!(unique_id(map, sensor)));
    payload.insert("state_topic".into(), json!(state_topic(cfg, map, sensor)));
    payload.insert("availability_topic".into(), json!(availability_topic(cfg)));
    payload.insert("payload_available".into(), json!("online"));
    payload.insert("payload_not_available".into(), json!("offline"));
    if let Some(unit) = &sensor.unit {
        payload.insert("unit_of_measurement".into(), json!(unit));
    }
    if let Some(class) = &sensor.device_class {
        payload.insert("device_class".into(), json!(class));
    }
    if let Some(class) = sensor.state_class {
        payload.insert("state_class".into(), json!(class.as_str()));
    }
    payload.insert(
        "suggested_display_precision".into(),
        json!(sensor.precision),
    );
    payload.insert("device".into(), Value::Object(device));
    Value::Object(payload).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(name: &str) -> DeviceMap {
        let path = format!("{}/../../config/devices/{name}", env!("CARGO_MANIFEST_DIR"));
        DeviceMap::from_toml(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn payload_has_required_fields() {
        let map = load("deye-sg05lp3.toml");
        let cfg = MqttConfig::default();
        let soc = &map.sensors[map.sensor_index("battery_soc").unwrap()];
        assert_eq!(
            config_topic(&cfg, &map, soc),
            "homeassistant/sensor/deye_battery_soc/config"
        );
        let v: Value = serde_json::from_str(&config_payload(&cfg, &map, soc)).unwrap();
        assert_eq!(v["unique_id"], "deye_battery_soc");
        assert_eq!(v["state_topic"], "wattledger/deye/battery_soc");
        assert_eq!(v["availability_topic"], "wattledger/status");
        assert_eq!(v["unit_of_measurement"], "%");
        assert_eq!(v["device_class"], "battery");
        assert_eq!(v["state_class"], "measurement");
        assert_eq!(v["device"]["identifiers"][0], "wattledger_deye");
        // A battery-class entity marked diagnostic would be absorbed into the
        // device's battery indicator by Domoticz instead of becoming a sensor.
        assert!(v.get("entity_category").is_none());
    }

    #[test]
    fn unverified_sensors_stay_private() {
        let map = load("deye-sg05lp3.toml");
        let mut cfg = MqttConfig::default();
        let pv = &map.sensors[map.sensor_index("pv1_power").unwrap()];
        assert!(!is_published(&cfg, pv));
        cfg.publish_unverified = true;
        assert!(is_published(&cfg, pv));
        let today = &map.sensors[map.sensor_index("pv_today_energy").unwrap()];
        assert!(!is_published(&cfg, today), "mqtt = false always wins");
    }

    /// Domoticz's get_auto_discovery_sensor_unit(): among sensors of the same
    /// device with the wanted unit, pick the one whose unique_id differs from
    /// ours at the latest position. Ids that are a prefix of one another never
    /// match, and on a tie the first one found wins, so ties must not happen.
    fn domoticz_partner<'a>(own: &str, candidates: &[&'a str]) -> Result<Option<&'a str>, String> {
        let mut best: Option<(usize, &str)> = None;
        let mut tied = false;
        for &cand in candidates {
            if cand == own {
                return Ok(Some(cand));
            }
            let len = own.len().min(cand.len());
            let Some(mismatch) = (1..len).find(|&i| own.as_bytes()[i] != cand.as_bytes()[i]) else {
                continue;
            };
            match best {
                Some((m, _)) if mismatch < m => {}
                Some((m, _)) if mismatch == m => tied = true,
                _ => {
                    best = Some((mismatch, cand));
                    tied = false;
                }
            }
        }
        if tied {
            return Err(format!("{own}: ambiguous W partner"));
        }
        Ok(best.map(|(_, c)| c))
    }

    /// Every published kWh sensor "<x>_energy" must be merged with its own
    /// "<x>_power" sensor, whatever else is later published beside it.
    #[test]
    fn energy_sensors_pair_with_their_power_sensor() {
        let cfg = MqttConfig {
            publish_unverified: true,
            ..MqttConfig::default()
        };
        for file in ["dtsu666.toml", "deye-sg05lp3.toml"] {
            let map = load(file);
            let published: Vec<&Sensor> = map
                .sensors
                .iter()
                .filter(|s| is_published(&cfg, s))
                .collect();
            let ids_with_unit = |unit: &str| -> Vec<String> {
                published
                    .iter()
                    .filter(|s| {
                        s.unit
                            .as_deref()
                            .is_some_and(|u| u.eq_ignore_ascii_case(unit))
                    })
                    .map(|s| unique_id(&map, s))
                    .collect()
            };
            let watts = ids_with_unit("W");
            let watt_refs: Vec<&str> = watts.iter().map(String::as_str).collect();
            let energies = ids_with_unit("kWh");
            assert!(!energies.is_empty(), "{file}: no energy sensors");
            for energy in &energies {
                let expected = energy.strip_suffix("_energy").map(|x| format!("{x}_power"));
                let expected =
                    expected.unwrap_or_else(|| panic!("{energy}: energy keys must end in _energy"));
                let partner = domoticz_partner(energy, &watt_refs).unwrap();
                assert_eq!(partner, Some(expected.as_str()), "{file}: {energy}");
            }
        }
    }
}
