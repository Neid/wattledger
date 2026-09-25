/// Formats a value for MQTT payloads and line protocol: rounded to
/// `precision` decimals, without trailing zeros, and never "-0".
///
/// Scaling in f64 produces artifacts like 222.60000000000002; consumers such
/// as Domoticz store the string as-is, so it is cleaned up here.
pub fn format_value(value: f64, precision: u8) -> String {
    let mut s = format!("{:.*}", usize::from(precision), value);
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.').len();
        s.truncate(trimmed);
    }
    if s == "-0" {
        s.replace_range(.., "0");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::format_value;

    #[test]
    fn rounds_and_trims() {
        assert_eq!(format_value(2226.0 * 0.1, 1), "222.6");
        assert_eq!(format_value(18.67, 3), "18.67");
        assert_eq!(format_value(42.0, 0), "42");
        assert_eq!(format_value(10.0, 2), "10");
        assert_eq!(format_value(0.596, 3), "0.596");
        assert_eq!(format_value(-10.04, 1), "-10");
    }

    #[test]
    fn no_negative_zero() {
        assert_eq!(format_value(-0.0, 0), "0");
        assert_eq!(format_value(-0.0004, 3), "0");
    }
}
