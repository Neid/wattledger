use crate::map::{DataType, WordOrder};

/// Combines two registers into a 32-bit value. Devices disagree on which
/// register holds the high half (DTSU666: first, Deye: second), so the order
/// is always explicit.
pub fn to_u32(words: [u16; 2], order: WordOrder) -> u32 {
    let (high, low) = match order {
        WordOrder::HighFirst => (words[0], words[1]),
        WordOrder::LowFirst => (words[1], words[0]),
    };
    (u32::from(high) << 16) | u32::from(low)
}

pub fn to_f32(words: [u16; 2], order: WordOrder) -> f32 {
    f32::from_bits(to_u32(words, order))
}

/// Decodes the unscaled value of one register (or register pair).
///
/// Returns `None` when fewer words are supplied than the type needs, so a
/// short read can never cause a panic.
pub fn decode_raw(data_type: DataType, order: WordOrder, words: &[u16]) -> Option<f64> {
    let value = match data_type {
        DataType::U16 => f64::from(*words.first()?),
        DataType::S16 => f64::from(*words.first()? as i16),
        DataType::U32 => f64::from(to_u32(pair(words)?, order)),
        DataType::S32 => f64::from(to_u32(pair(words)?, order) as i32),
        DataType::F32 => f64::from(to_f32(pair(words)?, order)),
    };
    Some(value)
}

fn pair(words: &[u16]) -> Option<[u16; 2]> {
    match words {
        [a, b, ..] => Some([*a, *b]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_high_word_first() {
        // 2231.0f32 == 0x450B_7000, as the DTSU666 sends it.
        assert_eq!(to_f32([0x450B, 0x7000], WordOrder::HighFirst), 2231.0);
        assert_eq!(
            decode_raw(DataType::F32, WordOrder::HighFirst, &[0x450B, 0x7000]),
            Some(2231.0)
        );
    }

    #[test]
    fn f32_negative_power() {
        // -910.0f32 == 0xC463_8000: export shows up as a negative Pt.
        assert_eq!(to_f32([0xC463, 0x8000], WordOrder::HighFirst), -910.0);
    }

    #[test]
    fn s16_negative() {
        assert_eq!(
            decode_raw(DataType::S16, WordOrder::HighFirst, &[0xFFF6]),
            Some(-10.0)
        );
        assert_eq!(
            decode_raw(DataType::U16, WordOrder::HighFirst, &[0xFFF6]),
            Some(65526.0)
        );
    }

    #[test]
    fn u32_low_word_first() {
        assert_eq!(to_u32([0x5678, 0x1234], WordOrder::LowFirst), 0x1234_5678);
        // 100000 = 0x0001_86A0 -> Deye sends [0x86A0, 0x0001].
        assert_eq!(
            decode_raw(DataType::U32, WordOrder::LowFirst, &[0x86A0, 0x0001]),
            Some(100_000.0)
        );
    }

    #[test]
    fn u32_high_word_first() {
        assert_eq!(to_u32([0x1234, 0x5678], WordOrder::HighFirst), 0x1234_5678);
    }

    #[test]
    fn s32_negative() {
        assert_eq!(
            decode_raw(DataType::S32, WordOrder::LowFirst, &[0xFFFE, 0xFFFF]),
            Some(-2.0)
        );
    }

    #[test]
    fn short_input_is_none() {
        assert_eq!(decode_raw(DataType::U16, WordOrder::HighFirst, &[]), None);
        assert_eq!(
            decode_raw(DataType::F32, WordOrder::HighFirst, &[0x450B]),
            None
        );
    }
}
