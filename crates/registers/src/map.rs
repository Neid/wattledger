use std::collections::HashMap;

use serde::Deserialize;

use crate::decode::decode_raw;

/// Modbus limits one read-holding-registers request to 125 registers.
pub const MAX_BLOCK_REGISTERS: u16 = 125;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    U16,
    S16,
    U32,
    S32,
    F32,
}

impl DataType {
    /// Number of 16-bit registers the value occupies.
    pub fn width(self) -> u16 {
        match self {
            Self::U16 | Self::S16 => 1,
            Self::U32 | Self::S32 | Self::F32 => 2,
        }
    }
}

/// Which register of a 32-bit pair carries the high half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordOrder {
    HighFirst,
    LowFirst,
}

/// Home Assistant state classes; `TotalIncreasing` also enables the
/// counter-must-not-decrease check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateClass {
    Measurement,
    Total,
    TotalIncreasing,
}

impl StateClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measurement => "measurement",
            Self::Total => "total",
            Self::TotalIncreasing => "total_increasing",
        }
    }
}

/// Values computed from other sensors of the same device. Kept deliberately
/// small: enough to split a signed power into import/export halves and to
/// add up strings, which is what energy dashboards need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedOp {
    /// Sum of all inputs.
    Sum,
    /// `max(x, 0)`
    PositivePart,
    /// `max(-x, 0)`
    NegativePart,
    /// `-x`, e.g. to flip a sign convention for a consumer that expects the
    /// opposite one.
    Negate,
}

impl DerivedOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::PositivePart => "positive_part",
            Self::NegativePart => "negative_part",
            Self::Negate => "negate",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MapError {
    #[error("invalid map file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error(
        "invalid identifier {0:?}: use lowercase ASCII letters, digits and '_', starting with a letter"
    )]
    InvalidIdentifier(String),
    #[error("duplicate key {0:?}")]
    DuplicateKey(String),
    #[error(
        "block [{start}, {count}] is invalid: it must hold 1..={MAX_BLOCK_REGISTERS} registers within the 16-bit address space"
    )]
    InvalidBlock { start: u16, count: u16 },
    #[error(
        "register {key:?} at address {address} ({width} words) is not fully inside any declared block"
    )]
    NotInBlock {
        key: String,
        address: u16,
        width: u16,
    },
    #[error("register {key:?} is 32-bit, so [device] must set word_order")]
    MissingWordOrder { key: String },
    #[error("{key:?}: {reason}")]
    InvalidField { key: String, reason: &'static str },
    #[error("derived {key:?}: input {input:?} is not defined above it")]
    UnknownInput { key: String, input: String },
    #[error("derived {key:?}: {op} takes {expected} input(s), got {got}")]
    InputCount {
        key: String,
        op: &'static str,
        expected: &'static str,
        got: usize,
    },
}

// ---- file format -----------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapFile {
    device: DeviceSection,
    #[serde(default, rename = "register")]
    registers: Vec<RegisterEntry>,
    #[serde(default)]
    derived: Vec<DerivedEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceSection {
    id: String,
    name: String,
    manufacturer: Option<String>,
    model: Option<String>,
    word_order: Option<WordOrder>,
    blocks: Vec<[u16; 2]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterEntry {
    key: String,
    name: String,
    address: u16,
    #[serde(rename = "type")]
    data_type: DataType,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    offset: f64,
    unit: Option<String>,
    device_class: Option<String>,
    state_class: Option<StateClass>,
    range: Option<[f64; 2]>,
    // Unverified unless stated: a register taken from someone else's map
    // must be checked against the device before it is trusted.
    #[serde(default)]
    verified: bool,
    #[serde(default = "yes")]
    mqtt: bool,
    precision: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivedEntry {
    key: String,
    name: String,
    op: DerivedOp,
    inputs: Vec<String>,
    unit: Option<String>,
    device_class: Option<String>,
    state_class: Option<StateClass>,
    range: Option<[f64; 2]>,
    // A derived value is only as verified as its inputs; this flag can only
    // lower that, e.g. while a sign convention is unconfirmed.
    #[serde(default = "yes")]
    verified: bool,
    #[serde(default = "yes")]
    mqtt: bool,
    precision: Option<u8>,
}

fn one() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

// ---- validated map -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Stable identifier; prefixes unique_ids, topics and measurement names.
    pub id: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub start: u16,
    pub count: u16,
}

impl Block {
    fn contains(&self, address: u16, width: u16) -> bool {
        let end = u32::from(self.start) + u32::from(self.count);
        address >= self.start && u32::from(address) + u32::from(width) <= end
    }
}

#[derive(Debug, Clone)]
pub struct RegisterSource {
    pub address: u16,
    pub data_type: DataType,
    pub word_order: WordOrder,
    pub scale: f64,
    pub offset: f64,
    /// Index into [`DeviceMap::blocks`].
    pub block: usize,
    /// Position of the first word within that block.
    pub offset_in_block: usize,
}

#[derive(Debug, Clone)]
pub enum Source {
    Register(RegisterSource),
    /// Inputs are indices into [`DeviceMap::sensors`], always lower than the
    /// derived sensor's own index.
    Derived {
        op: DerivedOp,
        inputs: Vec<usize>,
    },
}

#[derive(Debug, Clone)]
pub struct Sensor {
    pub key: String,
    pub name: String,
    pub unit: Option<String>,
    pub device_class: Option<String>,
    pub state_class: Option<StateClass>,
    /// Inclusive plausibility range; values outside are rejected.
    pub range: Option<(f64, f64)>,
    pub verified: bool,
    /// False keeps the sensor out of MQTT (and therefore out of Domoticz).
    pub mqtt: bool,
    /// Decimal places when formatting.
    pub precision: u8,
    pub source: Source,
}

#[derive(Debug, Clone)]
pub struct DeviceMap {
    pub device: DeviceInfo,
    pub blocks: Vec<Block>,
    /// Registers first, then derived values, both in file order.
    pub sensors: Vec<Sensor>,
}

impl DeviceMap {
    pub fn from_toml(text: &str) -> Result<Self, MapError> {
        let file: MapFile = toml::from_str(text)?;
        compile(file)
    }

    pub fn sensor_index(&self, key: &str) -> Option<usize> {
        self.sensors.iter().position(|s| s.key == key)
    }

    /// Unscaled value of a register sensor, or `None` for derived sensors and
    /// when its block was not read.
    pub fn raw_value(&self, sensor: usize, blocks: &[Option<&[u16]>]) -> Option<f64> {
        let Source::Register(src) = &self.sensors.get(sensor)?.source else {
            return None;
        };
        let block = (*blocks.get(src.block)?)?;
        let width = usize::from(src.data_type.width());
        let words = block.get(src.offset_in_block..src.offset_in_block + width)?;
        decode_raw(src.data_type, src.word_order, words)
    }
}

fn compile(file: MapFile) -> Result<DeviceMap, MapError> {
    let DeviceSection {
        id,
        name,
        manufacturer,
        model,
        word_order,
        blocks,
    } = file.device;
    check_identifier(&id)?;

    let blocks = blocks
        .into_iter()
        .map(|[start, count]| {
            let fits = u32::from(start) + u32::from(count) <= 0x1_0000;
            if count == 0 || count > MAX_BLOCK_REGISTERS || !fits {
                return Err(MapError::InvalidBlock { start, count });
            }
            Ok(Block { start, count })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut sensors = Vec::with_capacity(file.registers.len() + file.derived.len());
    let mut index = HashMap::new();

    for entry in file.registers {
        check_identifier(&entry.key)?;
        let width = entry.data_type.width();
        let order = match (width, word_order) {
            (1, order) => order.unwrap_or(WordOrder::HighFirst),
            (_, Some(order)) => order,
            (_, None) => return Err(MapError::MissingWordOrder { key: entry.key }),
        };
        if !entry.scale.is_finite() || entry.scale == 0.0 {
            return Err(invalid(
                &entry.key,
                "scale must be a finite, non-zero number",
            ));
        }
        if !entry.offset.is_finite() {
            return Err(invalid(&entry.key, "offset must be finite"));
        }
        let Some(block) = blocks.iter().position(|b| b.contains(entry.address, width)) else {
            return Err(MapError::NotInBlock {
                key: entry.key,
                address: entry.address,
                width,
            });
        };
        let range = check_range(&entry.key, entry.range)?;
        let precision = entry.precision.unwrap_or_else(|| {
            if entry.data_type == DataType::F32 {
                3
            } else {
                decimals(entry.scale).max(decimals(entry.offset))
            }
        });
        let source = Source::Register(RegisterSource {
            address: entry.address,
            data_type: entry.data_type,
            word_order: order,
            scale: entry.scale,
            offset: entry.offset,
            block,
            offset_in_block: usize::from(entry.address - blocks[block].start),
        });
        insert(
            &mut sensors,
            &mut index,
            Sensor {
                key: entry.key,
                name: entry.name,
                unit: entry.unit,
                device_class: entry.device_class,
                state_class: entry.state_class,
                range,
                verified: entry.verified,
                mqtt: entry.mqtt,
                precision,
                source,
            },
        )?;
    }

    for entry in file.derived {
        check_identifier(&entry.key)?;
        let (expected, ok) = match entry.op {
            DerivedOp::Sum => ("at least 1", !entry.inputs.is_empty()),
            DerivedOp::PositivePart | DerivedOp::NegativePart | DerivedOp::Negate => {
                ("exactly 1", entry.inputs.len() == 1)
            }
        };
        if !ok {
            return Err(MapError::InputCount {
                key: entry.key,
                op: entry.op.as_str(),
                expected,
                got: entry.inputs.len(),
            });
        }
        let inputs = entry
            .inputs
            .iter()
            .map(|input| {
                index
                    .get(input)
                    .copied()
                    .ok_or_else(|| MapError::UnknownInput {
                        key: entry.key.clone(),
                        input: input.clone(),
                    })
            })
            .collect::<Result<Vec<usize>, _>>()?;
        let range = check_range(&entry.key, entry.range)?;
        let verified = entry.verified && inputs.iter().all(|&i| sensors[i].verified);
        let precision = entry.precision.unwrap_or_else(|| {
            inputs
                .iter()
                .map(|&i| sensors[i].precision)
                .max()
                .unwrap_or(0)
        });
        insert(
            &mut sensors,
            &mut index,
            Sensor {
                key: entry.key,
                name: entry.name,
                unit: entry.unit,
                device_class: entry.device_class,
                state_class: entry.state_class,
                range,
                verified,
                mqtt: entry.mqtt,
                precision,
                source: Source::Derived {
                    op: entry.op,
                    inputs,
                },
            },
        )?;
    }

    Ok(DeviceMap {
        device: DeviceInfo {
            id,
            name,
            manufacturer,
            model,
        },
        blocks,
        sensors,
    })
}

fn insert(
    sensors: &mut Vec<Sensor>,
    index: &mut HashMap<String, usize>,
    sensor: Sensor,
) -> Result<(), MapError> {
    if index.contains_key(&sensor.key) {
        return Err(MapError::DuplicateKey(sensor.key));
    }
    index.insert(sensor.key.clone(), sensors.len());
    sensors.push(sensor);
    Ok(())
}

/// Identifiers end up in MQTT topics, unique_ids and metric names, so they are
/// restricted to characters that are safe in all three.
fn check_identifier(id: &str) -> Result<(), MapError> {
    let mut chars = id.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(MapError::InvalidIdentifier(id.to_owned()))
    }
}

fn check_range(key: &str, range: Option<[f64; 2]>) -> Result<Option<(f64, f64)>, MapError> {
    match range {
        None => Ok(None),
        Some([min, max]) if min.is_finite() && max.is_finite() && min <= max => {
            Ok(Some((min, max)))
        }
        Some(_) => Err(invalid(
            key,
            "range must be [min, max] with finite min <= max",
        )),
    }
}

fn invalid(key: &str, reason: &'static str) -> MapError {
    MapError::InvalidField {
        key: key.to_owned(),
        reason,
    }
}

/// Number of decimals needed to represent `x` exactly (up to 6), e.g. 0.01 -> 2.
fn decimals(x: f64) -> u8 {
    (0u8..=6)
        .find(|&d| {
            let scaled = x * 10f64.powi(i32::from(d));
            (scaled - scaled.round()).abs() < 1e-6
        })
        .unwrap_or(6)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"
        [device]
        id = "dev"
        name = "Test device"
        word_order = "low_first"
        blocks = [[100, 4], [200, 2]]
    "#;

    fn map(body: &str) -> Result<DeviceMap, MapError> {
        DeviceMap::from_toml(&format!("{HEADER}\n{body}"))
    }

    #[test]
    fn locates_registers_in_blocks() {
        let m = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 102
            type = "u32"
            scale = 0.1

            [[register]]
            key = "b"
            name = "B"
            address = 201
            type = "s16"
        "#)
        .unwrap();
        let Source::Register(a) = &m.sensors[0].source else {
            panic!()
        };
        assert_eq!((a.block, a.offset_in_block), (0, 2));
        assert_eq!(m.sensors[0].precision, 1);
        assert!(!m.sensors[0].verified, "registers default to unverified");
        let Source::Register(b) = &m.sensors[1].source else {
            panic!()
        };
        assert_eq!((b.block, b.offset_in_block), (1, 1));
    }

    #[test]
    fn rejects_register_crossing_block_end() {
        let err = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 103
            type = "u32"
        "#)
        .unwrap_err();
        assert!(
            matches!(err, MapError::NotInBlock { address: 103, .. }),
            "{err}"
        );
    }

    #[test]
    fn rejects_register_outside_blocks() {
        let err = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 150
            type = "u16"
        "#)
        .unwrap_err();
        assert!(matches!(err, MapError::NotInBlock { .. }), "{err}");
    }

    #[test]
    fn rejects_duplicate_keys() {
        let err = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 100
            type = "u16"

            [[register]]
            key = "a"
            name = "A again"
            address = 101
            type = "u16"
        "#)
        .unwrap_err();
        assert!(
            matches!(err, MapError::DuplicateKey(ref k) if k == "a"),
            "{err}"
        );
    }

    #[test]
    fn requires_word_order_for_32_bit() {
        let text = r#"
            [device]
            id = "dev"
            name = "Test"
            blocks = [[0, 2]]

            [[register]]
            key = "a"
            name = "A"
            address = 0
            type = "f32"
        "#;
        let err = DeviceMap::from_toml(text).unwrap_err();
        assert!(matches!(err, MapError::MissingWordOrder { .. }), "{err}");
    }

    #[test]
    fn rejects_bad_identifiers() {
        for bad in ["Battery", "1st", "a-b", "a b", ""] {
            let body = format!(
                "[[register]]\nkey = {bad:?}\nname = \"x\"\naddress = 100\ntype = \"u16\"\n"
            );
            assert!(
                matches!(map(&body), Err(MapError::InvalidIdentifier(_))),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn rejects_oversized_block() {
        let text = r#"
            [device]
            id = "dev"
            name = "Test"
            blocks = [[0, 126]]
        "#;
        assert!(matches!(
            DeviceMap::from_toml(text),
            Err(MapError::InvalidBlock { count: 126, .. })
        ));
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 100
            type = "u16"
            scal = 0.1
        "#)
        .unwrap_err();
        assert!(matches!(err, MapError::Parse(_)), "{err}");
    }

    #[test]
    fn derived_inputs_must_exist_above() {
        let err = map(r#"
            [[derived]]
            key = "d"
            name = "D"
            op = "positive_part"
            inputs = ["missing"]
        "#)
        .unwrap_err();
        assert!(matches!(err, MapError::UnknownInput { .. }), "{err}");
    }

    #[test]
    fn derived_input_count() {
        let err = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 100
            type = "s16"

            [[derived]]
            key = "d"
            name = "D"
            op = "negative_part"
            inputs = ["a", "a"]
        "#)
        .unwrap_err();
        assert!(matches!(err, MapError::InputCount { got: 2, .. }), "{err}");
    }

    #[test]
    fn derived_verified_follows_inputs() {
        let m = map(r#"
            [[register]]
            key = "a"
            name = "A"
            address = 100
            type = "s16"
            verified = true

            [[register]]
            key = "b"
            name = "B"
            address = 101
            type = "s16"
            scale = 0.01

            [[derived]]
            key = "only_a"
            name = "From A"
            op = "positive_part"
            inputs = ["a"]

            [[derived]]
            key = "a_and_b"
            name = "A + B"
            op = "sum"
            inputs = ["a", "b"]
        "#)
        .unwrap();
        let only_a = &m.sensors[m.sensor_index("only_a").unwrap()];
        let a_and_b = &m.sensors[m.sensor_index("a_and_b").unwrap()];
        assert!(only_a.verified);
        assert!(!a_and_b.verified);
        assert_eq!(a_and_b.precision, 2);
    }

    #[test]
    fn default_precision() {
        assert_eq!(decimals(1.0), 0);
        assert_eq!(decimals(0.1), 1);
        assert_eq!(decimals(0.01), 2);
        assert_eq!(decimals(0.001), 3);
        assert_eq!(decimals(-100.0), 0);
    }
}
