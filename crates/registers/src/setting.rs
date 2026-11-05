//! Writable registers ("settings"). Only registers listed as `[[setting]]`
//! in a device map can ever be written, and only within their range.

/// A single u16 holding register that may be written.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub key: String,
    pub name: String,
    /// 0-based register address.
    pub address: u16,
    /// value = raw * scale
    pub scale: f64,
    pub unit: Option<String>,
    /// Inclusive range of allowed values, in engineering units. Required.
    pub range: (f64, f64),
    /// Writes to unverified settings are refused outside dry-run mode.
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EncodeError {
    #[error("{key}: {value} is not a finite number")]
    NotFinite { key: String, value: f64 },
    #[error("{key}: {value} is outside the allowed range {min}..={max}")]
    OutOfRange {
        key: String,
        value: f64,
        min: f64,
        max: f64,
    },
    #[error("{key}: {value} is not a multiple of the register step {scale}")]
    NotMultiple { key: String, value: f64, scale: f64 },
    #[error("{key}: {value} does not fit in a 16-bit register")]
    Overflow { key: String, value: f64 },
}

impl Setting {
    /// Converts an engineering value into the raw register word, refusing
    /// anything outside the range or between register steps.
    pub fn encode(&self, value: f64) -> Result<u16, EncodeError> {
        if !value.is_finite() {
            return Err(EncodeError::NotFinite {
                key: self.key.clone(),
                value,
            });
        }
        let (min, max) = self.range;
        if value < min || value > max {
            return Err(EncodeError::OutOfRange {
                key: self.key.clone(),
                value,
                min,
                max,
            });
        }
        let raw = value / self.scale;
        if (raw - raw.round()).abs() > 1e-6 {
            return Err(EncodeError::NotMultiple {
                key: self.key.clone(),
                value,
                scale: self.scale,
            });
        }
        let raw = raw.round();
        if !(0.0..=f64::from(u16::MAX)).contains(&raw) {
            return Err(EncodeError::Overflow {
                key: self.key.clone(),
                value,
            });
        }
        Ok(raw as u16)
    }

    /// Engineering value of a raw register word.
    pub fn decode(&self, raw: u16) -> f64 {
        let value = f64::from(raw) * self.scale;
        // Keep 4000 from showing up as 4000.000000000001.
        (value * 1e6).round() / 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn power() -> Setting {
        Setting {
            key: "max_sell_power".into(),
            name: "Max sell power".into(),
            address: 143,
            scale: 10.0,
            unit: Some("W".into()),
            range: (0.0, 12_000.0),
            verified: false,
        }
    }

    #[test]
    fn encodes_scaled_values() {
        assert_eq!(power().encode(4000.0), Ok(400));
        assert_eq!(power().encode(0.0), Ok(0));
        assert_eq!(power().encode(12_000.0), Ok(1200));
    }

    #[test]
    fn decodes_raw_words() {
        assert_eq!(power().decode(400), 4000.0);
        let soc = Setting {
            scale: 1.0,
            range: (0.0, 100.0),
            ..power()
        };
        assert_eq!(soc.decode(35), 35.0);
    }

    #[test]
    fn refuses_values_outside_range() {
        assert!(matches!(
            power().encode(12_010.0),
            Err(EncodeError::OutOfRange { .. })
        ));
        assert!(matches!(
            power().encode(-10.0),
            Err(EncodeError::OutOfRange { .. })
        ));
    }

    #[test]
    fn refuses_values_between_steps() {
        assert!(matches!(
            power().encode(4005.0),
            Err(EncodeError::NotMultiple { .. })
        ));
    }

    #[test]
    fn refuses_non_finite_values() {
        assert!(matches!(
            power().encode(f64::NAN),
            Err(EncodeError::NotFinite { .. })
        ));
        assert!(matches!(
            power().encode(f64::INFINITY),
            Err(EncodeError::NotFinite { .. })
        ));
    }

    #[test]
    fn refuses_values_that_overflow_a_word() {
        let wide = Setting {
            scale: 1.0,
            range: (0.0, 100_000.0),
            ..power()
        };
        assert!(matches!(
            wide.encode(70_000.0),
            Err(EncodeError::Overflow { .. })
        ));
    }
}
