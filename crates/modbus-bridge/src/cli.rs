use std::path::PathBuf;

use anyhow::bail;

pub const DEFAULT_CONFIG: &str = "/etc/wattledger/bridge.toml";

const USAGE: &str = "\
Usage: modbus-bridge [OPTIONS]

Reads Modbus RTU devices (read-only) and publishes to MQTT and InfluxDB line protocol.

Options:
  -c, --config <PATH>  Main config file [default: /etc/wattledger/bridge.toml]
      --dry-run        Print decoded values instead of publishing
      --once           Poll every bus once, then exit (requires --dry-run)
  -h, --help           Show this help
  -V, --version        Show version

Environment:
  WATTLEDGER_LOG       Log level: error, warn, info (default), debug, trace";

#[derive(Debug)]
pub struct Args {
    pub config: PathBuf,
    pub dry_run: bool,
    pub once: bool,
}

pub enum Command {
    Run(Args),
    Help,
    Version,
}

pub fn usage() -> &'static str {
    USAGE
}

pub fn parse() -> anyhow::Result<Command> {
    parse_from(lexopt::Parser::from_env())
}

fn parse_from(mut parser: lexopt::Parser) -> anyhow::Result<Command> {
    use lexopt::prelude::*;

    let mut args = Args {
        config: PathBuf::from(DEFAULT_CONFIG),
        dry_run: false,
        once: false,
    };
    while let Some(arg) = parser.next()? {
        match arg {
            Short('c') | Long("config") => args.config = parser.value()?.into(),
            Long("dry-run") => args.dry_run = true,
            Long("once") => args.once = true,
            Short('h') | Long("help") => return Ok(Command::Help),
            Short('V') | Long("version") => return Ok(Command::Version),
            _ => return Err(arg.unexpected().into()),
        }
    }
    if args.once && !args.dry_run {
        bail!("--once is only supported together with --dry-run");
    }
    Ok(Command::Run(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<Command> {
        parse_from(lexopt::Parser::from_iter(
            std::iter::once("modbus-bridge").chain(args.iter().copied()),
        ))
    }

    #[test]
    fn once_dry_run_with_config() {
        let Command::Run(a) = parse(&["--once", "--dry-run", "-c", "cfg.toml"]).unwrap() else {
            panic!("expected Run");
        };
        assert!(a.once && a.dry_run);
        assert_eq!(a.config, PathBuf::from("cfg.toml"));
    }

    #[test]
    fn once_alone_is_rejected() {
        assert!(parse(&["--once"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
