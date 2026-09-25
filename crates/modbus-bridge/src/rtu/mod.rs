//! Minimal Modbus RTU master that can only read holding registers.
//!
//! General-purpose Modbus crates all ship write functions. This bridge must
//! be physically unable to change inverter settings, so it carries its own
//! client that knows exactly one function code (3). Writing is left to a
//! separate binary.

mod crc;
mod frame;

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{self, Instant};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, SerialStream, StopBits};

/// Bytes tolerated before the slave address, e.g. a 0x00 some RS485
/// transceivers emit when the line turns around.
const MAX_SKIPPED_BYTES: usize = 4;
/// Silence that marks the end of stale traffic (t3.5 at 9600 baud is 4 ms;
/// the margin also catches late responses to a timed-out request).
const DRAIN_QUIET: Duration = Duration::from_millis(30);
const DRAIN_MAX_READS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum RtuError {
    #[error("serial I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("no complete response before timeout")]
    Timeout,
    #[error("CRC mismatch")]
    Crc,
    #[error("truncated frame")]
    Truncated,
    #[error("response from unexpected slave {0}")]
    UnexpectedSlave(u8),
    #[error("unexpected function code 0x{0:02X}")]
    UnexpectedFunction(u8),
    #[error("expected {expected} data bytes, got {got}")]
    ByteCount { expected: usize, got: usize },
    #[error("device exception {0} ({name})", name = exception_name(*.0))]
    Exception(u8),
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
}

fn exception_name(code: u8) -> &'static str {
    match code {
        1 => "illegal function",
        2 => "illegal data address",
        3 => "illegal data value",
        4 => "slave device failure",
        5 => "acknowledge",
        6 => "slave device busy",
        _ => "unknown",
    }
}

pub type SerialClient = Client<SerialStream>;

/// Opens a serial port as 8N1 without flow control.
pub fn open_serial(
    path: &str,
    baud: u32,
    timeout: Duration,
    gap: Duration,
) -> io::Result<SerialClient> {
    let port = tokio_serial::new(path, baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .open_native_async()?;
    Ok(Client::new(port, timeout, gap))
}

pub struct Client<T> {
    io: T,
    timeout: Duration,
    gap: Duration,
    ready_at: Instant,
    // Set after any failed exchange: bytes of a late or partial response may
    // still arrive and must not be mistaken for the next answer.
    needs_drain: bool,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Client<T> {
    pub fn new(io: T, timeout: Duration, gap: Duration) -> Self {
        Self {
            io,
            timeout,
            gap,
            ready_at: Instant::now(),
            needs_drain: true,
        }
    }

    /// Function code 3. `start` is the 0-based register address.
    pub async fn read_holding_registers(
        &mut self,
        slave: u8,
        start: u16,
        count: u16,
    ) -> Result<Vec<u16>, RtuError> {
        let request = frame::read_holding_registers_request(slave, start, count)?;
        time::sleep_until(self.ready_at).await;
        if self.needs_drain {
            self.discard_input().await;
            self.needs_drain = false;
        }
        let result = match time::timeout(self.timeout, self.exchange(&request, slave)).await {
            Ok(response) => response.and_then(|frame| frame::parse_response(slave, count, &frame)),
            Err(_elapsed) => Err(RtuError::Timeout),
        };
        // A complete exception frame leaves the line clean; anything else may not.
        self.needs_drain = matches!(&result, Err(e) if !matches!(e, RtuError::Exception(_)));
        self.ready_at = Instant::now() + self.gap;
        result
    }

    async fn exchange(&mut self, request: &[u8], slave: u8) -> Result<Vec<u8>, RtuError> {
        self.io.write_all(request).await?;
        self.io.flush().await?;

        let mut header = [0u8; frame::HEADER_LEN];
        let mut skipped = 0;
        loop {
            self.io.read_exact(&mut header[..1]).await?;
            if header[0] == slave {
                break;
            }
            skipped += 1;
            if skipped > MAX_SKIPPED_BYTES {
                return Err(RtuError::UnexpectedSlave(header[0]));
            }
        }
        self.io.read_exact(&mut header[1..]).await?;

        let len = frame::response_len(header)?;
        let mut response = vec![0u8; len];
        response[..frame::HEADER_LEN].copy_from_slice(&header);
        self.io
            .read_exact(&mut response[frame::HEADER_LEN..])
            .await?;
        Ok(response)
    }

    async fn discard_input(&mut self) {
        let mut buf = [0u8; 64];
        for _ in 0..DRAIN_MAX_READS {
            match time::timeout(DRAIN_QUIET, self.io.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => continue,
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::frame::with_crc;
    use super::*;
    use tokio::io::{DuplexStream, duplex};

    const TIMEOUT: Duration = Duration::from_millis(200);

    fn client() -> (Client<DuplexStream>, DuplexStream) {
        let (master, slave) = duplex(512);
        (Client::new(master, TIMEOUT, Duration::ZERO), slave)
    }

    async fn expect_request(slave: &mut DuplexStream) -> [u8; 8] {
        let mut req = [0u8; 8];
        slave.read_exact(&mut req).await.unwrap();
        assert_eq!(req[1], frame::READ_HOLDING_REGISTERS);
        req
    }

    #[tokio::test]
    async fn reads_registers() {
        let (mut client, mut dev) = client();
        let device = tokio::spawn(async move {
            let req = expect_request(&mut dev).await;
            assert_eq!(&req[..6], &[0x02, 0x03, 0x02, 0x4C, 0x00, 0x01]);
            dev.write_all(&with_crc(&[0x02, 0x03, 0x02, 0x00, 0x2A]))
                .await
                .unwrap();
            dev
        });
        assert_eq!(
            client.read_holding_registers(2, 588, 1).await.unwrap(),
            vec![42]
        );
        device.await.unwrap();
    }

    #[tokio::test]
    async fn skips_leading_noise() {
        let (mut client, mut dev) = client();
        let device = tokio::spawn(async move {
            expect_request(&mut dev).await;
            let mut frame = vec![0x00];
            frame.extend(with_crc(&[0x02, 0x03, 0x02, 0x00, 0x2A]));
            dev.write_all(&frame).await.unwrap();
            dev
        });
        assert_eq!(
            client.read_holding_registers(2, 588, 1).await.unwrap(),
            vec![42]
        );
        device.await.unwrap();
    }

    #[tokio::test]
    async fn exception_is_reported() {
        let (mut client, mut dev) = client();
        let device = tokio::spawn(async move {
            expect_request(&mut dev).await;
            dev.write_all(&with_crc(&[0x01, 0x83, 0x02])).await.unwrap();
            dev
        });
        let err = client.read_holding_registers(1, 8234, 8).await.unwrap_err();
        assert!(matches!(err, RtuError::Exception(2)), "{err}");
        device.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_then_recovers_after_late_garbage() {
        let (mut client, mut dev) = client();
        let device = tokio::spawn(async move {
            // First request: answer only half a frame, too late to matter.
            expect_request(&mut dev).await;
            dev.write_all(&[0x02, 0x03]).await.unwrap();
            // Second request: proper answer.
            expect_request(&mut dev).await;
            dev.write_all(&with_crc(&[0x02, 0x03, 0x02, 0x00, 0x2B]))
                .await
                .unwrap();
            dev
        });
        let err = client.read_holding_registers(2, 588, 1).await.unwrap_err();
        assert!(matches!(err, RtuError::Timeout), "{err}");
        assert_eq!(
            client.read_holding_registers(2, 588, 1).await.unwrap(),
            vec![43]
        );
        device.await.unwrap();
    }

    #[tokio::test]
    async fn closed_port_is_io_error() {
        let (mut client, dev) = client();
        drop(dev);
        let err = client.read_holding_registers(1, 0, 1).await.unwrap_err();
        assert!(matches!(err, RtuError::Io(_)), "{err}");
    }

    /// Guards the read-only rule: no Modbus write function code or write
    /// helper may appear in this crate, and no general Modbus client crate may
    /// be pulled in.
    #[test]
    fn crate_is_read_only() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/rtu");
        let sources = [
            include_str!("mod.rs"),
            include_str!("frame.rs"),
            include_str!("crc.rs"),
        ];
        let forbidden = [
            "write_single",
            "write_multiple",
            "WRITE_SINGLE",
            "WRITE_MULTIPLE",
            "mask_write",
            "read_write_multiple",
        ];
        for (i, src) in sources.iter().enumerate() {
            // Skip this test's own list of forbidden words.
            let code = src.split("fn crate_is_read_only").next().unwrap_or(src);
            for word in forbidden {
                assert!(!code.contains(word), "{dir}: source #{i} contains {word:?}");
            }
        }
        let manifest = include_str!("../../Cargo.toml");
        let deps = manifest.split("[dependencies]").nth(1).unwrap_or("");
        for krate in ["tokio-modbus", "rmodbus", "modbus"] {
            assert!(
                !deps.lines().any(|l| l.trim_start().starts_with(krate)),
                "Cargo.toml depends on {krate}"
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    pub(crate) use super::frame::with_crc;
}
