//! The map files in `config/devices` must always load.

use registers::{DeviceMap, Source};

fn load(name: &str) -> DeviceMap {
    let path = format!("{}/../../config/devices/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    DeviceMap::from_toml(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn dtsu666_loads() {
    let map = load("dtsu666.toml");
    assert_eq!(map.device.id, "dtsu");
    let pt = &map.sensors[map.sensor_index("grid_power").unwrap()];
    let Source::Register(src) = &pt.source else {
        panic!("grid_power is a register")
    };
    assert_eq!(src.address, 8210);
    assert!(pt.verified);
    let pf = &map.sensors[map.sensor_index("power_factor").unwrap()];
    assert!(!pf.verified);
}

#[test]
fn deye_loads_with_all_sensors_verified() {
    let map = load("deye-sg05lp3.toml");
    assert_eq!(map.device.id, "deye");
    let unverified: Vec<&str> = map
        .sensors
        .iter()
        .filter(|s| !s.verified)
        .map(|s| s.key.as_str())
        .collect();
    assert!(unverified.is_empty(), "unverified: {unverified:?}");
}

/// The export controller in the web server writes exactly these keys, plus
/// the slot times it only reads.
#[test]
fn deye_declares_export_settings() {
    let map = load("deye-sg05lp3.toml");
    let mut expected: Vec<(String, u16)> = vec![
        ("work_mode".into(), 142),
        ("max_sell_power".into(), 143),
        ("solar_sell".into(), 145),
        ("tou_selling".into(), 146),
    ];
    for slot in 1..=6u16 {
        expected.push((format!("tou_power_{slot}"), 153 + slot));
        expected.push((format!("tou_soc_{slot}"), 165 + slot));
        expected.push((format!("tou_charge_{slot}"), 171 + slot));
    }
    // Read for display only, never written by an export.
    for slot in 1..=6u16 {
        expected.push((format!("tou_time_{slot}"), 147 + slot));
    }
    for (key, address) in &expected {
        let i = map
            .setting_index(key)
            .unwrap_or_else(|| panic!("setting {key} missing"));
        assert_eq!(map.settings[i].address, *address, "{key}");
    }
    assert_eq!(
        map.settings.len(),
        expected.len(),
        "unexpected extra settings"
    );
    let mode = &map.settings[map.setting_index("work_mode").unwrap()];
    assert_eq!(mode.range, (0.0, 2.0));
    let time = &map.settings[map.setting_index("tou_time_1").unwrap()];
    assert_eq!(time.range, (0.0, 2359.0));
    assert!(!time.verified, "slot times are never written");
    let soc = &map.settings[map.setting_index("tou_soc_1").unwrap()];
    assert_eq!(soc.range, (0.0, 100.0));
}

/// Checked on the inverter (2026-11-05): max sell power 5 kW read as raw
/// 5000 and a 4 kW time-of-use slot as raw 4000, so power registers are in
/// watts. ha-solarman's x10 does not apply to this inverter.
#[test]
fn deye_power_settings_are_in_watts() {
    let map = load("deye-sg05lp3.toml");
    let mut keys = vec!["max_sell_power".to_owned()];
    keys.extend((1..=6).map(|slot| format!("tou_power_{slot}")));
    for key in &keys {
        let s = &map.settings[map.setting_index(key).unwrap()];
        assert_eq!(s.scale, 1.0, "{key}");
        assert_eq!(s.unit.as_deref(), Some("W"), "{key}");
        assert_eq!(s.decode(5000), 5000.0, "{key}");
        assert_eq!(s.encode(4000.0), Ok(4000), "{key}");
    }
}

#[test]
fn dtsu666_has_no_settings() {
    assert!(load("dtsu666.toml").settings.is_empty());
}
