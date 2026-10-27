use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use registers::{Decoder, Outcome};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::time::{self, MissedTickBehavior};
use tracing::{info, warn};

use crate::config::{BusConfig, BusSetup};
use crate::reading::{Fanout, Reading};
use crate::rtu::{self, RtuError};
use crate::throttle::ErrorLog;
use crate::usb_reset::UsbReset;

const REOPEN_BACKOFF_MIN: Duration = Duration::from_secs(1);
const REOPEN_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// One serial port and the devices on it, polled strictly one request at a
/// time because the Pi is the only master on the line.
pub struct Bus {
    name: Arc<str>,
    config: BusConfig,
    devices: Vec<PolledDevice>,
    consecutive_timeouts: u32,
    health: ErrorLog,
}

struct PolledDevice {
    slave: u8,
    decoder: Decoder,
    block_logs: Vec<ErrorLog>,
    sensor_logs: Vec<ErrorLog>,
}

/// Reasons to close and reopen the port.
#[derive(Debug)]
enum BusFault {
    Io(io::Error),
    Timeouts(u32),
}

impl fmt::Display for BusFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "serial I/O error: {e}"),
            Self::Timeouts(n) => write!(f, "{n} consecutive timeouts"),
        }
    }
}

impl Bus {
    pub fn new(setup: BusSetup) -> Self {
        let devices = setup
            .devices
            .into_iter()
            .map(|d| PolledDevice {
                slave: d.slave,
                block_logs: d.map.blocks.iter().map(|_| ErrorLog::new()).collect(),
                sensor_logs: d.map.sensors.iter().map(|_| ErrorLog::new()).collect(),
                decoder: Decoder::new(d.map),
            })
            .collect();
        Self {
            name: setup.config.name.as_str().into(),
            config: setup.config,
            devices,
            consecutive_timeouts: 0,
            health: ErrorLog::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn open(&self) -> io::Result<rtu::SerialClient> {
        rtu::open_serial(
            &self.config.port,
            self.config.baud,
            self.config.request_timeout,
            self.config.inter_request_gap,
        )
    }

    /// Polls until shutdown, reopening the port with backoff on failure.
    pub async fn run(mut self, fanout: Arc<Fanout>, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = REOPEN_BACKOFF_MIN;
        let mut usb_reset = self.usb_reset();
        loop {
            let (fault, port_was_open) = match self.open() {
                Ok(mut client) => {
                    info!(bus = %self.name, port = %self.config.port, "serial port opened");
                    self.consecutive_timeouts = 0;
                    let mut ticker = time::interval(self.config.poll_interval);
                    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                    loop {
                        let cycle = tokio::select! {
                            () = stopped(&mut shutdown) => return,
                            cycle = async {
                                ticker.tick().await;
                                self.poll_cycle(&mut client).await
                            } => cycle,
                        };
                        let (readings, fault) = cycle;
                        if readings.iter().any(|r| r.blocks.iter().any(Result::is_ok)) {
                            backoff = REOPEN_BACKOFF_MIN;
                            if let Some(reset) = &mut usb_reset {
                                reset.alive(std::time::Instant::now());
                            }
                            if let Some(n) = self.health.success() {
                                info!(bus = %self.name, failures = n, "bus recovered");
                            }
                        }
                        for reading in readings {
                            fanout.send(reading);
                        }
                        if let Some(fault) = fault {
                            break (fault.to_string(), true);
                        }
                    }
                }
                Err(e) => (format!("cannot open {}: {e}", self.config.port), false),
            };
            // A missing adapter cannot be reset; one that opens but stays
            // silent may be hung.
            if port_was_open
                && let Some(reset) = &mut usb_reset
                && let Some(silent) = reset.due(std::time::Instant::now())
            {
                match reset.request(&self.config.port) {
                    Ok(()) => {
                        warn!(bus = %self.name, port = %self.config.port, silent = %humantime::format_duration(round_secs(silent)), "no answer for too long, resetting the USB adapter");
                        backoff = REOPEN_BACKOFF_MIN;
                    }
                    Err(e) => {
                        warn!(bus = %self.name, error = %e, "cannot request a USB adapter reset");
                    }
                }
            }
            if let Some(n) = self.health.failure() {
                warn!(bus = %self.name, error = %fault, failures = n, retry_in = ?backoff, "bus failure, reopening port");
            }
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                () = time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(REOPEN_BACKOFF_MAX);
        }
    }

    fn usb_reset(&self) -> Option<UsbReset> {
        let after = self.config.usb_reset_after?;
        let Some(dir) = UsbReset::runtime_dir() else {
            warn!(bus = %self.name, "usb_reset_after is set, but RUNTIME_DIRECTORY is not (not started by systemd?); USB resets are off");
            return None;
        };
        info!(bus = %self.name, after = %humantime::format_duration(after), "USB adapter reset enabled");
        Some(UsbReset::new(dir, &self.name, after, std::time::Instant::now()))
    }

    /// Opens the port, polls every device once and closes it again.
    /// Returns the readings and, if the cycle was cut short, why.
    pub async fn poll_once(mut self) -> anyhow::Result<(Vec<Reading>, Option<String>)> {
        let mut client = self
            .open()
            .with_context(|| format!("bus {}: opening {}", self.name, self.config.port))?;
        let (readings, fault) = self.poll_cycle(&mut client).await;
        Ok((readings, fault.map(|f| f.to_string())))
    }

    async fn poll_cycle<T>(
        &mut self,
        client: &mut rtu::Client<T>,
    ) -> (Vec<Reading>, Option<BusFault>)
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let mut readings = Vec::with_capacity(self.devices.len());
        for dev in &mut self.devices {
            let map = Arc::clone(dev.decoder.map());
            let mut blocks = Vec::with_capacity(map.blocks.len());
            for (block, log) in map.blocks.iter().zip(&mut dev.block_logs) {
                match client
                    .read_holding_registers(dev.slave, block.start, block.count)
                    .await
                {
                    Ok(words) => {
                        self.consecutive_timeouts = 0;
                        if let Some(n) = log.success() {
                            info!(bus = %self.name, device = %map.device.id, block = block.start, failures = n, "block readable again");
                        }
                        blocks.push(Ok(words));
                    }
                    Err(RtuError::Io(e)) => return (readings, Some(BusFault::Io(e))),
                    Err(e) => {
                        if matches!(e, RtuError::Timeout) {
                            self.consecutive_timeouts += 1;
                        }
                        if let Some(n) = log.failure() {
                            warn!(
                                bus = %self.name,
                                device = %map.device.id,
                                slave = dev.slave,
                                block = block.start,
                                count = block.count,
                                error = %e,
                                failures = n,
                                "block read failed"
                            );
                        }
                        blocks.push(Err(e.to_string()));
                        if self.consecutive_timeouts >= self.config.reopen_after_timeouts {
                            return (
                                readings,
                                Some(BusFault::Timeouts(self.consecutive_timeouts)),
                            );
                        }
                    }
                }
            }

            let words: Vec<Option<&[u16]>> = blocks
                .iter()
                .map(|b| b.as_ref().ok().map(Vec::as_slice))
                .collect();
            let outcomes = dev.decoder.decode(&words);
            for ((sensor, outcome), log) in
                map.sensors.iter().zip(&outcomes).zip(&mut dev.sensor_logs)
            {
                match outcome {
                    Outcome::Rejected(reason) => {
                        if let Some(n) = log.failure() {
                            warn!(device = %map.device.id, sensor = %sensor.key, %reason, rejections = n, "implausible value rejected");
                        }
                    }
                    Outcome::Value(_) => {
                        if let Some(n) = log.success() {
                            info!(device = %map.device.id, sensor = %sensor.key, rejections = n, "value plausible again");
                        }
                    }
                    Outcome::Unavailable => {}
                }
            }

            readings.push(Reading {
                map,
                bus: Arc::clone(&self.name),
                slave: dev.slave,
                timestamp: SystemTime::now(),
                blocks,
                outcomes,
            });
        }
        (readings, None)
    }
}

fn round_secs(d: Duration) -> Duration {
    Duration::from_secs(d.as_secs())
}

/// Resolves once shutdown is requested (or the sender is gone).
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|&stop| stop).await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use registers::DeviceMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

    use super::*;
    use crate::config::DeviceSetup;

    const MAP: &str = r#"
        [device]
        id = "meter"
        name = "Test meter"
        blocks = [[100, 2], [200, 1]]

        [[register]]
        key = "soc"
        name = "SOC"
        address = 101
        type = "u16"
        range = [0, 100]

        [[register]]
        key = "power"
        name = "Power"
        address = 200
        type = "s16"
    "#;

    fn bus(reopen_after_timeouts: u32) -> Bus {
        let map = Arc::new(DeviceMap::from_toml(MAP).unwrap());
        Bus::new(BusSetup {
            config: BusConfig {
                name: "test".into(),
                port: "/dev/null".into(),
                baud: 9600,
                poll_interval: Duration::from_secs(1),
                request_timeout: Duration::from_millis(100),
                inter_request_gap: Duration::ZERO,
                reopen_after_timeouts,
                usb_reset_after: None,
                devices: Vec::new(),
            },
            devices: vec![DeviceSetup {
                slave: 3,
                map,
                mqtt_interval: Duration::from_secs(10),
            }],
        })
    }

    /// Answers reads from `registers`; addresses it does not know get
    /// exception 2, except `silent`, which gets no answer at all.
    async fn fake_device(mut io: DuplexStream, registers: HashMap<u16, u16>, silent: Option<u16>) {
        let mut req = [0u8; 8];
        while io.read_exact(&mut req).await.is_ok() {
            let start = u16::from_be_bytes([req[2], req[3]]);
            let count = u16::from_be_bytes([req[4], req[5]]);
            if Some(start) == silent {
                continue;
            }
            let words: Option<Vec<u16>> = (start..start + count)
                .map(|a| registers.get(&a).copied())
                .collect();
            let body = match words {
                Some(words) => {
                    let mut body = vec![req[0], 0x03, (count * 2) as u8];
                    body.extend(words.iter().flat_map(|w| w.to_be_bytes()));
                    body
                }
                None => vec![req[0], 0x83, 0x02],
            };
            let frame = crate::rtu::tests_support::with_crc(&body);
            if io.write_all(&frame).await.is_err() {
                break;
            }
        }
    }

    fn client(io: DuplexStream) -> rtu::Client<DuplexStream> {
        rtu::Client::new(io, Duration::from_millis(100), Duration::ZERO)
    }

    #[tokio::test]
    async fn polls_all_blocks_and_rejects_implausible() {
        let (master, slave) = duplex(256);
        let regs = HashMap::from([(100, 0), (101, 150), (200, 0xFFF6)]);
        tokio::spawn(fake_device(slave, regs, None));

        let mut bus = bus(5);
        let (readings, fault) = bus.poll_cycle(&mut client(master)).await;
        assert!(fault.is_none());
        let r = &readings[0];
        assert!(r.blocks.iter().all(Result::is_ok));
        assert!(
            matches!(r.outcomes[0], Outcome::Rejected(_)),
            "SOC 150 must be rejected"
        );
        assert_eq!(r.outcomes[1], Outcome::Value(-10.0));
        assert_eq!(r.values().count(), 1);
    }

    #[tokio::test]
    async fn failed_block_does_not_hide_others() {
        let (master, slave) = duplex(256);
        let regs = HashMap::from([(200, 5)]);
        tokio::spawn(fake_device(slave, regs, None));

        let mut bus = bus(5);
        let (readings, fault) = bus.poll_cycle(&mut client(master)).await;
        assert!(fault.is_none());
        let r = &readings[0];
        assert!(
            r.blocks[0]
                .as_ref()
                .unwrap_err()
                .contains("illegal data address")
        );
        assert_eq!(r.outcomes[0], Outcome::Unavailable);
        assert_eq!(r.outcomes[1], Outcome::Value(5.0));
    }

    #[tokio::test]
    async fn one_silent_block_is_not_a_bus_fault() {
        let (master, slave) = duplex(256);
        let regs = HashMap::from([(200, 5)]);
        tokio::spawn(fake_device(slave, regs, Some(100)));

        let mut bus = bus(2);
        let mut client = client(master);
        for _ in 0..3 {
            let (readings, fault) = bus.poll_cycle(&mut client).await;
            assert!(fault.is_none(), "the device still answers other blocks");
            assert_eq!(readings[0].outcomes[1], Outcome::Value(5.0));
        }
    }

    #[tokio::test]
    async fn silent_device_faults_the_bus() {
        let (master, mut slave) = duplex(256);
        tokio::spawn(async move {
            let mut sink = [0u8; 64];
            while matches!(slave.read(&mut sink).await, Ok(n) if n > 0) {}
        });

        let mut bus = bus(3);
        let mut client = client(master);
        let (_, fault) = bus.poll_cycle(&mut client).await;
        assert!(fault.is_none(), "two timeouts are tolerated");
        let (_, fault) = bus.poll_cycle(&mut client).await;
        assert!(matches!(fault, Some(BusFault::Timeouts(3))), "{fault:?}");
    }
}
