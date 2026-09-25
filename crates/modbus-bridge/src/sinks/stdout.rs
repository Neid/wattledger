//! Human-readable output for `--dry-run`, meant for comparing values with
//! the inverter display and with mbpoll.

use std::io::{self, Write};
use std::sync::Arc;

use registers::{Outcome, Source, format_value};
use tokio::sync::mpsc;

use crate::reading::Reading;

pub async fn run(mut readings: mpsc::Receiver<Arc<Reading>>) {
    while let Some(reading) = readings.recv().await {
        print(&reading);
    }
}

/// Write errors (e.g. a closed pipe) are ignored: output is best effort and
/// must never take the process down.
pub fn print(reading: &Reading) {
    let _ = io::stdout().lock().write_all(render(reading).as_bytes());
}

pub fn render(reading: &Reading) -> String {
    use std::fmt::Write as _;

    let map = &reading.map;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n== {} ({}) - slave {} on bus {} - {}",
        map.device.id,
        map.device.name,
        reading.slave,
        reading.bus,
        humantime::format_rfc3339_seconds(reading.timestamp),
    );
    for (block, result) in map.blocks.iter().zip(&reading.blocks) {
        let status = match result {
            Ok(_) => "ok".to_owned(),
            Err(e) => format!("FAILED: {e}"),
        };
        let _ = writeln!(
            out,
            "   block {:>5} x{:<3} {status}",
            block.start, block.count
        );
    }
    let _ = writeln!(
        out,
        "   {:<26} {:>12} {:<5} {:>12}  {:<30} STATUS",
        "KEY", "VALUE", "UNIT", "RAW", "SOURCE"
    );

    let words = reading.block_words();
    for (i, (sensor, outcome)) in map.sensors.iter().zip(&reading.outcomes).enumerate() {
        let raw = map
            .raw_value(i, &words)
            .map(|r| format_value(r, 3))
            .unwrap_or_else(|| "-".into());
        let source = match &sensor.source {
            Source::Register(src) => {
                format!("reg {} {:?}", src.address, src.data_type).to_lowercase()
            }
            Source::Derived { op, inputs } => {
                let names: Vec<&str> = inputs
                    .iter()
                    .map(|&j| map.sensors[j].key.as_str())
                    .collect();
                format!("{}({})", op.as_str(), names.join(","))
            }
        };
        let (value, status) = match outcome {
            Outcome::Value(v) => (format_value(*v, sensor.precision), String::new()),
            Outcome::Unavailable => ("-".into(), "unavailable".into()),
            Outcome::Rejected(r) => ("-".into(), format!("REJECTED: {r}")),
        };
        let mut flags = Vec::new();
        if !sensor.verified {
            flags.push("UNVERIFIED");
        }
        if !sensor.mqtt {
            flags.push("no-mqtt");
        }
        let status = [status, flags.join(" ")]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(
            out,
            "   {:<26} {:>12} {:<5} {:>12}  {:<30} {}",
            sensor.key,
            value,
            sensor.unit.as_deref().unwrap_or(""),
            raw,
            source,
            if status.is_empty() { "ok" } else { &status },
        );
    }
    out
}
