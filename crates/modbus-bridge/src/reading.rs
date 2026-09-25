use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use registers::{DeviceMap, Outcome, Sensor};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::warn;

use crate::throttle::ErrorLog;

/// The result of polling one device once.
#[derive(Debug)]
pub struct Reading {
    pub map: Arc<DeviceMap>,
    pub bus: Arc<str>,
    pub slave: u8,
    pub timestamp: SystemTime,
    /// Per block of `map.blocks`: the words read, or why the read failed.
    pub blocks: Vec<Result<Vec<u16>, String>>,
    /// Per sensor of `map.sensors`.
    pub outcomes: Vec<Outcome>,
}

impl Reading {
    /// Sensors with an accepted value.
    pub fn values(&self) -> impl Iterator<Item = (&Sensor, f64)> {
        self.map
            .sensors
            .iter()
            .zip(&self.outcomes)
            .filter_map(|(sensor, outcome)| outcome.value().map(|v| (sensor, v)))
    }

    pub fn block_words(&self) -> Vec<Option<&[u16]>> {
        self.blocks
            .iter()
            .map(|b| b.as_ref().ok().map(Vec::as_slice))
            .collect()
    }
}

/// Hands readings to every sink without ever waiting: a slow or dead sink
/// loses readings instead of delaying Modbus polling.
pub struct Fanout {
    sinks: Vec<SinkHandle>,
}

struct SinkHandle {
    name: &'static str,
    tx: mpsc::Sender<Arc<Reading>>,
    drops: Mutex<ErrorLog>,
}

impl Fanout {
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    /// Registers a sink and returns the receiving end for its task.
    pub fn add(&mut self, name: &'static str, capacity: usize) -> mpsc::Receiver<Arc<Reading>> {
        let (tx, rx) = mpsc::channel(capacity);
        self.sinks.push(SinkHandle {
            name,
            tx,
            drops: Mutex::new(ErrorLog::new()),
        });
        rx
    }

    pub fn send(&self, reading: Reading) {
        let reading = Arc::new(reading);
        for sink in &self.sinks {
            let result = sink.tx.try_send(Arc::clone(&reading));
            // A poisoned lock only means another thread panicked while
            // logging; the counter inside is still usable.
            let mut drops = sink.drops.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(()) => {
                    drops.success();
                }
                Err(TrySendError::Full(_)) => {
                    if let Some(n) = drops.failure() {
                        warn!(
                            sink = sink.name,
                            dropped = n,
                            "sink is not keeping up, dropping readings"
                        );
                    }
                }
                Err(TrySendError::Closed(_)) => {
                    if let Some(n) = drops.failure() {
                        warn!(
                            sink = sink.name,
                            dropped = n,
                            "sink has stopped, dropping readings"
                        );
                    }
                }
            }
        }
    }
}
