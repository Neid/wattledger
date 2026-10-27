use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use registers::DeviceMap;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(rename = "bus")]
    buses: Vec<BusConfig>,
    #[serde(default)]
    mqtt: MqttConfig,
    #[serde(default)]
    influx: InfluxConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusConfig {
    pub name: String,
    pub port: String,
    #[serde(default = "default_baud")]
    pub baud: u32,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    #[serde(with = "humantime_serde", default = "default_request_timeout")]
    pub request_timeout: Duration,
    /// Quiet time between the end of one response and the next request.
    #[serde(with = "humantime_serde", default = "default_inter_request_gap")]
    pub inter_request_gap: Duration,
    /// Consecutive timeouts after which the port is closed and reopened.
    #[serde(default = "default_reopen_after_timeouts")]
    pub reopen_after_timeouts: u32,
    /// Silence after which the port's USB adapter is reset (needs the root
    /// helper from deploy/). None disables resets.
    #[serde(default, with = "humantime_serde::option")]
    pub usb_reset_after: Option<Duration>,
    pub(crate) devices: Vec<DeviceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceEntry {
    slave: u8,
    map: PathBuf,
    #[serde(default, with = "humantime_serde::option")]
    mqtt_interval: Option<Duration>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MqttConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(with = "humantime_serde")]
    pub keep_alive: Duration,
    /// Root of state and availability topics.
    pub topic_prefix: String,
    pub discovery_prefix: String,
    /// Default minimum time between state publishes per device. Domoticz
    /// writes every update to its database, so polling speed is not wanted.
    #[serde(with = "humantime_serde")]
    pub publish_interval: Duration,
    /// Also announce sensors whose register mapping is not yet verified.
    pub publish_unverified: bool,
    /// Optional virtual P1 Smart Meter in Domoticz, fed via `domoticz/in`.
    pub domoticz_p1: Option<DomoticzP1Config>,
}

/// A Domoticz P1 Smart Meter device (created by hand, addressed by idx) that
/// gets one device's grid import/export counters and signed grid power.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomoticzP1Config {
    pub idx: u32,
    /// Device id from the device map, e.g. "dtsu".
    pub device: String,
    #[serde(default = "default_p1_import")]
    pub import_energy: String,
    #[serde(default = "default_p1_export")]
    pub export_energy: String,
    /// Signed power, + import / - export.
    #[serde(default = "default_p1_power")]
    pub power: String,
    #[serde(default = "default_p1_topic")]
    pub topic: String,
}

fn default_p1_import() -> String {
    "grid_import_energy".into()
}
fn default_p1_export() -> String {
    "grid_export_energy".into()
}
fn default_p1_power() -> String {
    "grid_power".into()
}
fn default_p1_topic() -> String {
    "domoticz/in".into()
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 1883,
            client_id: "wattledger-bridge".into(),
            username: None,
            password: None,
            keep_alive: Duration::from_secs(30),
            topic_prefix: "wattledger".into(),
            discovery_prefix: "homeassistant".into(),
            publish_interval: Duration::from_secs(10),
            publish_unverified: false,
            domoticz_p1: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InfluxConfig {
    pub enabled: bool,
    /// Full write URL including any query parameters, e.g.
    /// VictoriaMetrics / InfluxDB v1: http://host:8428/write (add ?db=name for v1)
    /// InfluxDB v2: http://host:8086/api/v2/write?org=home&bucket=energy
    pub url: String,
    /// Sent as "Authorization: Token <token>" (InfluxDB v2).
    pub token: Option<String>,
    #[serde(with = "humantime_serde")]
    pub batch_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Lines kept while the database is unreachable; the oldest are dropped first.
    pub buffer_lines: usize,
    pub max_batch_lines: usize,
}

impl Default for InfluxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://127.0.0.1:8428/write".into(),
            token: None,
            batch_interval: Duration::from_secs(10),
            request_timeout: Duration::from_secs(5),
            buffer_lines: 10_000,
            max_batch_lines: 5_000,
        }
    }
}

/// A reset takes a few seconds; resetting more often only adds outages.
const MIN_USB_RESET_AFTER: Duration = Duration::from_secs(30);

fn default_baud() -> u32 {
    9600
}
fn default_request_timeout() -> Duration {
    Duration::from_millis(500)
}
fn default_inter_request_gap() -> Duration {
    Duration::from_millis(50)
}
fn default_reopen_after_timeouts() -> u32 {
    5
}

/// Everything the bridge needs, with device maps loaded and validated.
#[derive(Debug)]
pub struct Setup {
    pub buses: Vec<BusSetup>,
    pub mqtt: MqttConfig,
    pub influx: InfluxConfig,
}

#[derive(Debug)]
pub struct BusSetup {
    pub config: BusConfig,
    pub devices: Vec<DeviceSetup>,
}

#[derive(Debug)]
pub struct DeviceSetup {
    pub slave: u8,
    pub map: Arc<DeviceMap>,
    pub mqtt_interval: Duration,
}

/// Loads the main config and every device map it references. Map paths are
/// relative to the main config file.
pub fn load(path: &Path) -> anyhow::Result<Setup> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: ConfigFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));

    ensure!(!file.buses.is_empty(), "no [[bus]] configured");
    validate_topic("mqtt.topic_prefix", &file.mqtt.topic_prefix)?;
    validate_topic("mqtt.discovery_prefix", &file.mqtt.discovery_prefix)?;
    if file.influx.enabled {
        ensure!(
            file.influx.url.starts_with("http://"),
            "influx.url must be a plain http:// URL (TLS is not compiled in)"
        );
        ensure!(
            file.influx.buffer_lines > 0 && file.influx.max_batch_lines > 0,
            "influx buffer sizes must be > 0"
        );
    }

    let mut bus_names = HashSet::new();
    let mut device_ids = HashSet::new();
    let mut buses = Vec::with_capacity(file.buses.len());
    for bus in file.buses {
        // The name is part of the USB reset request file name.
        ensure!(
            !bus.name.is_empty()
                && bus
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "bus name {:?} may only contain letters, digits, '-' and '_'",
            bus.name
        );
        ensure!(
            bus_names.insert(bus.name.clone()),
            "duplicate bus name {:?}",
            bus.name
        );
        ensure!(!bus.devices.is_empty(), "bus {:?} has no devices", bus.name);
        ensure!(
            !bus.poll_interval.is_zero(),
            "bus {:?}: poll_interval must be > 0",
            bus.name
        );
        ensure!(
            !bus.request_timeout.is_zero(),
            "bus {:?}: request_timeout must be > 0",
            bus.name
        );
        ensure!(
            bus.reopen_after_timeouts > 0,
            "bus {:?}: reopen_after_timeouts must be > 0",
            bus.name
        );
        if let Some(after) = bus.usb_reset_after {
            ensure!(
                after >= MIN_USB_RESET_AFTER,
                "bus {:?}: usb_reset_after must be at least {}",
                bus.name,
                humantime::format_duration(MIN_USB_RESET_AFTER)
            );
        }

        let mut slaves = HashSet::new();
        let mut devices = Vec::with_capacity(bus.devices.len());
        for entry in &bus.devices {
            ensure!(
                (1..=247).contains(&entry.slave),
                "bus {:?}: slave address {} is outside 1..=247",
                bus.name,
                entry.slave
            );
            ensure!(
                slaves.insert(entry.slave),
                "bus {:?}: slave {} listed twice",
                bus.name,
                entry.slave
            );
            let map_path = base.join(&entry.map);
            let map_text = std::fs::read_to_string(&map_path)
                .with_context(|| format!("reading device map {}", map_path.display()))?;
            let map = DeviceMap::from_toml(&map_text)
                .with_context(|| format!("loading device map {}", map_path.display()))?;
            if !device_ids.insert(map.device.id.clone()) {
                bail!(
                    "device id {:?} is used by more than one device",
                    map.device.id
                );
            }
            devices.push(DeviceSetup {
                slave: entry.slave,
                map: Arc::new(map),
                mqtt_interval: entry.mqtt_interval.unwrap_or(file.mqtt.publish_interval),
            });
        }
        buses.push(BusSetup {
            config: bus,
            devices,
        });
    }

    if let Some(p1) = &file.mqtt.domoticz_p1 {
        validate_p1(p1, &buses)?;
    }

    Ok(Setup {
        buses,
        mqtt: file.mqtt,
        influx: file.influx,
    })
}

fn validate_p1(p1: &DomoticzP1Config, buses: &[BusSetup]) -> anyhow::Result<()> {
    ensure!(p1.idx > 0, "mqtt.domoticz_p1.idx must be the Domoticz device idx (> 0)");
    validate_topic("mqtt.domoticz_p1.topic", &p1.topic)?;
    let Some(map) = buses
        .iter()
        .flat_map(|b| &b.devices)
        .map(|d| &d.map)
        .find(|m| m.device.id == p1.device)
    else {
        bail!("mqtt.domoticz_p1.device {:?} is not a configured device", p1.device);
    };
    for (field, key, unit) in [
        ("import_energy", &p1.import_energy, "kWh"),
        ("export_energy", &p1.export_energy, "kWh"),
        ("power", &p1.power, "W"),
    ] {
        let Some(i) = map.sensor_index(key) else {
            bail!("mqtt.domoticz_p1.{field}: device {:?} has no sensor {key:?}", p1.device);
        };
        let got = map.sensors[i].unit.as_deref().unwrap_or("");
        ensure!(
            got == unit,
            "mqtt.domoticz_p1.{field}: sensor {key:?} is in {got:?}, expected {unit}"
        );
    }
    Ok(())
}

fn validate_topic(what: &str, topic: &str) -> anyhow::Result<()> {
    ensure!(
        !topic.is_empty()
            && !topic.contains(['#', '+'])
            && !topic.starts_with('/')
            && !topic.ends_with('/'),
        "{what} {topic:?} must be a non-empty topic without wildcards or leading/trailing '/'"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_loads() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/bridge.toml"
        ));
        let setup = load(path).unwrap();
        assert_eq!(setup.buses.len(), 2);
        let ids: Vec<&str> = setup
            .buses
            .iter()
            .flat_map(|b| b.devices.iter().map(|d| d.map.device.id.as_str()))
            .collect();
        assert_eq!(ids, ["dtsu", "deye"]);
        assert_eq!(setup.buses[0].config.poll_interval, Duration::from_secs(2));
        assert_eq!(setup.buses[1].config.poll_interval, Duration::from_secs(5));
        assert!(setup.buses.iter().all(|b| b.config.usb_reset_after.is_some()));
    }

    /// Loads a config with one DTSU bus plus `extra`.
    fn load_with(name: &str, extra: &str) -> anyhow::Result<Setup> {
        let maps = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/devices");
        let text = format!(
            r#"
            [[bus]]
            name = "grid-meter"
            port = "/dev/null"
            poll_interval = "2s"
            devices = [{{ slave = 1, map = "{maps}/dtsu666.toml" }}]
            {extra}
            "#
        );
        let dir = std::env::temp_dir().join(format!("wattledger-config-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("bridge.toml");
        std::fs::write(&path, text)?;
        let result = load(&path);
        std::fs::remove_dir_all(&dir)?;
        result
    }

    #[test]
    fn p1_defaults_to_grid_sensors() {
        let setup = load_with("p1-ok", "[mqtt.domoticz_p1]\nidx = 42\ndevice = \"dtsu\"").unwrap();
        let p1 = setup.mqtt.domoticz_p1.unwrap();
        assert_eq!(p1.idx, 42);
        assert_eq!(p1.import_energy, "grid_import_energy");
        assert_eq!(p1.power, "grid_power");
        assert_eq!(p1.topic, "domoticz/in");
    }

    #[test]
    fn p1_rejects_bad_setups() {
        for (name, extra, msg) in [
            ("p1-dev", "idx = 1\ndevice = \"deye\"", "not a configured device"),
            ("p1-key", "idx = 1\ndevice = \"dtsu\"\npower = \"nope\"", "no sensor"),
            ("p1-unit", "idx = 1\ndevice = \"dtsu\"\npower = \"l1_voltage\"", "expected W"),
            ("p1-idx", "idx = 0\ndevice = \"dtsu\"", "idx"),
        ] {
            let err = load_with(name, &format!("[mqtt.domoticz_p1]\n{extra}")).unwrap_err();
            assert!(format!("{err:#}").contains(msg), "{name}: {err:#}");
        }
    }
}
