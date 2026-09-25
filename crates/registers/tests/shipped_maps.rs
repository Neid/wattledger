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
fn deye_loads_with_only_soc_verified() {
    let map = load("deye-sg05lp3.toml");
    assert_eq!(map.device.id, "deye");
    let verified: Vec<&str> = map
        .sensors
        .iter()
        .filter(|s| s.verified)
        .map(|s| s.key.as_str())
        .collect();
    assert_eq!(verified, ["battery_soc"]);
}
