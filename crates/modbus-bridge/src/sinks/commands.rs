//! Write commands over MQTT: topics and payload decoding.
//!
//! Commands arrive on `<prefix>/command/<device>` as JSON (see
//! [`crate::write::Command`]); the answer goes to
//! `<prefix>/command/<device>/result`.

use crate::write::{Command, CommandResult};

pub fn command_topic(prefix: &str, device: &str) -> String {
    format!("{prefix}/command/{device}")
}

pub fn result_topic(prefix: &str, device: &str) -> String {
    format!("{prefix}/command/{device}/result")
}

/// The device a command topic addresses, or `None` for any other topic.
pub fn device_of<'a>(prefix: &str, topic: &'a str) -> Option<&'a str> {
    let device = topic.strip_prefix(prefix)?.strip_prefix("/command/")?;
    (!device.is_empty() && !device.contains('/')).then_some(device)
}

/// Decodes a command. On failure returns the command id (if one could be
/// found, so the sender still gets an answer) and the reason.
pub fn decode(payload: &[u8]) -> Result<Command, (Option<String>, String)> {
    serde_json::from_slice(payload).map_err(|e| {
        let id = serde_json::from_slice::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(str::to_owned));
        (id, format!("invalid command: {e}"))
    })
}

pub fn error_result(id: String, error: String) -> CommandResult {
    CommandResult {
        id,
        ok: false,
        error: Some(error),
        ..CommandResult::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics() {
        assert_eq!(
            command_topic("wattledger", "deye"),
            "wattledger/command/deye"
        );
        assert_eq!(
            result_topic("wattledger", "deye"),
            "wattledger/command/deye/result"
        );
        assert_eq!(
            device_of("wattledger", "wattledger/command/deye"),
            Some("deye")
        );
        assert_eq!(
            device_of("wattledger", "wattledger/command/deye/result"),
            None
        );
        assert_eq!(device_of("wattledger", "wattledger/deye/battery_soc"), None);
        assert_eq!(device_of("wattledger", "other/command/deye"), None);
        assert_eq!(device_of("wattledger", "wattledger/command/"), None);
    }

    #[test]
    fn decodes_commands() {
        let c = decode(br#"{"id":"a1","expires":1800000000,"write":[["work_mode",0],["tou_soc_1",35]],"read":["max_sell_power"]}"#).unwrap();
        assert_eq!(c.id, "a1");
        assert_eq!(c.expires, 1_800_000_000);
        assert_eq!(
            c.write,
            [
                ("work_mode".to_owned(), 0.0),
                ("tou_soc_1".to_owned(), 35.0)
            ]
        );
        assert_eq!(c.read, ["max_sell_power"]);

        let read_only = decode(br#"{"id":"b","expires":1,"read":["work_mode"]}"#).unwrap();
        assert!(read_only.write.is_empty());
    }

    #[test]
    fn bad_commands_keep_their_id_when_possible() {
        let (id, err) = decode(br#"{"id":"x","expires":"soon"}"#).unwrap_err();
        assert_eq!(id.as_deref(), Some("x"));
        assert!(!err.is_empty());

        let (id, _) = decode(b"not json").unwrap_err();
        assert_eq!(id, None);

        let (id, err) = decode(br#"{"id":"y","expires":1,"wirte":[]}"#).unwrap_err();
        assert_eq!(id.as_deref(), Some("y"));
        assert!(err.contains("wirte"), "{err}");
    }

    #[test]
    fn error_results_are_not_ok() {
        let r = error_result("z".into(), "busy".into());
        assert_eq!(r.id, "z");
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("busy"));
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "busy");
    }
}
