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

    Ok(Setup {
        buses,
        mqtt: file.mqtt,
        influx: file.influx,
    })
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
}
