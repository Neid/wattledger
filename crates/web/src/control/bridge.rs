//! Commands to the bridge over MQTT (`<prefix>/command/<device>`), matched
//! with their answers by id.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use rumqttc::{AsyncClient, QoS};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// The bridge's command format (`modbus-bridge/src/write.rs`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Command {
    pub id: String,
    pub expires: u64,
    pub write: Vec<(String, f64)>,
    pub read: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct CommandResult {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub before: BTreeMap<String, f64>,
    #[serde(default)]
    pub after: BTreeMap<String, f64>,
    #[serde(default)]
    pub written: Vec<String>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Something that runs commands on the inverter. Errors mean no answer
/// arrived: the command may or may not have been carried out.
pub trait Inverter: Send + Sync {
    fn send(&self, cmd: Command) -> BoxFuture<'_, anyhow::Result<CommandResult>>;
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<CommandResult>>>>;

/// Sends commands through the web server's MQTT connection.
#[derive(Clone)]
pub struct MqttInverter {
    client: AsyncClient,
    topic: String,
    pending: Pending,
    timeout: Duration,
}

impl MqttInverter {
    pub fn new(client: AsyncClient, prefix: &str, device: &str, timeout: Duration) -> Self {
        Self {
            client,
            topic: format!("{prefix}/command/{device}"),
            pending: Arc::default(),
            timeout,
        }
    }

    pub fn result_topic(&self) -> String {
        format!("{}/result", self.topic)
    }

    /// Hands an answer from the result topic to whoever waits for it.
    pub fn resolve(&self, payload: &[u8]) -> bool {
        resolve(&self.pending, payload)
    }
}

fn resolve(pending: &Pending, payload: &[u8]) -> bool {
    let Ok(result) = serde_json::from_slice::<CommandResult>(payload) else {
        return false;
    };
    let waiter = pending.lock().expect("not poisoned").remove(&result.id);
    waiter.is_some_and(|tx| tx.send(result).is_ok())
}

impl Inverter for MqttInverter {
    fn send(&self, cmd: Command) -> BoxFuture<'_, anyhow::Result<CommandResult>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            let id = cmd.id.clone();
            self.pending
                .lock()
                .expect("not poisoned")
                .insert(id.clone(), tx);
            let payload = serde_json::to_vec(&cmd)?;
            let sent = self
                .client
                .publish(&self.topic, QoS::AtLeastOnce, false, payload)
                .await
                .context("publishing the command");
            let result = match sent {
                Ok(()) => match tokio::time::timeout(self.timeout, rx).await {
                    Ok(Ok(result)) => Ok(result),
                    Ok(Err(_)) => Err(anyhow!("answer channel closed")),
                    Err(_) => Err(anyhow!(
                        "no answer from the bridge within {}s",
                        self.timeout.as_secs()
                    )),
                },
                Err(e) => Err(e),
            };
            self.pending.lock().expect("not poisoned").remove(&id);
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_matches_the_bridge_format() {
        let c = Command {
            id: "x".into(),
            expires: 5,
            write: vec![("work_mode".into(), 0.0)],
            read: vec!["max_sell_power".into()],
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"id":"x","expires":5,"write":[["work_mode",0.0]],"read":["max_sell_power"]}"#
        );
    }

    #[tokio::test]
    async fn answers_reach_their_waiter() {
        let pending: Pending = Arc::default();
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert("a".into(), tx);
        assert!(
            !resolve(&pending, br#"{"id":"b","ok":true}"#),
            "nobody waits for b"
        );
        assert!(!resolve(&pending, b"garbage"));
        assert!(resolve(
            &pending,
            br#"{"id":"a","ok":false,"dry_run":false,"error":"busy","before":{},"after":{},"written":[]}"#
        ));
        let r = rx.await.unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("busy"));
        assert!(pending.lock().unwrap().is_empty());
    }
}
