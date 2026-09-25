//! InfluxDB line protocol over HTTP. Works with VictoriaMetrics `/write`,
//! InfluxDB v1 `/write` and InfluxDB v2 `/api/v2/write`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use registers::format_value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info, warn};
use ureq::Agent;

use crate::config::InfluxConfig;
use crate::reading::Reading;
use crate::throttle::ErrorLog;

/// Time allowed for the last flush on shutdown.
const FINAL_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

enum PostResult {
    Written,
    /// The server refused the data itself; retrying would fail the same way.
    Refused(String),
    /// Network or server trouble; worth retrying.
    Failed(String),
}

type Batch = Vec<String>;

pub struct InfluxSink {
    cfg: InfluxConfig,
    agent: Agent,
    /// Lines not yet written, oldest first.
    buffer: VecDeque<String>,
    overflow: ErrorLog,
    failures: ErrorLog,
}

impl InfluxSink {
    pub fn new(cfg: InfluxConfig) -> Self {
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(cfg.request_timeout))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            buffer: VecDeque::with_capacity(cfg.buffer_lines.min(4096)),
            cfg,
            agent,
            overflow: ErrorLog::new(),
            failures: ErrorLog::new(),
        }
    }

    /// Buffers every reading and writes batches every `batch_interval`; runs
    /// until `readings` is closed, then tries one final flush.
    pub async fn run(mut self, mut readings: mpsc::Receiver<Arc<Reading>>) {
        let mut ticker = time::interval(self.cfg.batch_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut inflight: Option<JoinHandle<(Batch, PostResult)>> = None;

        loop {
            tokio::select! {
                reading = readings.recv() => match reading {
                    Some(reading) => {
                        if let Some(line) = to_line(&reading) {
                            self.push(line);
                        }
                    }
                    None => break,
                },
                _ = ticker.tick(), if inflight.is_none() && !self.buffer.is_empty() => {
                    inflight = Some(self.post_next_batch());
                }
                joined = async {
                    match inflight.as_mut() {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                } => {
                    inflight = None;
                    self.handle(joined);
                }
            }
        }

        let flush = async {
            if let Some(task) = inflight.take() {
                self.handle(task.await);
            }
            while !self.buffer.is_empty() {
                let joined = self.post_next_batch().await;
                let failed = !matches!(
                    &joined,
                    Ok((_, PostResult::Written | PostResult::Refused(_)))
                );
                self.handle(joined);
                if failed {
                    break;
                }
            }
        };
        if time::timeout(FINAL_FLUSH_TIMEOUT, flush).await.is_err() || !self.buffer.is_empty() {
            warn!(
                lines = self.buffer.len(),
                "database unreachable at shutdown, lines lost"
            );
        }
    }

    fn push(&mut self, line: String) {
        if self.buffer.len() >= self.cfg.buffer_lines {
            self.buffer.pop_front();
            if let Some(n) = self.overflow.failure() {
                warn!(
                    dropped = n,
                    capacity = self.cfg.buffer_lines,
                    "line buffer full, dropping oldest"
                );
            }
        }
        self.buffer.push_back(line);
    }

    fn post_next_batch(&mut self) -> JoinHandle<(Batch, PostResult)> {
        let n = self.buffer.len().min(self.cfg.max_batch_lines);
        let batch: Batch = self.buffer.drain(..n).collect();
        let agent = self.agent.clone();
        let url = self.cfg.url.clone();
        let token = self.cfg.token.clone();
        tokio::task::spawn_blocking(move || {
            let result = post(&agent, &url, token.as_deref(), &batch);
            (batch, result)
        })
    }

    fn handle(&mut self, joined: Result<(Batch, PostResult), tokio::task::JoinError>) {
        let (batch, result) = match joined {
            Ok(done) => done,
            Err(e) => {
                error!(error = %e, "line protocol writer task failed, batch lost");
                return;
            }
        };
        match result {
            PostResult::Written => {
                if let Some(n) = self.failures.success() {
                    info!(failures = n, "database writes recovered");
                }
                if self.overflow.success().is_some() {
                    info!("line buffer has room again");
                }
            }
            PostResult::Refused(reason) => {
                error!(lines = batch.len(), %reason, "database refused batch, dropping it");
            }
            PostResult::Failed(reason) => {
                if let Some(n) = self.failures.failure() {
                    warn!(%reason, failures = n, buffered = self.buffer.len() + batch.len(), "database write failed, will retry");
                }
                // Put the batch back in front, keeping the newest data if the
                // buffer filled up meanwhile.
                let room = self.cfg.buffer_lines.saturating_sub(self.buffer.len());
                let skip = batch.len().saturating_sub(room);
                for line in batch.into_iter().skip(skip).rev() {
                    self.buffer.push_front(line);
                }
            }
        }
    }
}

fn post(agent: &Agent, url: &str, token: Option<&str>, batch: &[String]) -> PostResult {
    let body = batch.join("\n");
    let mut request = agent
        .post(url)
        .header("Content-Type", "text/plain; charset=utf-8");
    if let Some(token) = token {
        request = request.header("Authorization", &format!("Token {token}"));
    }
    match request.send(body.as_bytes()) {
        Ok(mut response) => {
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                return PostResult::Written;
            }
            let detail = response.body_mut().read_to_string().unwrap_or_default();
            let reason = format!("HTTP {status}: {}", detail.trim());
            // 4xx other than rate limiting/timeouts means the payload itself is bad.
            if (400..500).contains(&status) && status != 408 && status != 429 {
                PostResult::Refused(reason)
            } else {
                PostResult::Failed(reason)
            }
        }
        Err(e) => PostResult::Failed(e.to_string()),
    }
}

/// One line per device poll: `<device_id> <key>=<value>,... <ns timestamp>`.
/// Unverified sensors are included on purpose; tagging them would split each
/// series in two the day the flag is flipped.
pub fn to_line(reading: &Reading) -> Option<String> {
    let mut line = escape_measurement(&reading.map.device.id);
    let mut first = true;
    for (sensor, value) in reading.values() {
        line.push(if first { ' ' } else { ',' });
        first = false;
        // Keys are restricted to [a-z0-9_] by the map loader: no escaping needed.
        let _ = write!(
            line,
            "{}={}",
            sensor.key,
            format_value(value, sensor.precision)
        );
    }
    if first {
        return None;
    }
    let ns = reading
        .timestamp
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let _ = write!(line, " {ns}");
    Some(line)
}

fn escape_measurement(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, ',' | ' ' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use registers::{DeviceMap, Outcome};

    use super::*;

    const MAP: &str = r#"
        [device]
        id = "deye"
        name = "Deye"
        blocks = [[586, 6]]

        [[register]]
        key = "battery_temperature"
        name = "T"
        address = 586
        type = "s16"
        scale = 0.1
        offset = -100

        [[register]]
        key = "battery_soc"
        name = "SOC"
        address = 588
        type = "u16"

        [[register]]
        key = "battery_power"
        name = "P"
        address = 590
        type = "s16"
    "#;

    fn reading(outcomes: Vec<Outcome>) -> Reading {
        Reading {
            map: Arc::new(DeviceMap::from_toml(MAP).unwrap()),
            bus: "inverter".into(),
            slave: 2,
            timestamp: UNIX_EPOCH + Duration::from_millis(1_758_830_400_123),
            blocks: vec![],
            outcomes,
        }
    }

    #[test]
    fn formats_line() {
        let r = reading(vec![
            Outcome::Value(25.0 + 1e-12),
            Outcome::Value(42.0),
            Outcome::Value(-10.0),
        ]);
        assert_eq!(
            to_line(&r).unwrap(),
            "deye battery_temperature=25,battery_soc=42,battery_power=-10 1758830400123000000"
        );
    }

    #[test]
    fn skips_missing_values() {
        let r = reading(vec![
            Outcome::Unavailable,
            Outcome::Value(42.0),
            Outcome::Unavailable,
        ]);
        assert_eq!(
            to_line(&r).unwrap(),
            "deye battery_soc=42 1758830400123000000"
        );
        let r = reading(vec![Outcome::Unavailable; 3]);
        assert_eq!(to_line(&r), None);
    }

    #[test]
    fn escapes_measurement() {
        assert_eq!(escape_measurement("a b,c"), "a\\ b\\,c");
    }

    #[test]
    fn failed_batch_goes_back_in_order() {
        let cfg = InfluxConfig {
            buffer_lines: 5,
            ..InfluxConfig::default()
        };
        let mut sink = InfluxSink::new(cfg);
        for i in 0..3 {
            sink.push(format!("l{i}"));
        }
        let batch: Batch = sink.buffer.drain(..2).collect();
        for i in 3..6 {
            sink.push(format!("l{i}"));
        }
        // Buffer now holds l2..l5 (4 lines) and has room for one more.
        sink.handle(Ok((batch, PostResult::Failed("down".into()))));
        let lines: Vec<&str> = sink.buffer.iter().map(String::as_str).collect();
        assert_eq!(lines, ["l1", "l2", "l3", "l4", "l5"]);
    }
}
