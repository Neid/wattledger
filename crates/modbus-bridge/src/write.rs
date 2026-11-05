//! Writes to `[[setting]]` registers on request.
//!
//! A command lists settings to write (in order) and settings to only read.
//! The bridge reads every named setting first, writes, then reads them all
//! back, and answers with both sets of values, so the caller can restore the
//! previous state later. This module is the only caller of
//! `rtu::Client::write_registers`.

use std::collections::BTreeMap;

use registers::{Block, DeviceMap, MAX_BLOCK_REGISTERS};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;

use crate::rtu::{self, RtuError};

/// Most writes accepted in one command.
pub const MAX_WRITES: usize = 64;
/// Registers between two settings that may be read along to save a request.
const MAX_READ_GAP: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Write commands are refused; read-only commands still work.
    #[default]
    Off,
    /// Settings are read, nothing is written; the answer shows what would
    /// have been written.
    DryRun,
    /// Verified settings are written.
    On,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// Echoed in the result so the caller can match it.
    pub id: String,
    /// Unix seconds after which the command is refused, so a message stuck in
    /// a queue is never acted on late.
    pub expires: u64,
    /// `[key, value]` pairs, written in this order.
    #[serde(default)]
    pub write: Vec<(String, f64)>,
    /// Settings to read without writing.
    #[serde(default)]
    pub read: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct CommandResult {
    pub id: String,
    pub ok: bool,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Values read before writing.
    pub before: BTreeMap<String, f64>,
    /// Values read after writing (dry run: the values that would be written,
    /// on top of `before`).
    pub after: BTreeMap<String, f64>,
    /// Keys actually written, in order.
    pub written: Vec<String>,
}

/// A command for one device, with the channel the answer goes back on.
#[derive(Debug)]
pub struct Job {
    pub device: String,
    pub command: Command,
    pub reply: oneshot::Sender<CommandResult>,
}

/// Validated writes: setting index and raw word, in command order.
type Writes = Vec<(usize, u16)>;

/// Checks a command against the map and the mode without touching the bus.
pub fn validate(
    map: &DeviceMap,
    mode: WriteMode,
    cmd: &Command,
    now_unix: u64,
) -> Result<Writes, String> {
    if now_unix > cmd.expires {
        return Err(format!("command {} expired", cmd.id));
    }
    if cmd.write.is_empty() && cmd.read.is_empty() {
        return Err("nothing to write or read".into());
    }
    if cmd.write.len() > MAX_WRITES {
        return Err(format!("more than {MAX_WRITES} writes in one command"));
    }
    if mode == WriteMode::Off && !cmd.write.is_empty() {
        return Err("writes are off ([writes] mode in bridge.toml)".into());
    }
    for key in &cmd.read {
        if map.setting_index(key).is_none() {
            return Err(format!("unknown setting {key:?}"));
        }
    }
    let mut writes = Vec::with_capacity(cmd.write.len());
    for (n, (key, value)) in cmd.write.iter().enumerate() {
        let Some(i) = map.setting_index(key) else {
            return Err(format!("unknown setting {key:?}"));
        };
        if cmd.write[..n].iter().any(|(k, _)| k == key) {
            return Err(format!("{key} is written twice"));
        }
        let setting = &map.settings[i];
        if mode == WriteMode::On && !setting.verified {
            return Err(format!(
                "{key} is not verified (verified = false in the device map)"
            ));
        }
        let raw = setting.encode(*value).map_err(|e| e.to_string())?;
        writes.push((i, raw));
    }
    Ok(writes)
}

/// Groups register addresses into as few read requests as is sensible.
pub fn read_blocks(addresses: &[u16]) -> Vec<Block> {
    let mut sorted = addresses.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut blocks: Vec<Block> = Vec::new();
    for a in sorted {
        if let Some(last) = blocks.last_mut() {
            let end = last.start + last.count - 1;
            if a - end - 1 <= MAX_READ_GAP && a - last.start < MAX_BLOCK_REGISTERS {
                last.count = a - last.start + 1;
                continue;
            }
        }
        blocks.push(Block { start: a, count: 1 });
    }
    blocks
}

/// Reads the given settings, grouped into as few requests as possible.
async fn read_settings<T>(
    client: &mut rtu::Client<T>,
    slave: u8,
    map: &DeviceMap,
    settings: &[usize],
) -> Result<BTreeMap<String, f64>, RtuError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let addresses: Vec<u16> = settings.iter().map(|&i| map.settings[i].address).collect();
    let mut words = BTreeMap::new();
    for block in read_blocks(&addresses) {
        let values = client
            .read_holding_registers(slave, block.start, block.count)
            .await?;
        for (a, w) in (block.start..).zip(values) {
            words.insert(a, w);
        }
    }
    Ok(settings
        .iter()
        .map(|&i| {
            let s = &map.settings[i];
            (s.key.clone(), s.decode(words[&s.address]))
        })
        .collect())
}

/// Runs one command on the bus. Never panics; failures end up in the result.
pub async fn execute<T>(
    client: &mut rtu::Client<T>,
    slave: u8,
    map: &DeviceMap,
    mode: WriteMode,
    cmd: &Command,
    now_unix: u64,
) -> CommandResult
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut result = CommandResult {
        id: cmd.id.clone(),
        dry_run: mode == WriteMode::DryRun,
        ..CommandResult::default()
    };
    let writes = match validate(map, mode, cmd, now_unix) {
        Ok(w) => w,
        Err(e) => {
            result.error = Some(e);
            return result;
        }
    };
    let mut keys: Vec<usize> = writes.iter().map(|&(i, _)| i).collect();
    for key in &cmd.read {
        let i = map.setting_index(key).expect("validated");
        if !keys.contains(&i) {
            keys.push(i);
        }
    }

    result.before = match read_settings(client, slave, map, &keys).await {
        Ok(v) => v,
        Err(e) => {
            result.error = Some(format!("reading settings before writing: {e}"));
            return result;
        }
    };

    if mode != WriteMode::On {
        result.after = result.before.clone();
        for &(i, raw) in &writes {
            let s = &map.settings[i];
            result.after.insert(s.key.clone(), s.decode(raw));
        }
        result.ok = true;
        return result;
    }

    let mut error = None;
    for &(i, raw) in &writes {
        let s = &map.settings[i];
        match client.write_registers(slave, s.address, &[raw]).await {
            Ok(()) => result.written.push(s.key.clone()),
            Err(e) => {
                error = Some(format!("writing {}: {e}", s.key));
                break;
            }
        }
    }

    match read_settings(client, slave, map, &keys).await {
        Ok(after) => {
            for &(i, raw) in writes.iter().take(result.written.len()) {
                let s = &map.settings[i];
                let wanted = s.decode(raw);
                let got = after[&s.key];
                if got != wanted && error.is_none() {
                    error = Some(format!("{}: wrote {wanted}, read back {got}", s.key));
                }
            }
            result.after = after;
        }
        Err(e) => {
            let msg = format!("reading settings back: {e}");
            error = Some(match error {
                Some(first) => format!("{first}; {msg}"),
                None => msg,
            });
        }
    }
    result.ok = error.is_none();
    result.error = error;
    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

    use super::*;
    use crate::rtu::tests_support::with_crc;

    const MAP: &str = r#"
        [device]
        id = "inv"
        name = "Test inverter"
        blocks = [[0, 1]]

        [[setting]]
        key = "work_mode"
        name = "Work mode"
        address = 142
        range = [0, 2]
        verified = true

        [[setting]]
        key = "max_sell_power"
        name = "Max sell power"
        address = 143
        scale = 10
        unit = "W"
        range = [0, 10000]
        verified = true

        [[setting]]
        key = "tou_soc_1"
        name = "Slot 1 SOC"
        address = 166
        range = [0, 100]
        verified = true

        [[setting]]
        key = "untested"
        name = "Not verified yet"
        address = 200
        range = [0, 1]
    "#;

    const NOW: u64 = 1_800_000_000;

    fn map() -> DeviceMap {
        DeviceMap::from_toml(MAP).unwrap()
    }

    fn cmd(write: &[(&str, f64)], read: &[&str]) -> Command {
        Command {
            id: "c1".into(),
            expires: NOW + 30,
            write: write.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
            read: read.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    #[derive(Default)]
    struct FakeState {
        registers: HashMap<u16, u16>,
        /// Addresses written, in order.
        writes: Vec<u16>,
        /// Writes to these addresses get exception 4.
        fail_writes: Vec<u16>,
        /// Writes to these addresses are acknowledged but ignored.
        ignore_writes: Vec<u16>,
    }

    /// A slave that answers FC3 and FC16 from a shared register table.
    async fn fake_device(mut io: DuplexStream, state: Arc<Mutex<FakeState>>) {
        loop {
            let mut head = [0u8; 2];
            if io.read_exact(&mut head).await.is_err() {
                return;
            }
            let reply = match head[1] {
                0x03 => {
                    let mut rest = [0u8; 6];
                    io.read_exact(&mut rest).await.unwrap();
                    let start = u16::from_be_bytes([rest[0], rest[1]]);
                    let count = u16::from_be_bytes([rest[2], rest[3]]);
                    let st = state.lock().unwrap();
                    let mut body = vec![head[0], 0x03, (count * 2) as u8];
                    for a in start..start + count {
                        body.extend(st.registers.get(&a).copied().unwrap_or(0).to_be_bytes());
                    }
                    body
                }
                0x10 => {
                    let mut hdr = [0u8; 5];
                    io.read_exact(&mut hdr).await.unwrap();
                    let start = u16::from_be_bytes([hdr[0], hdr[1]]);
                    let count = u16::from_be_bytes([hdr[2], hdr[3]]);
                    let mut data = vec![0u8; usize::from(hdr[4]) + 2];
                    io.read_exact(&mut data).await.unwrap();
                    let mut st = state.lock().unwrap();
                    if st.fail_writes.contains(&start) {
                        vec![head[0], 0x90, 0x04]
                    } else {
                        st.writes.push(start);
                        if !st.ignore_writes.contains(&start) {
                            for i in 0..count {
                                let w = u16::from_be_bytes([
                                    data[usize::from(i) * 2],
                                    data[usize::from(i) * 2 + 1],
                                ]);
                                st.registers.insert(start + i, w);
                            }
                        }
                        let mut body = vec![head[0], 0x10];
                        body.extend(start.to_be_bytes());
                        body.extend(count.to_be_bytes());
                        body
                    }
                }
                other => panic!("unexpected function {other:#x}"),
            };
            if io.write_all(&with_crc(&reply)).await.is_err() {
                return;
            }
        }
    }

    fn setup() -> (rtu::Client<DuplexStream>, Arc<Mutex<FakeState>>) {
        let (master, slave) = duplex(1024);
        let state = Arc::new(Mutex::new(FakeState::default()));
        {
            let mut st = state.lock().unwrap();
            st.registers.insert(142, 2); // zero export to CT
            st.registers.insert(143, 400); // 4000 W
            st.registers.insert(166, 20);
        }
        tokio::spawn(fake_device(slave, Arc::clone(&state)));
        let client = rtu::Client::new(master, Duration::from_millis(200), Duration::ZERO);
        (client, state)
    }

    async fn run(mode: WriteMode, c: &Command) -> (CommandResult, Arc<Mutex<FakeState>>) {
        let (mut client, state) = setup();
        let result = execute(&mut client, 2, &map(), mode, c, NOW).await;
        (result, state)
    }

    #[tokio::test]
    async fn writes_in_order_and_reports_before_and_after() {
        let c = cmd(
            &[("tou_soc_1", 35.0), ("work_mode", 0.0)],
            &["max_sell_power"],
        );
        let (r, state) = run(WriteMode::On, &c).await;
        assert!(r.ok, "{r:?}");
        assert!(!r.dry_run);
        assert_eq!(r.id, "c1");
        assert_eq!(r.written, ["tou_soc_1", "work_mode"]);
        assert_eq!(r.before["work_mode"], 2.0);
        assert_eq!(r.before["tou_soc_1"], 20.0);
        assert_eq!(r.before["max_sell_power"], 4000.0);
        assert_eq!(r.after["work_mode"], 0.0);
        assert_eq!(r.after["tou_soc_1"], 35.0);
        assert_eq!(r.after["max_sell_power"], 4000.0);
        let st = state.lock().unwrap();
        assert_eq!(st.writes, [166, 142], "command order is kept");
        assert_eq!(st.registers[&142], 0);
    }

    #[tokio::test]
    async fn dry_run_reads_but_never_writes() {
        let c = cmd(&[("work_mode", 0.0), ("max_sell_power", 3000.0)], &[]);
        let (r, state) = run(WriteMode::DryRun, &c).await;
        assert!(r.ok, "{r:?}");
        assert!(r.dry_run);
        assert!(r.written.is_empty());
        assert_eq!(r.before["work_mode"], 2.0);
        assert_eq!(r.after["work_mode"], 0.0, "shows what would be written");
        assert_eq!(r.after["max_sell_power"], 3000.0);
        let st = state.lock().unwrap();
        assert!(st.writes.is_empty());
        assert_eq!(st.registers[&142], 2);
    }

    #[tokio::test]
    async fn dry_run_allows_unverified_settings() {
        let c = cmd(&[("untested", 1.0)], &[]);
        let (r, _) = run(WriteMode::DryRun, &c).await;
        assert!(r.ok, "{r:?}");
    }

    #[tokio::test]
    async fn refuses_unverified_settings_when_on() {
        let c = cmd(&[("work_mode", 0.0), ("untested", 1.0)], &[]);
        let (r, state) = run(WriteMode::On, &c).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("untested"), "{r:?}");
        assert!(state.lock().unwrap().writes.is_empty(), "nothing written");
    }

    #[tokio::test]
    async fn off_refuses_writes_but_reads() {
        let (r, state) = run(WriteMode::Off, &cmd(&[("work_mode", 0.0)], &[])).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("off"), "{r:?}");
        assert!(state.lock().unwrap().writes.is_empty());

        let (r, _) = run(WriteMode::Off, &cmd(&[], &["work_mode", "max_sell_power"])).await;
        assert!(r.ok, "{r:?}");
        assert_eq!(r.before["max_sell_power"], 4000.0);
        assert_eq!(r.before, r.after);
    }

    #[tokio::test]
    async fn refuses_out_of_range_values_before_writing_anything() {
        let c = cmd(&[("work_mode", 0.0), ("tou_soc_1", 101.0)], &[]);
        let (r, state) = run(WriteMode::On, &c).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("tou_soc_1"), "{r:?}");
        assert!(state.lock().unwrap().writes.is_empty());
    }

    #[tokio::test]
    async fn stops_at_the_first_failed_write() {
        let (mut client, state) = setup();
        state.lock().unwrap().fail_writes.push(143);
        let c = cmd(
            &[
                ("work_mode", 1.0),
                ("max_sell_power", 3000.0),
                ("tou_soc_1", 30.0),
            ],
            &[],
        );
        let r = execute(&mut client, 2, &map(), WriteMode::On, &c, NOW).await;
        assert!(!r.ok);
        assert!(
            r.error.as_deref().unwrap().contains("max_sell_power"),
            "{r:?}"
        );
        assert_eq!(r.written, ["work_mode"]);
        assert_eq!(
            r.after["work_mode"], 1.0,
            "state after the failure is read back"
        );
        assert_eq!(r.after["tou_soc_1"], 20.0, "later writes are not attempted");
        assert_eq!(state.lock().unwrap().writes, [142]);
    }

    #[tokio::test]
    async fn detects_writes_that_did_not_stick() {
        let (mut client, state) = setup();
        state.lock().unwrap().ignore_writes.push(142);
        let r = execute(
            &mut client,
            2,
            &map(),
            WriteMode::On,
            &cmd(&[("work_mode", 0.0)], &[]),
            NOW,
        )
        .await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("read back"), "{r:?}");
    }

    #[test]
    fn validation_errors() {
        let m = map();
        let ok = cmd(&[("work_mode", 0.0)], &[]);
        assert!(validate(&m, WriteMode::On, &ok, NOW).is_ok());

        let mut expired = ok.clone();
        expired.expires = NOW - 1;
        assert!(
            validate(&m, WriteMode::On, &expired, NOW)
                .unwrap_err()
                .contains("expired")
        );

        let unknown = cmd(&[("nope", 0.0)], &[]);
        assert!(
            validate(&m, WriteMode::On, &unknown, NOW)
                .unwrap_err()
                .contains("nope")
        );
        let unknown_read = cmd(&[], &["nope"]);
        assert!(
            validate(&m, WriteMode::On, &unknown_read, NOW)
                .unwrap_err()
                .contains("nope")
        );

        let twice = cmd(&[("work_mode", 0.0), ("work_mode", 1.0)], &[]);
        assert!(
            validate(&m, WriteMode::On, &twice, NOW)
                .unwrap_err()
                .contains("twice")
        );

        let step = cmd(&[("max_sell_power", 3005.0)], &[]);
        assert!(validate(&m, WriteMode::On, &step, NOW).is_err());

        let too_many: Vec<(&str, f64)> = vec![("work_mode", 0.0); MAX_WRITES + 1];
        assert!(validate(&m, WriteMode::On, &cmd(&too_many, &[]), NOW).is_err());

        let empty = cmd(&[], &[]);
        assert!(
            validate(&m, WriteMode::On, &empty, NOW)
                .unwrap_err()
                .contains("nothing")
        );
    }

    #[test]
    fn validation_returns_raw_words_in_order() {
        let c = cmd(&[("max_sell_power", 3000.0), ("work_mode", 1.0)], &[]);
        let writes = validate(&map(), WriteMode::On, &c, NOW).unwrap();
        assert_eq!(writes, [(1, 300), (0, 1)]);
    }

    #[test]
    fn groups_reads() {
        let b = |start, count| Block { start, count };
        assert_eq!(read_blocks(&[]), []);
        assert_eq!(read_blocks(&[143, 142, 142]), [b(142, 2)]);
        // 142..=146 and 154..=177 are within the gap limit of each other.
        let mut deye: Vec<u16> = vec![142, 143, 145, 146];
        deye.extend(154..=159);
        deye.extend(166..=177);
        assert_eq!(read_blocks(&deye), [b(142, 36)]);
        // Far apart: separate requests.
        assert_eq!(read_blocks(&[10, 500]), [b(10, 1), b(500, 1)]);
        // Never more than one request can carry.
        let wide: Vec<u16> = (0..200).map(|i| i * 2).collect();
        let blocks = read_blocks(&wide);
        assert!(blocks.iter().all(|b| b.count <= MAX_BLOCK_REGISTERS));
        for a in wide {
            assert!(
                blocks.iter().any(|b| a >= b.start && a < b.start + b.count),
                "{a} not covered"
            );
        }
    }
}
