//! wattledger web server: live energy flow, history charts, RCE prices and
//! battery export plans carried out through the bridge.

mod config;
mod control;
mod db;
mod energy;
mod live;
mod prices;
mod projection;
mod tasks;
mod time;
mod vm;
mod web;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rumqttc::{AsyncClient, MqttOptions};
use tokio::sync::watch;
use tracing::{Level, error, info, warn};

use crate::control::bridge::MqttInverter;
use crate::live::LiveState;

const DEFAULT_CONFIG: &str = "/etc/wattledger/web.toml";
const VM_TIMEOUT: Duration = Duration::from_secs(30);

fn usage() -> &'static str {
    "usage: wattledger-web [-c|--config PATH]\n\n\
     Serves the wattledger pages, fetches RCE prices and runs export plans.\n\
     PATH defaults to /etc/wattledger/web.toml."
}

fn parse_args() -> Result<Option<PathBuf>, lexopt::Error> {
    use lexopt::prelude::*;
    let mut config = PathBuf::from(DEFAULT_CONFIG);
    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Short('c') | Long("config") => config = parser.value()?.into(),
            Short('h') | Long("help") => return Ok(None),
            Short('V') | Long("version") => {
                println!("wattledger-web {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(Some(config))
}

fn main() -> ExitCode {
    let path = match parse_args() {
        Ok(Some(p)) => p,
        Ok(None) => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    init_logging();
    let result = config::load(&path).and_then(|cfg| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(serve(cfg))
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
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
    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_target(false);
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().with_ansi(false).init();
    } else {
        builder.with_ansi(std::io::stderr().is_terminal()).init();
    }
}

async fn serve(cfg: config::Config) -> anyhow::Result<()> {
    let pool = db::connect(&cfg.database_url).await?;

    let mut options = MqttOptions::new(&cfg.mqtt.client_id, &cfg.mqtt.host, cfg.mqtt.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_clean_session(true);
    if let Some(user) = &cfg.mqtt.username {
        options.set_credentials(user, cfg.mqtt.password.as_deref().unwrap_or(""));
    }
    let (client, eventloop) = AsyncClient::new(options, 64);
    let inverter = Arc::new(MqttInverter::new(
        client.clone(),
        &cfg.mqtt.topic_prefix,
        &cfg.mqtt.inverter,
        cfg.mqtt.command_timeout,
    ));
    let live = Arc::new(RwLock::new(LiveState::default()));

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(tasks::mqtt_loop(
        eventloop,
        client.clone(),
        cfg.mqtt.clone(),
        Arc::clone(&live),
        Arc::clone(&inverter),
    ));
    let prices = tokio::spawn(tasks::price_loop(
        pool.clone(),
        cfg.prices.clone(),
        stop_rx.clone(),
    ));
    let control = tokio::spawn(tasks::control_loop(
        pool.clone(),
        Arc::clone(&inverter),
        Arc::clone(&live),
        cfg.control.interval,
        stop_rx.clone(),
    ));

    let state = web::AppState {
        pool,
        live,
        history: Arc::new(vm::Vm::new(&cfg.victoria_url, VM_TIMEOUT)),
        inverter,
        now: Arc::new(chrono::Utc::now),
        today: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    info!(listen = %cfg.listen, "web server running");
    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(wait_for_signal())
        .await?;

    info!("shutting down");
    let _ = stop_tx.send(true);
    // An export pass in progress finishes its current command first.
    let _ = tokio::time::timeout(Duration::from_secs(70), control).await;
    let _ = prices.await;
    let _ = client.disconnect().await;
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
            warn!(error = %e, "cannot listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
