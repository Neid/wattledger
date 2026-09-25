//! Encoding and parsing of Modbus RTU frames for function code 3 only.

use registers::MAX_BLOCK_REGISTERS;

use super::RtuError;
use super::crc::crc16;

/// Read holding registers. The only function code this crate knows.
pub const READ_HOLDING_REGISTERS: u8 = 0x03;
const EXCEPTION_FLAG: u8 = 0x80;

/// Bytes needed to know the full length of a response: slave, function,
/// byte count (or exception code).
pub const HEADER_LEN: usize = 3;

pub fn read_holding_registers_request(
    slave: u8,
    start: u16,
    count: u16,
) -> Result<[u8; 8], RtuError> {
    if !(1..=247).contains(&slave) {
        return Err(RtuError::InvalidRequest("slave address must be 1..=247"));
    }
    if count == 0 || count > MAX_BLOCK_REGISTERS {
        return Err(RtuError::InvalidRequest("register count must be 1..=125"));
    }
    let [start_hi, start_lo] = start.to_be_bytes();
    let [count_hi, count_lo] = count.to_be_bytes();
    let mut frame = [
        slave,
        READ_HOLDING_REGISTERS,
        start_hi,
        start_lo,
        count_hi,
        count_lo,
        0,
        0,
    ];
    let [crc_lo, crc_hi] = crc16(&frame[..6]).to_le_bytes();
    frame[6] = crc_lo;
    frame[7] = crc_hi;
    Ok(frame)
}

/// Total response length implied by the first [`HEADER_LEN`] bytes.
pub fn response_len(header: [u8; HEADER_LEN]) -> Result<usize, RtuError> {
    match header[1] {
        READ_HOLDING_REGISTERS => Ok(HEADER_LEN + usize::from(header[2]) + 2),
        f if f == READ_HOLDING_REGISTERS | EXCEPTION_FLAG => Ok(HEADER_LEN + 2),
        other => Err(RtuError::UnexpectedFunction(other)),
    }
}

/// Validates a complete response frame and extracts the register values.
pub fn parse_response(slave: u8, count: u16, frame: &[u8]) -> Result<Vec<u16>, RtuError> {
    let Some((body, crc)) = frame.split_last_chunk::<2>() else {
        return Err(RtuError::Truncated);
    };
    if body.len() < HEADER_LEN {
        return Err(RtuError::Truncated);
    }
    if crc16(body) != u16::from_le_bytes(*crc) {
        return Err(RtuError::Crc);
    }
    if body[0] != slave {
        return Err(RtuError::UnexpectedSlave(body[0]));
    }
    match body[1] {
        READ_HOLDING_REGISTERS => {}
        f if f == READ_HOLDING_REGISTERS | EXCEPTION_FLAG => {
            return Err(RtuError::Exception(body[2]));
        }
        other => return Err(RtuError::UnexpectedFunction(other)),
    }
    let data = &body[HEADER_LEN..];
    let expected = usize::from(count) * 2;
    if usize::from(body[2]) != expected || data.len() != expected {
        return Err(RtuError::ByteCount {
            expected,
            got: usize::from(body[2]),
        });
    }
    Ok(data
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect())
}

#[cfg(test)]
pub(crate) fn with_crc(body: &[u8]) -> Vec<u8> {
    let mut frame = body.to_vec();
    frame.extend_from_slice(&crc16(body).to_le_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_request() {
        // mbpoll -a 2 -r 588 -c 1 (0-based): 02 03 02 4C 00 01 + CRC
        let frame = read_holding_registers_request(2, 588, 1).unwrap();
        assert_eq!(&frame[..6], &[0x02, 0x03, 0x02, 0x4C, 0x00, 0x01]);
        assert_eq!(frame[6..], crc16(&frame[..6]).to_le_bytes());
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(read_holding_registers_request(0, 0, 1).is_err());
        assert!(read_holding_registers_request(248, 0, 1).is_err());
        assert!(read_holding_registers_request(1, 0, 0).is_err());
        assert!(read_holding_registers_request(1, 0, 126).is_err());
    }

    #[test]
    fn parses_soc_response() {
        let frame = with_crc(&[0x02, 0x03, 0x02, 0x00, 0x2A]);
        assert_eq!(
            response_len([frame[0], frame[1], frame[2]]).unwrap(),
            frame.len()
        );
        assert_eq!(parse_response(2, 1, &frame).unwrap(), vec![42]);
    }

    #[test]
    fn parses_float_words() {
        let frame = with_crc(&[0x01, 0x03, 0x04, 0x45, 0x0B, 0x70, 0x00]);
        assert_eq!(parse_response(1, 2, &frame).unwrap(), vec![0x450B, 0x7000]);
    }

    #[test]
    fn detects_crc_error() {
        let mut frame = with_crc(&[0x02, 0x03, 0x02, 0x00, 0x2A]);
        frame[4] ^= 0x01;
        assert!(matches!(parse_response(2, 1, &frame), Err(RtuError::Crc)));
    }

    #[test]
    fn reports_exception() {
        let frame = with_crc(&[0x01, 0x83, 0x02]);
        assert_eq!(response_len([0x01, 0x83, 0x02]).unwrap(), frame.len());
        assert!(matches!(
            parse_response(1, 4, &frame),
            Err(RtuError::Exception(2))
        ));
    }

    #[test]
    fn rejects_wrong_slave_and_count() {
        let frame = with_crc(&[0x01, 0x03, 0x02, 0x00, 0x2A]);
        assert!(matches!(
            parse_response(2, 1, &frame),
            Err(RtuError::UnexpectedSlave(1))
        ));
        assert!(matches!(
            parse_response(1, 2, &frame),
            Err(RtuError::ByteCount { .. })
        ));
    }

    #[test]
    fn rejects_unknown_function() {
        assert!(matches!(
            response_len([0x01, 0x10, 0x00]),
            Err(RtuError::UnexpectedFunction(0x10))
        ));
    }

    #[test]
    fn truncated_frames_do_not_panic() {
        for len in 0..5 {
            let frame = vec![0u8; len];
            assert!(parse_response(1, 1, &frame).is_err());
        }
    }
}
