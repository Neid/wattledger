//! Asks a root helper to reset a serial adapter that has gone silent.
//!
//! A USB-RS485 adapter can hang while it stays on the USB bus: the port still
//! opens, but nothing comes back. Reopening the port does not help; only a USB
//! reset or unplugging it does. The bridge runs unprivileged, so it writes a
//! request file into its systemd runtime directory, and
//! `wattledger-usb-reset.path` runs `deploy/wattledger-usb-reset` as root.

use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{env, fs, io};

/// Longest wait between resets of an adapter that stays silent.
const MAX_WAIT: Duration = Duration::from_secs(3600);

pub struct UsbReset {
    dir: PathBuf,
    file_name: String,
    after: Duration,
    silent_since: Instant,
    last_reset: Option<Instant>,
    /// Silence needed before the next reset; doubles after each reset.
    wait: Duration,
}

impl UsbReset {
    /// The directory systemd creates for `RuntimeDirectory=`, if any.
    pub fn runtime_dir() -> Option<PathBuf> {
        let dirs = env::var_os("RUNTIME_DIRECTORY")?;
        // Several directories are separated by ':'; the first is ours.
        let first = dirs.to_str()?.split(':').next()?;
        (!first.is_empty()).then(|| PathBuf::from(first))
    }

    /// `bus` must be a valid file name part (config validates bus names).
    pub fn new(dir: PathBuf, bus: &str, after: Duration, now: Instant) -> Self {
        Self {
            dir,
            file_name: format!("reset-{bus}"),
            after,
            silent_since: now,
            last_reset: None,
            wait: after,
        }
    }

    /// The bus got an answer: start over.
    pub fn alive(&mut self, now: Instant) {
        self.silent_since = now;
        self.last_reset = None;
        self.wait = self.after;
    }

    /// Called when the open port faults. Returns how long the bus has been
    /// silent if a reset is due now.
    pub fn due(&mut self, now: Instant) -> Option<Duration> {
        let since = self.last_reset.unwrap_or(self.silent_since);
        if now.duration_since(since) < self.wait {
            return None;
        }
        self.last_reset = Some(now);
        self.wait = (self.wait * 2).min(MAX_WAIT);
        Some(now.duration_since(self.silent_since))
    }

    /// Writes the request; the rename makes it appear complete or not at all.
    pub fn request(&self, port: &str) -> io::Result<()> {
        let tmp = self.dir.join(format!(".{}.tmp", self.file_name));
        fs::write(&tmp, format!("{port}\n"))?;
        fs::rename(&tmp, self.dir.join(&self.file_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn reset(now: Instant) -> UsbReset {
        UsbReset::new(PathBuf::from("/nonexistent"), "bus", 3 * MIN, now)
    }

    #[test]
    fn resets_after_silence_then_backs_off() {
        let t0 = Instant::now();
        let mut r = reset(t0);
        assert_eq!(r.due(t0 + 2 * MIN), None);
        assert_eq!(r.due(t0 + 3 * MIN), Some(3 * MIN));
        // Next one only after twice the wait, then four times.
        assert_eq!(r.due(t0 + 8 * MIN), None);
        assert_eq!(r.due(t0 + 9 * MIN), Some(9 * MIN));
        assert_eq!(r.due(t0 + 20 * MIN), None);
        assert_eq!(r.due(t0 + 21 * MIN), Some(21 * MIN));
    }

    #[test]
    fn backoff_is_capped() {
        let t0 = Instant::now();
        let mut r = reset(t0);
        let mut t = t0;
        for _ in 0..10 {
            t += MAX_WAIT;
            assert!(r.due(t).is_some());
        }
        assert_eq!(r.wait, MAX_WAIT);
    }

    #[test]
    fn answer_starts_over() {
        let t0 = Instant::now();
        let mut r = reset(t0);
        assert!(r.due(t0 + 3 * MIN).is_some());
        r.alive(t0 + 4 * MIN);
        assert_eq!(r.due(t0 + 6 * MIN), None);
        assert_eq!(r.due(t0 + 7 * MIN), Some(3 * MIN));
    }

    #[test]
    fn request_writes_port_file() {
        let dir = env::temp_dir().join(format!("wattledger-usb-reset-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let r = UsbReset::new(dir.clone(), "inverter", MIN, Instant::now());
        r.request("/dev/serial/by-id/usb-x").unwrap();
        let text = fs::read_to_string(dir.join("reset-inverter")).unwrap();
        assert_eq!(text, "/dev/serial/by-id/usb-x\n");
        assert!(!dir.join(".reset-inverter.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
