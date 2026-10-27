use std::fmt;
use std::sync::Arc;

use crate::map::{DerivedOp, DeviceMap, Sensor, Source, StateClass};

/// A counter that keeps reading lower than the last accepted value is taken
/// as genuinely reset (or the earlier value as the glitch) after this many
/// consecutive polls, so one bad read cannot freeze a counter forever.
const REBASELINE_AFTER: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Value(f64),
    /// The block holding the register was not read, or an input of a derived
    /// value was not available.
    Unavailable,
    Rejected(Rejection),
}

impl Outcome {
    pub fn value(&self) -> Option<f64> {
        match self {
            Self::Value(v) => Some(*v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rejection {
    NotFinite,
    OutOfRange { value: f64, min: f64, max: f64 },
    CounterDecreased { previous: f64, value: f64 },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite => f.write_str("not a finite number"),
            Self::OutOfRange { value, min, max } => {
                write!(f, "{value} outside plausible range [{min}, {max}]")
            }
            Self::CounterDecreased { previous, value } => {
                write!(f, "counter went back from {previous} to {value}")
            }
        }
    }
}

/// Turns raw block reads into checked values for one device.
///
/// Stateful only for `total_increasing` sensors, which must never go down.
#[derive(Debug)]
pub struct Decoder {
    map: Arc<DeviceMap>,
    last_total: Vec<Option<f64>>,
    decreases: Vec<u32>,
}

impl Decoder {
    pub fn new(map: Arc<DeviceMap>) -> Self {
        let n = map.sensors.len();
        Self {
            map,
            last_total: vec![None; n],
            decreases: vec![0; n],
        }
    }

    pub fn map(&self) -> &Arc<DeviceMap> {
        &self.map
    }

    /// `blocks[i]` holds the words read for `map.blocks[i]`, or `None` if that
    /// read failed. Returns one outcome per sensor, in map order.
    pub fn decode(&mut self, blocks: &[Option<&[u16]>]) -> Vec<Outcome> {
        let map = Arc::clone(&self.map);
        let mut out: Vec<Outcome> = Vec::with_capacity(map.sensors.len());
        for (i, sensor) in map.sensors.iter().enumerate() {
            let value = match &sensor.source {
                Source::Register(src) => map
                    .raw_value(i, blocks)
                    .map(|raw| raw * src.scale + src.offset),
                Source::Derived { op, inputs } => derive(
                    *op,
                    inputs.iter().map(|&j| out.get(j).and_then(Outcome::value)),
                ),
            };
            let outcome = match value {
                None => Outcome::Unavailable,
                Some(v) => self.check(i, sensor, v),
            };
            out.push(outcome);
        }
        out
    }

    fn check(&mut self, i: usize, sensor: &Sensor, value: f64) -> Outcome {
        if !value.is_finite() {
            return Outcome::Rejected(Rejection::NotFinite);
        }
        if let Some((min, max)) = sensor.range
            && !(min..=max).contains(&value)
        {
            return Outcome::Rejected(Rejection::OutOfRange { value, min, max });
        }
        if sensor.state_class == Some(StateClass::TotalIncreasing) {
            if let Some(previous) = self.last_total[i]
                && value < previous
            {
                self.decreases[i] += 1;
                if self.decreases[i] < REBASELINE_AFTER {
                    return Outcome::Rejected(Rejection::CounterDecreased { previous, value });
                }
            }
            self.decreases[i] = 0;
            self.last_total[i] = Some(value);
        }
        Outcome::Value(value)
    }
}

fn derive(op: DerivedOp, mut inputs: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    // Adding 0.0 turns a -0.0 from max() into +0.0.
    match op {
        DerivedOp::Sum => inputs.try_fold(0.0, |acc, v| v.map(|v| acc + v)),
        DerivedOp::PositivePart => inputs.next().flatten().map(|v| v.max(0.0) + 0.0),
        DerivedOp::NegativePart => inputs.next().flatten().map(|v| (-v).max(0.0) + 0.0),
        // + 0.0 turns -0.0 into 0.0.
        DerivedOp::Negate => inputs.next().flatten().map(|v| -v + 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(text: &str) -> Decoder {
        Decoder::new(Arc::new(DeviceMap::from_toml(text).unwrap()))
    }

    fn value(d: &Decoder, outcomes: &[Outcome], key: &str) -> Outcome {
        outcomes[d.map().sensor_index(key).unwrap()]
    }

    const DTSU: &str = r#"
        [device]
        id = "dtsu"
        name = "DTSU666"
        word_order = "high_first"
        blocks = [[8192, 26], [16414, 12]]

        [[register]]
        key = "l1_voltage"
        name = "L1 voltage"
        address = 8198
        type = "f32"
        scale = 0.1
        range = [0, 300]

        [[register]]
        key = "l1_current"
        name = "L1 current"
        address = 8204
        type = "f32"
        scale = 0.001

        [[register]]
        key = "grid_power"
        name = "Grid power"
        address = 8210
        type = "f32"
        scale = 0.1

        [[register]]
        key = "grid_import_energy"
        name = "Import"
        address = 16414
        type = "f32"
        state_class = "total_increasing"

        [[derived]]
        key = "grid_import_power"
        name = "Import power"
        op = "positive_part"
        inputs = ["grid_power"]

        [[derived]]
        key = "grid_export_power"
        name = "Export power"
        op = "negative_part"
        inputs = ["grid_power"]
    "#;

    fn f32_words(v: f32) -> [u16; 2] {
        let bits = v.to_bits();
        [(bits >> 16) as u16, bits as u16]
    }

    fn dtsu_blocks(voltage: f32, current: f32, power: f32, import: f32) -> (Vec<u16>, Vec<u16>) {
        let mut main = vec![0u16; 26];
        main[6..8].copy_from_slice(&f32_words(voltage));
        main[12..14].copy_from_slice(&f32_words(current));
        main[18..20].copy_from_slice(&f32_words(power));
        let mut energy = vec![0u16; 12];
        energy[0..2].copy_from_slice(&f32_words(import));
        (main, energy)
    }

    #[test]
    fn dtsu_sample_values() {
        // Values read with mbpoll from the live meter.
        let mut d = decoder(DTSU);
        let (main, energy) = dtsu_blocks(2226.0, 596.0, 910.0, 18.67);
        let out = d.decode(&[Some(&main), Some(&energy)]);

        let Outcome::Value(v) = value(&d, &out, "l1_voltage") else {
            panic!()
        };
        assert!((v - 222.6).abs() < 1e-9);
        let Outcome::Value(a) = value(&d, &out, "l1_current") else {
            panic!()
        };
        assert!((a - 0.596).abs() < 1e-9);
        let Outcome::Value(p) = value(&d, &out, "grid_power") else {
            panic!()
        };
        assert!((p - 91.0).abs() < 1e-9);
        let Outcome::Value(e) = value(&d, &out, "grid_import_energy") else {
            panic!()
        };
        assert!((e - 18.67).abs() < 1e-5);

        assert_eq!(value(&d, &out, "grid_import_power"), Outcome::Value(p));
        assert_eq!(value(&d, &out, "grid_export_power"), Outcome::Value(0.0));
    }

    #[test]
    fn export_splits_to_export_power() {
        let mut d = decoder(DTSU);
        let (main, energy) = dtsu_blocks(2300.0, 5000.0, -12345.0, 20.0);
        let out = d.decode(&[Some(&main), Some(&energy)]);
        assert_eq!(value(&d, &out, "grid_import_power"), Outcome::Value(0.0));
        let Outcome::Value(x) = value(&d, &out, "grid_export_power") else {
            panic!()
        };
        assert!((x - 1234.5).abs() < 1e-9);
    }

    #[test]
    fn missing_block_is_unavailable_and_propagates() {
        let mut d = decoder(DTSU);
        let (_, energy) = dtsu_blocks(0.0, 0.0, 0.0, 18.67);
        let out = d.decode(&[None, Some(&energy)]);
        assert_eq!(value(&d, &out, "grid_power"), Outcome::Unavailable);
        assert_eq!(value(&d, &out, "grid_import_power"), Outcome::Unavailable);
        assert!(matches!(
            value(&d, &out, "grid_import_energy"),
            Outcome::Value(_)
        ));
    }

    #[test]
    fn short_block_is_unavailable_not_panic() {
        let mut d = decoder(DTSU);
        let out = d.decode(&[Some(&[1, 2, 3]), Some(&[])]);
        assert!(out.iter().all(|o| *o == Outcome::Unavailable));
        let out = d.decode(&[]);
        assert!(out.iter().all(|o| *o == Outcome::Unavailable));
    }

    #[test]
    fn out_of_range_and_nan_rejected() {
        let mut d = decoder(DTSU);
        let (main, energy) = dtsu_blocks(3500.0, f32::NAN, 0.0, 1.0);
        let out = d.decode(&[Some(&main), Some(&energy)]);
        assert!(matches!(
            value(&d, &out, "l1_voltage"),
            Outcome::Rejected(Rejection::OutOfRange { .. })
        ));
        assert_eq!(
            value(&d, &out, "l1_current"),
            Outcome::Rejected(Rejection::NotFinite)
        );
    }

    const DEYE: &str = r#"
        [device]
        id = "deye"
        name = "Deye"
        word_order = "low_first"
        blocks = [[516, 4], [586, 6]]

        [[register]]
        key = "battery_charge_energy"
        name = "Charged"
        address = 516
        type = "u32"
        scale = 0.1
        state_class = "total_increasing"

        [[register]]
        key = "battery_temperature"
        name = "Battery temperature"
        address = 586
        type = "s16"
        scale = 0.1
        offset = -100

        [[register]]
        key = "battery_soc"
        name = "SOC"
        address = 588
        type = "u16"
        range = [0, 100]
        verified = true

        [[register]]
        key = "battery_power"
        name = "Battery power"
        address = 590
        type = "s16"
    "#;

    fn deye_blocks(charged: u32, temp_raw: u16, soc: u16, power: i16) -> (Vec<u16>, Vec<u16>) {
        let totals = vec![charged as u16, (charged >> 16) as u16, 0, 0];
        let battery = vec![temp_raw, 0, soc, 0, power as u16, 0];
        (totals, battery)
    }

    #[test]
    fn deye_sample_values() {
        let mut d = decoder(DEYE);
        let (totals, battery) = deye_blocks(123_456, 1250, 42, -10);
        let out = d.decode(&[Some(&totals), Some(&battery)]);
        let Outcome::Value(e) = value(&d, &out, "battery_charge_energy") else {
            panic!()
        };
        assert!((e - 12345.6).abs() < 1e-6);
        let Outcome::Value(t) = value(&d, &out, "battery_temperature") else {
            panic!()
        };
        assert!((t - 25.0).abs() < 1e-9);
        assert_eq!(value(&d, &out, "battery_soc"), Outcome::Value(42.0));
        assert_eq!(value(&d, &out, "battery_power"), Outcome::Value(-10.0));
    }

    #[test]
    fn temperature_below_zero() {
        let mut d = decoder(DEYE);
        let (totals, battery) = deye_blocks(0, 900, 50, 0);
        let out = d.decode(&[Some(&totals), Some(&battery)]);
        let Outcome::Value(t) = value(&d, &out, "battery_temperature") else {
            panic!()
        };
        assert!((t + 10.0).abs() < 1e-9);
    }

    #[test]
    fn soc_above_100_rejected() {
        let mut d = decoder(DEYE);
        let (totals, battery) = deye_blocks(0, 1250, 101, 0);
        let out = d.decode(&[Some(&totals), Some(&battery)]);
        assert!(matches!(
            value(&d, &out, "battery_soc"),
            Outcome::Rejected(Rejection::OutOfRange { value: 101.0, .. })
        ));
    }

    #[test]
    fn counter_must_not_decrease_but_rebaselines() {
        let mut d = decoder(DEYE);
        let decode = |d: &mut Decoder, charged: u32| {
            let (totals, battery) = deye_blocks(charged, 1250, 50, 0);
            let out = d.decode(&[Some(&totals), Some(&battery)]);
            value(d, &out, "battery_charge_energy")
        };

        assert!(matches!(decode(&mut d, 1000), Outcome::Value(_)));
        assert!(
            matches!(decode(&mut d, 1000), Outcome::Value(_)),
            "equal is fine"
        );
        assert!(matches!(
            decode(&mut d, 999),
            Outcome::Rejected(Rejection::CounterDecreased { .. })
        ));
        assert!(matches!(decode(&mut d, 1001), Outcome::Value(_)));

        // A persistent lower value is eventually accepted as the new baseline.
        for _ in 1..REBASELINE_AFTER {
            assert!(matches!(decode(&mut d, 5), Outcome::Rejected(_)));
        }
        assert!(matches!(decode(&mut d, 5), Outcome::Value(_)));
        assert!(matches!(decode(&mut d, 6), Outcome::Value(_)));
    }

    #[test]
    fn sum_needs_all_inputs() {
        assert_eq!(
            derive(DerivedOp::Sum, [Some(1.0), Some(2.5)].into_iter()),
            Some(3.5)
        );
        assert_eq!(derive(DerivedOp::Sum, [Some(1.0), None].into_iter()), None);
        assert_eq!(
            derive(DerivedOp::NegativePart, [Some(0.0)].into_iter()),
            Some(0.0)
        );
        assert!(
            derive(DerivedOp::NegativePart, [Some(0.0)].into_iter())
                .unwrap()
                .is_sign_positive()
        );
        assert_eq!(derive(DerivedOp::Negate, [Some(-440.0)].into_iter()), Some(440.0));
        assert_eq!(derive(DerivedOp::Negate, [Some(125.0)].into_iter()), Some(-125.0));
        assert_eq!(derive(DerivedOp::Negate, [None].into_iter()), None);
        assert!(
            derive(DerivedOp::Negate, [Some(0.0)].into_iter())
                .unwrap()
                .is_sign_positive()
        );
    }
}
