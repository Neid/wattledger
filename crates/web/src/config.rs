use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, ensure};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub database_url: String,
    #[serde(default = "default_victoria")]
    pub victoria_url: String,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub prices: PricesConfig,
    #[serde(default)]
    pub control: ControlConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topic_prefix: String,
    pub inverter: String,
    #[serde(with = "humantime_serde")]
    pub command_timeout: Duration,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1883,
            client_id: "wattledger-web".into(),
            username: None,
            password: None,
            topic_prefix: "wattledger".into(),
            inverter: "deye".into(),
            command_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PricesConfig {
    pub url: String,
    pub backfill_days: u32,
    #[serde(with = "humantime_serde")]
    pub refresh: Duration,
}

impl Default for PricesConfig {
    fn default() -> Self {
        Self {
            url: "https://api.raporty.pse.pl/api/rce-pln".into(),
            backfill_days: 90,
            refresh: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ControlConfig {
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
        }
    }
}

fn default_listen() -> SocketAddr {
    "0.0.0.0:8080".parse().expect("valid address")
}

fn default_victoria() -> String {
    "http://127.0.0.1:8428".into()
}

pub fn parse(text: &str) -> anyhow::Result<Config> {
    let cfg: Config = toml::from_str(text)?;
    ensure!(
        cfg.victoria_url.starts_with("http://"),
        "victoria_url must be a plain http:// URL"
    );
    ensure!(
        cfg.prices.url.starts_with("https://") || cfg.prices.url.starts_with("http://"),
        "prices.url must be an http(s) URL"
    );
    ensure!(
        cfg.prices.backfill_days <= 3650,
        "prices.backfill_days is at most 3650"
    );
    ensure!(!cfg.prices.refresh.is_zero(), "prices.refresh must be > 0");
    ensure!(
        !cfg.control.interval.is_zero(),
        "control.interval must be > 0"
    );
    ensure!(
        !cfg.mqtt.topic_prefix.is_empty() && !cfg.mqtt.topic_prefix.contains(['#', '+']),
        "mqtt.topic_prefix must be a plain topic"
    );
    Ok(cfg)
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_config_loads() {
        let cfg = load(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/web.toml"
        )))
        .unwrap();
        assert_eq!(cfg.listen.port(), 8080);
        assert_eq!(cfg.mqtt.inverter, "deye");
        assert_eq!(cfg.mqtt.topic_prefix, "wattledger");
        assert_eq!(cfg.prices.backfill_days, 90);
        assert_eq!(cfg.control.interval, Duration::from_secs(5));
    }

    #[test]
    fn defaults_and_checks() {
        let cfg = parse("database_url = \"postgres:///x\"").unwrap();
        assert_eq!(cfg.victoria_url, "http://127.0.0.1:8428");
        assert_eq!(cfg.prices.refresh, Duration::from_secs(900));
        assert!(parse("").is_err(), "database_url is required");
        assert!(parse("database_url = \"x\"\nvictoria_url = \"https://vm\"").is_err());
        assert!(parse("database_url = \"x\"\ntypo = 1").is_err());
    }
}
