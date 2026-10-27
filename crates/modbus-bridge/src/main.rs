//! Read-only Modbus RTU bridge: polls each serial bus in its own task and
//! hands decoded readings to MQTT (Home Assistant discovery) and InfluxDB
//! line protocol sinks.

mod bus;
mod cli;
mod config;
mod reading;
mod rtu;
mod sinks;
mod throttle;
mod usb_reset;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{Level, error, info, warn};

use crate::bus::Bus;
use crate::cli::Command;
use crate::config::Setup;
use crate::reading::Fanout;
use crate::sinks::influx::InfluxSink;
use crate::sinks::mqtt::MqttSink;

/// Readings buffered per sink before new ones are dropped.
const SINK_QUEUE: usize = 64;

fn main() -> ExitCode {
    let args = match cli::parse() {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{}", cli::usage());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("modbus-bridge {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", cli::usage());
            return ExitCode::from(2);
        }
    };
    init_logging();

    let result = config::load(&args.config).and_then(|setup| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            if args.once {
                Ok(poll_once(setup).await)
            } else {
                serve(setup, args.dry_run).await.map(|()| ExitCode::SUCCESS)
            }
        });
        // Bounded by the HTTP request timeout anyway; don't hang on exit.
        runtime.shutdown_timeout(Duration::from_secs(2));
        result
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    let level = std::env::var("WATTLEDGER_LOG")
        .ok()
        .and_then(|s| s.parse::<Level>().ok())
        .unwrap_or(Level::INFO);
    // stdout is reserved for --dry-run output.
    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_target(false);
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        // journald adds its own timestamps.
        builder.without_time().with_ansi(false).init();
    } else {
        builder.with_ansi(std::io::stderr().is_terminal()).init();
    }
}

/// `--once --dry-run`: one poll cycle per bus, printed, then exit.
async fn poll_once(setup: Setup) -> ExitCode {
    let tasks: Vec<_> = setup
        .buses
        .into_iter()
        .map(|b| {
            let bus = Bus::new(b);
            (bus.name().to_owned(), tokio::spawn(bus.poll_once()))
        })
        .collect();

    let mut all_ok = true;
    for (name, task) in tasks {
        match task.await {
            Ok(Ok((readings, fault))) => {
                for reading in &readings {
                    sinks::stdout::print(reading);
                }
                all_ok &= readings.iter().all(|r| r.blocks.iter().all(Result::is_ok));
                if let Some(fault) = fault {
                    all_ok = false;
                    error!(bus = %name, "poll cycle aborted: {fault}");
                }
            }
            Ok(Err(e)) => {
                all_ok = false;
                error!("{e:#}");
            }
            Err(e) => {
                all_ok = false;
                error!(bus = %name, "poll task failed: {e}");
            }
        }
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn serve(setup: Setup, dry_run: bool) -> anyhow::Result<()> {
    let Setup {
        buses,
        mqtt,
        influx,
    } = setup;

    let mut fanout = Fanout::new();
    let mut sink_tasks = Vec::new();
    if dry_run {
        let rx = fanout.add("stdout", SINK_QUEUE);
        sink_tasks.push(tokio::spawn(sinks::stdout::run(rx)));
    } else {
        if mqtt.enabled {
            let devices = buses
                .iter()
                .flat_map(|b| {
                    b.devices
                        .iter()
                        .map(|d| (Arc::clone(&d.map), d.mqtt_interval))
                })
                .collect();
            let rx = fanout.add("mqtt", SINK_QUEUE);
            sink_tasks.push(tokio::spawn(MqttSink::new(mqtt, devices).run(rx)));
        }
        if influx.enabled {
            let rx = fanout.add("influx", SINK_QUEUE);
            sink_tasks.push(tokio::spawn(InfluxSink::new(influx).run(rx)));
        }
        if fanout.is_empty() {
            warn!("neither MQTT nor InfluxDB is enabled; readings are decoded and discarded");
        }
    }

    let fanout = Arc::new(fanout);
    let (stop_tx, stop_rx) = watch::channel(false);
    let bus_count = buses.len();
    let bus_tasks: Vec<_> = buses
        .into_iter()
        .map(|b| tokio::spawn(Bus::new(b).run(Arc::clone(&fanout), stop_rx.clone())))
        .collect();
    // From here the bus tasks hold the only references, so the sinks see
    // their channels close once every bus has stopped.
    drop(fanout);
    info!(buses = bus_count, dry_run, "bridge running");

    wait_for_signal().await;
    info!("shutting down");
    let _ = stop_tx.send(true);
    for task in bus_tasks {
        let _ = task.await;
    }
    // Each sink bounds its own shutdown work (offline message, final flush).
    for task in sink_tasks {
        let _ = task.await;
    }
    info!("stopped");
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(e) => {
            warn!(error = %e, "cannot listen for SIGTERM, only Ctrl-C will stop the bridge");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
