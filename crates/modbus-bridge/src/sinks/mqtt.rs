use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use registers::{DeviceMap, format_value};
use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, EventLoop, LastWill, MqttOptions, Outgoing, Packet, QoS,
};
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tracing::{debug, info, warn};

use super::{discovery, domoticz_p1};
use crate::config::MqttConfig;
use crate::reading::Reading;
use crate::throttle::ErrorLog;

/// Requests buffered inside rumqttc; beyond this, publishes are dropped.
const REQUEST_CAPACITY: usize = 256;
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Readings arrive on the poll grid; without slack a 10 s interval with 2 s
/// polling would publish every 12 s.
const INTERVAL_SLACK: Duration = Duration::from_millis(500);

const ONLINE: &str = "online";
const OFFLINE: &str = "offline";

#[derive(Debug)]
enum ConnEvent {
    Connected,
    Disconnected,
    /// Home Assistant or Domoticz (re)started and wants discovery again.
    HomeAssistantOnline,
}

pub struct MqttSink {
    cfg: MqttConfig,
    devices: Vec<(Arc<DeviceMap>, Duration)>,
}

impl MqttSink {
    /// `devices` are all polled devices with their minimum publish interval.
    pub fn new(cfg: MqttConfig, devices: Vec<(Arc<DeviceMap>, Duration)>) -> Self {
        Self { cfg, devices }
    }

    /// Runs until `readings` is closed, then marks the bridge offline.
    pub async fn run(self, mut readings: mpsc::Receiver<Arc<Reading>>) {
        let availability = discovery::availability_topic(&self.cfg);
        let mut options = MqttOptions::new(&self.cfg.client_id, &self.cfg.host, self.cfg.port);
        options.set_keep_alive(self.cfg.keep_alive);
        options.set_clean_session(true);
        options.set_last_will(LastWill::new(
            &availability,
            OFFLINE,
            QoS::AtLeastOnce,
            true,
        ));
        if let Some(user) = &self.cfg.username {
            options.set_credentials(user, self.cfg.password.as_deref().unwrap_or(""));
        }

        if let Some(p1) = &self.cfg.domoticz_p1 {
            info!(idx = p1.idx, device = %p1.device, topic = %p1.topic, "feeding a Domoticz P1 meter");
        }
        let (client, eventloop) = AsyncClient::new(options, REQUEST_CAPACITY);
        let (events_tx, mut events) = mpsc::channel(8);
        let driver = tokio::spawn(drive(
            eventloop,
            events_tx,
            discovery::birth_topic(&self.cfg),
        ));

        let intervals: HashMap<&str, Duration> = self
            .devices
            .iter()
            .map(|(map, interval)| (map.device.id.as_str(), *interval))
            .collect();
        let mut last_sent: HashMap<String, Instant> = HashMap::new();
        let mut connected = false;
        let mut drops = ErrorLog::new();

        loop {
            tokio::select! {
                reading = readings.recv() => {
                    let Some(reading) = reading else { break };
                    if !connected {
                        continue;
                    }
                    let id = reading.map.device.id.as_str();
                    let interval = intervals.get(id).copied().unwrap_or(self.cfg.publish_interval);
                    let due = last_sent
                        .get(id)
                        .is_none_or(|t| t.elapsed() + INTERVAL_SLACK >= interval);
                    if due {
                        last_sent.insert(id.to_owned(), Instant::now());
                        self.publish_states(&client, &reading, &mut drops);
                        self.publish_p1(&client, &reading, &mut drops);
                    }
                }
                event = events.recv() => match event {
                    Some(ConnEvent::Connected) => {
                        info!(host = %self.cfg.host, port = self.cfg.port, "MQTT connected");
                        connected = true;
                        // Domoticz creates a device only once a state arrives,
                        // so send states right after (re)announcing.
                        last_sent.clear();
                        self.announce(&client, &availability, &mut drops);
                    }
                    Some(ConnEvent::HomeAssistantOnline) => {
                        if connected {
                            info!("discovery consumer came online, republishing discovery");
                            self.publish_discovery(&client, &mut drops);
                            last_sent.clear();
                        }
                    }
                    Some(ConnEvent::Disconnected) => connected = false,
                    None => break,
                },
            }
        }

        if connected {
            let goodbye = async {
                client
                    .publish(&availability, QoS::AtLeastOnce, true, OFFLINE)
                    .await?;
                client.disconnect().await
            };
            if let Err(e) = time::timeout(SHUTDOWN_TIMEOUT, goodbye)
                .await
                .unwrap_or(Ok(()))
            {
                warn!(error = %e, "could not publish offline status");
            }
        }
        match time::timeout(SHUTDOWN_TIMEOUT, driver).await {
            Ok(_) => info!("MQTT disconnected"),
            // The LWT covers this case: the broker publishes "offline" for us.
            Err(_) => warn!("MQTT did not close cleanly in time"),
        }
    }

    fn announce(&self, client: &AsyncClient, availability: &str, drops: &mut ErrorLog) {
        let birth = discovery::birth_topic(&self.cfg);
        if let Err(e) = client.try_subscribe(&birth, QoS::AtLeastOnce) {
            warn!(topic = %birth, error = %e, "cannot subscribe");
        }
        self.publish_discovery(client, drops);
        try_publish(
            client,
            availability,
            QoS::AtLeastOnce,
            true,
            ONLINE.into(),
            drops,
        );
    }

    fn publish_discovery(&self, client: &AsyncClient, drops: &mut ErrorLog) {
        let mut count = 0;
        for (map, _) in &self.devices {
            for sensor in map
                .sensors
                .iter()
                .filter(|s| discovery::is_published(&self.cfg, s))
            {
                let topic = discovery::config_topic(&self.cfg, map, sensor);
                let payload = discovery::config_payload(&self.cfg, map, sensor);
                try_publish(client, &topic, QoS::AtLeastOnce, true, payload, drops);
                count += 1;
            }
        }
        debug!(sensors = count, "discovery published");
    }

    fn publish_p1(&self, client: &AsyncClient, reading: &Reading, drops: &mut ErrorLog) {
        let Some(p1) = &self.cfg.domoticz_p1 else {
            return;
        };
        if let Some(payload) = domoticz_p1::payload(p1, reading) {
            try_publish(client, &p1.topic, QoS::AtMostOnce, false, payload, drops);
        }
    }

    fn publish_states(&self, client: &AsyncClient, reading: &Reading, drops: &mut ErrorLog) {
        for (sensor, value) in reading.values() {
            if !discovery::is_published(&self.cfg, sensor) {
                continue;
            }
            let topic = discovery::state_topic(&self.cfg, &reading.map, sensor);
            let payload = format_value(value, sensor.precision);
            try_publish(client, &topic, QoS::AtMostOnce, false, payload, drops);
        }
    }
}

/// Never waits: when rumqttc's queue is full the message is dropped.
fn try_publish(
    client: &AsyncClient,
    topic: &str,
    qos: QoS,
    retain: bool,
    payload: String,
    drops: &mut ErrorLog,
) {
    match client.try_publish(topic, qos, retain, payload) {
        Ok(()) => {
            drops.success();
        }
        Err(e) => {
            if let Some(n) = drops.failure() {
                warn!(error = %e, dropped = n, "MQTT queue full, dropping messages");
            }
        }
    }
}

/// Polls the rumqttc event loop, which also performs reconnects, and reports
/// connection changes. Ends after our own DISCONNECT has been sent.
async fn drive(mut eventloop: EventLoop, events: mpsc::Sender<ConnEvent>, birth_topic: String) {
    let mut backoff = RECONNECT_BACKOFF_MIN;
    let mut errors = ErrorLog::new();
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) if ack.code == ConnectReturnCode::Success => {
                backoff = RECONNECT_BACKOFF_MIN;
                errors.success();
                let _ = events.send(ConnEvent::Connected).await;
            }
            Ok(Event::Incoming(Packet::Publish(p)))
                if p.topic == birth_topic && p.payload.as_ref() == ONLINE.as_bytes() =>
            {
                let _ = events.send(ConnEvent::HomeAssistantOnline).await;
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => return,
            Ok(_) => {}
            Err(e) => {
                let _ = events.send(ConnEvent::Disconnected).await;
                if let Some(n) = errors.failure() {
                    warn!(error = %e, failures = n, retry_in = ?backoff, "MQTT connection failed");
                }
                time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
            }
        }
    }
}
