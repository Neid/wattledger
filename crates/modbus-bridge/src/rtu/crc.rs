/// CRC-16/MODBUS (reflected poly 0xA001, init 0xFFFF). Sent low byte first.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::crc16;

    #[test]
    fn known_frames() {
        // Read 10 holding registers from slave 1 at address 0: 01 03 00 00 00 0A C5 CD
        assert_eq!(
            crc16(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0A]).to_le_bytes(),
            [0xC5, 0xCD]
        );
        // CRC-16/MODBUS check value.
        assert_eq!(crc16(b"123456789"), 0x4B37);
    }
}
