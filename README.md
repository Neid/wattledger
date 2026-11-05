# wattledger

A small, reliable home energy bridge and (later) planner for a Raspberry Pi 4.

It reads a **Deye SUN-10K-SG05LP3** hybrid inverter and a **Chint DTSU666** grid
meter over Modbus RTU, and publishes their data to:

- **MQTT** with Home Assistant discovery, consumed by Domoticz' "MQTT Auto
  Discovery Client Gateway";
- a **time-series database** via InfluxDB line protocol over HTTP
  (VictoriaMetrics, or InfluxDB v1/v2), at full polling resolution.

A web server (`wattledger-web`) shows the live energy flow, history charts
and RCE prices, and carries out battery export plans through the bridge.
Later phases add a planner that schedules battery charging, discharging and
export from prices and forecasts.

## Safety rule

**The bridge only writes what a device map allows, only when told to, and
only within range.** Its Modbus client (`crates/modbus-bridge/src/rtu`) is
hand-written because general-purpose Modbus crates ship every write function.
It knows function code 3 (read holding registers) and 16 (write multiple
registers, the only write Deye accepts), and nothing else; a unit test fails
if another write function or a Modbus client crate shows up.

- Only registers listed as `[[setting]]` in a device map can be written. Each
  has a mandatory range, and a value outside it (or between register steps)
  is refused before anything is sent.
- `[writes] mode` in `bridge.toml` is `off`, `dry_run` (read, never write) or
  `on`. In `on`, settings with `verified = false` are refused.
- Writes run only between poll cycles, one register at a time, in the order
  the command gives. The bridge reads the settings before and after and
  answers with both, so the caller can restore the previous values.
- The shipped config is `dry_run`, and every Deye setting is unverified (see
  [Writing inverter settings](#writing-inverter-settings)).

## Phases

| Phase | Scope | Status |
|---|---|---|
| 1 | Workspace, `registers` crate, read-only bridge to MQTT and line protocol, dry-run mode, systemd deployment | **done** |
| 2 | Verify every Deye register against the display, add remaining totals, Grafana dashboards | next |
| 2.5 | Web server: live flow, history charts, RCE prices, export plans written through the bridge (dry run until the settings are verified) | **done** |
| 3 | `planner`: price feed (Polish tariff zones / dynamic tariff, market price for net-billing export), PV forecast, load forecast from history, battery wear cost (~0.07 PLN/kWh cycled, 16.1 kWh LiFePO4), writes Deye TOU slots a few times per day, only when the plan changes | later |
| 4 | Control the SmartLoad/GEN output (water heater), then a heat pump | later |

## Architecture

```
            ┌──────────── modbus-bridge (one process, tokio current-thread) ─────────────┐
 RS485 #1 ──┤ bus task "grid-meter"  (DTSU666, slave 1, every 2 s) ─┐                     │
            │                                                      ├─► Fanout ─┬─► MQTT sink ───► Mosquitto ─► Domoticz
 RS485 #2 ──┤ bus task "inverter"    (Deye, slave 2, every 5 s) ────┘  (try_send) ├─► Influx sink ─► VictoriaMetrics
            │                                                                 └─► stdout (--dry-run)
            │      ▲ write commands (between polls)                                     │
            └──────┼─────────────────────────────────────────────────────────────────────┘
                   │ MQTT wattledger/command/deye (+ /result)
            ┌──────┴──────── wattledger-web (axum, htmx, PostgreSQL) ──────────────────┐
            │ live values (MQTT) · history (VictoriaMetrics) · RCE prices (PSE, curl)  │
            │ export plans → controller: baseline read, export writes, restore         │
            └──────────────────────────────────────────────────────────────────────────┘
```

- **One task per serial bus.** Each has its own poll interval, reads the
  device's register blocks one request at a time, and reopens its port with
  backoff (1 s up to 60 s) after an I/O error or repeated timeouts. A failing
  bus never affects the other one.
- **Hung adapters are reset.** A USB-RS485 adapter can hang while it stays on
  the USB bus: the port opens, but nothing comes back, and reopening it does
  not help. When a bus gets no answer for `usb_reset_after` (3 minutes in the
  shipped config), the bridge writes a request to
  `/run/wattledger-bridge/reset-<bus>`. `wattledger-usb-reset.path` then runs
  `/usr/local/sbin/wattledger-usb-reset` as root, which switches the
  adapter's USB hub port off for 3 s and on again, like unplugging it. If the
  bus stays silent, later resets wait twice as long each time, up to 1 hour.
  The helper only accepts `ttyACM`/`ttyUSB` ports, so it cannot reset any
  other USB device.
- **Decoding** happens in the `registers` crate: raw words become scaled
  values, which are checked against a plausibility range. `total_increasing`
  counters are also rejected if they go backwards. A rejected value is not
  published, and the rejection is logged.
- **Sinks never block polling.** Each sink has a bounded queue. If a sink falls
  behind, readings are dropped for that sink and the drop is logged. MQTT
  publishes are dropped while the broker is unreachable. The line-protocol sink
  buffers up to `buffer_lines` lines and drops the oldest first.
- **Logging is rate-limited.** A recurring failure is logged when it starts,
  then at most every 5 minutes, and once more when it clears.
- **Clean shutdown on SIGTERM:** the bridge publishes `offline`, disconnects
  from MQTT and flushes the line buffer (with timeouts). If the process dies
  instead, the broker publishes the LWT `offline`.

### Workspace layout

```
Cargo.toml                    workspace, release profile tuned for size
config/
  bridge.toml                 buses, MQTT, database
  devices/dtsu666.toml        register map, DTSU666
  devices/deye-sg05lp3.toml   register map, Deye SG05LP3
crates/
  registers/                  map format, validation, decoding, plausibility (pure, no I/O)
  modbus-bridge/
    src/rtu/                  FC3 + FC16 Modbus RTU client (CRC, framing, resync)
    src/bus.rs                per-bus polling task, runs write jobs between polls
    src/write.rs              write commands: validate, read before, write, read back
    src/reading.rs            Reading + non-blocking fan-out
    src/sinks/                mqtt + HA discovery + commands, influx line protocol, stdout
  web/                        wattledger-web
    migrations/               PostgreSQL schema (run at startup)
    src/control/              export plans: decisions, bridge client, controller loop
    src/energy.rs             counters -> per-bucket energy, ratios, export earnings
    src/prices.rs             PSE RCE parsing and fetching
    src/projection.rs         export income projection, best start
    src/web/                  routes, pages, SVG charts
    assets/                   htmx, CSS, tooltip script (built into the binary)
deploy/
  deploy.sh                   build machine: build, copy to the Pi, update + restart
  install.sh                  on the Pi: Mosquitto, VictoriaMetrics, bridge, services
  wattledger-bridge.service   systemd unit
  wattledger-web.service      systemd unit of the web server
  wattledger-usb-reset        root helper: resets a hung USB-RS485 adapter
  wattledger-usb-reset.path   systemd units that run it on the bridge's request
  wattledger-usb-reset.service
tools/mbpoll/                 the mbpoll commands used to verify registers
```

## Configuration

### Main config (`config/bridge.toml`)

This file lists the buses (port, baud, poll interval, timeouts, devices) and
holds the `[mqtt]` and `[influx]` sections. Every option is commented in the
file. Device map paths are relative to the config file.

### Device maps (`config/devices/*.toml`)

Adding a device means adding a map file and one `devices = [...]` entry. No
code changes are needed.

```toml
[device]
id = "deye"                         # prefix of unique_ids, topics, metric names; never change
name = "Inverter Deye SUN-10K"
manufacturer = "Deye"
model = "SUN-10K-SG05LP3-EU-SM2"
word_order = "low_first"            # for 32-bit types: low_first | high_first
blocks = [[586, 6], ...]            # [start, count] per read request (count <= 125)

[[register]]
key = "battery_temperature"         # unique_id = deye_battery_temperature; never rename
name = "Battery temperature"
address = 586                       # 0-based
type = "s16"                        # u16 | s16 | u32 | s32 | f32
scale = 0.1                         # value = raw * scale + offset
offset = -100                       # (raw - 1000) * 0.1
unit = "°C"
device_class = "temperature"
state_class = "measurement"         # measurement | total | total_increasing
range = [-30, 80]                   # plausibility; outside -> rejected
verified = false                    # default false; see "Verifying registers"
# mqtt = false                      # keep out of MQTT (database only)
# precision = 1                     # decimals; default follows scale (f32: 3)

[[derived]]                         # computed from sensors defined above it
key = "battery_charge_power"
name = "Battery charge power"
op = "negative_part"                # sum | positive_part (max(x,0)) | negative_part (max(-x,0)) | negate (-x)
inputs = ["battery_power"]
unit = "W"
device_class = "power"
state_class = "measurement"
```

When a map is loaded, it is checked: every register must lie entirely inside a
declared block, keys must be unique identifiers, 32-bit types require
`word_order`, and derived values may only reference sensors defined above
them. Unknown fields are errors, so a typo like `scal = 0.1` is caught.

## Web server

`wattledger-web` listens on port 8080 (`config/web.toml`) and has four tabs:

| Tab | Shows |
|---|---|
| Energy flow | `domoticz/Energy.html` (the same file), fed from `/api/live`: live values from MQTT, today's totals from VictoriaMetrics |
| History | day / week / month / year: production, home use, grid import/export, battery charge/discharge, inverter own use, solar used at home %, battery efficiency %, export earnings at RCE |
| RCE prices | today and tomorrow (or any stored day) per 15 minutes, with the current slot and the best hour to export |
| Export | schedule "export from time X until battery Y%", with projected energy, end time and income, the best start with known prices, the plan list, the inverter's current settings and the app settings (max export power, battery capacity, ...) |

There is no login: keep port 8080 on the home network.

- **Data.** Prices, settings, plans and every command sent to the bridge are
  in PostgreSQL (database `wattledger`, Unix socket, peer authentication).
  History comes straight from VictoriaMetrics; nothing is copied.
- **Prices.** Every 15 minutes the server fetches RCE days that are missing
  (the last `backfill_days`, today, and tomorrow from 13:00) from
  `api.raporty.pse.pl` with `curl`. The binary has no TLS stack, so it stays a
  static musl build without a C toolchain.
- **Definitions.** Inverter own use = (solar + battery out + grid in) − (home +
  battery in + grid out). Solar used at home = (solar − export) / solar.
  Battery efficiency = discharged / charged. Earnings = each 15 minutes of
  export × that slot's RCE, negative prices as zero, × the export price
  factor (1 by default).
- **Projection.** From the live SOC, the battery delivers
  `(SOC − stop SOC) × capacity × discharge efficiency`; it exports at the
  export power while also covering the home load setting.

### Export plans

A plan has a start, a stop SOC, an export power and a latest end. The
controller checks plans every 5 seconds:

1. **Start:** it reads the 22 export settings once and stores them as the
   baseline, then writes Deye's forced-discharge recipe: every time-of-use
   slot gets the export power, the stop SOC and no grid charging, max sell
   power gets the export power, solar sell and time of use are switched on,
   and the work mode becomes "Selling first" (written last).
2. **Finish:** when the live SOC reaches the stop SOC, at the latest end, or
   when you press Stop, it writes the baseline back, work mode first.
3. **Failures:** a refused start is retried every minute until the latest
   end. A start that wrote part of the settings is rolled back at once. If
   the bridge does not answer, the export may be running, so a plan that got
   as far as a baseline is always restored before it is closed. A failed
   restore is retried every minute.

Only one plan runs at a time, and plans may not overlap. With the bridge in
`dry_run`, plans go through every step and are marked "dry run"; nothing on
the inverter changes.

## Writing inverter settings

The Deye map's `[[setting]]` entries (work mode 142, max sell power 143, solar
sell 145, time of use 146, slot power 154–159, slot SOC 166–171, slot charge
172–177) come from the ha-solarman `deye_p3` map and are **not yet checked**
on this inverter. To check them:

1. With the bridge in `dry_run` (the shipped config), open Export → **Read
   inverter settings** and compare every value with the inverter display
   (System settings and Time of use). Max sell power is 4 kW today, so
   `max_sell_power` must show 4000; 400 or 40000 means the scale is wrong.
2. Schedule a short dry-run export and read the bridge log
   (`journalctl -u wattledger-bridge`): it lists what it would write.
3. Set `verified = true` for each matching setting in
   `config/devices/deye-sg05lp3.toml`, set `[writes] mode = "on"` in
   `config/bridge.toml`, and deploy.
4. Watch the first real export at the inverter: it should sell from the
   battery at the export power and stop at the stop SOC, and its settings
   should be back as before once the plan ends.

## Verifying registers

All Deye registers are enabled. `battery_soc` was checked against the
inverter display; the rest passed a consistency check of a live reading (see
the comment at the top of `config/devices/deye-sg05lp3.toml`). The DTSU666
blocks were verified with mbpoll, except for power factor.

1. Stop the service, since the ports are opened exclusively:
   `sudo systemctl stop wattledger-bridge`
2. Run `modbus-bridge -c /etc/wattledger/bridge.toml --once --dry-run`. Each
   sensor is printed with its value, unit, raw register value, source and
   status (`UNVERIFIED`, `REJECTED: ...`, `unavailable`, `no-mqtt`). The exit
   code is non-zero if any block failed.
3. Compare the values with the inverter display (and with mbpoll if needed:
   `tools/mbpoll/`).
4. Set `verified = true` for each register that matches, in the repository's
   `config/devices/*.toml`, then run `deploy/deploy.sh`. It syncs the config
   and restarts the service.

Unverified sensors go to the database but not to MQTT, so Domoticz never gets
devices it would later need cleaning up. `mqtt.publish_unverified = true`
overrides this. `--dry-run` without `--once` keeps polling and prints every
reading.

Open points:
- **DTSU `grid_power` sign:** positive at night, so assumed `+ = import`.
  Confirm on the first export. If it's wrong, swap `positive_part` and
  `negative_part` for `grid_import_power` and `grid_export_power`.
- **Deye `grid_power` (625) sign:** `+ = import`, matching the DTSU666 while
  importing. Confirm on the first export.
- **Deye `gen_power` and energy counters:** published, but not yet compared
  with the inverter display.
- **DTSU power factor block (8234):** unverified. If the meter rejects the
  block, it shows as a failed block and the other blocks are unaffected.

## MQTT and Domoticz

| What | Topic | Retained |
|---|---|---|
| Discovery | `homeassistant/sensor/<device>_<key>/config` | yes |
| State | `wattledger/<device>/<key>` (plain number) | no |
| Availability / LWT | `wattledger/status` (`online` / `offline`) | yes |

- Discovery is republished on every (re)connect and whenever
  `homeassistant/status` reports `online`.
- `unique_id` is `<device>_<key>` and is the Domoticz database key. **Never
  rename a key or device id after it has been published.**
- Each discovery config carries `device_class`, `state_class`, the unit,
  `suggested_display_precision` and a `device` block. The `device` identifiers
  are `wattledger_<device id>`.
- Domoticz only creates a device when its first state arrives. States are sent
  right after discovery, then at most every `publish_interval` (default 10 s)
  per device. The database still gets every poll.
- **kWh meter pairing:** Domoticz merges a kWh sensor with the W sensor of the
  same device whose `unique_id` diverges from it latest (the logic is in
  `MQTTAutoDiscover.cpp`, `get_auto_discovery_sensor_unit`).
  - Energy keys are `<x>_energy` with a matching `<x>_power`, e.g.
    `dtsu_grid_import_energy` / `dtsu_grid_import_power` and
    `deye_pv_energy` / `deye_pv_power`.
  - Per-phase keys start with the phase (`l1_power`, `l1_load_power`), so they
    can never tie with the intended partner.
  - A unit test replays Domoticz' matching rule over the shipped maps.
- Signed powers are split into import/export (charge/discharge) halves with
  `[[derived]]` entries. The signed value is published as well.

### Energy flow page

`domoticz/Energy.html` is a Domoticz custom page: an animated diagram of the
power flowing between solar, grid, home and battery (with the battery's state
of charge), plus today's totals. `deploy/deploy.sh` copies it to
`~/domoticz/www/templates/`, and after a browser refresh it is under
**Custom → Energy** in Domoticz. It reads the devices through the Domoticz
JSON API by their `unique_id`s, so it needs no idx numbers. Grid power comes
from the DTSU666. Open the file directly with `?demo` to see it with made-up
values.

### Domoticz Energy Dashboard

Domoticz has its own Energy Dashboard (Setup → Settings → Energy Dashboard).
Assign:

| Field | Device |
|---|---|
| P1/Grid | the virtual P1 Smart Meter below (or Grid meter DTSU666 (Grid import energy)) |
| Solar | Inverter Deye SUN-10K (PV energy) |
| Battery | Inverter Deye SUN-10K (Battery flow (+charge / -discharge)) |
| Battery SOC | Inverter Deye SUN-10K (Battery SOC) |
| Battery Voltage | Inverter Deye SUN-10K (Battery voltage) |
| Battery Energy In | Inverter Deye SUN-10K (Battery charged energy) |
| Battery Energy Out | Inverter Deye SUN-10K (Battery discharged energy) |

The dashboard wants battery power as + charge / - discharge, the opposite of
the Deye's `battery_power`, so `battery_flow_power` exists only for it.

It shows grid export only for a "P1 Smart Meter" device, which MQTT discovery
cannot create (with the import kWh device, export counts as zero). So the
bridge can feed a virtual one:

1. Setup → Hardware: add "Dummy (Does nothing, use for virtual switches
   only)", then on it **Create Virtual Sensors**, type **P1 Smart Meter**.
2. Note the new device's idx (Setup → Devices) and set it in
   `[mqtt.domoticz_p1]` in `config/bridge.toml`, then deploy.
3. The bridge publishes `{"idx":N,"nvalue":0,"svalue":"USAGE1;0;RETURN1;0;CONS;PROD"}`
   to `domoticz/in` (counters in Wh, power in W) with the DTSU666's import and
   export counters and grid power.
4. Domoticz only acts on `domoticz/in` with an **MQTT Client Gateway with LAN
   interface** hardware (127.0.0.1:1883, publish topic "out" or none). The
   "MQTT Auto Discovery" hardware shows the topic in its settings but ignores
   messages on it.

The counters are absolute, so the day the device is created may show the
whole meter total as that day's usage; from the next day on it is right.

## Time-series database

There is one line per device poll, all fields in one line, with a nanosecond
timestamp:

```
dtsu l1_voltage=222.6,grid_power=91,grid_import_energy=18.67,... 1790369924092177293
```

VictoriaMetrics stores this as metrics `dtsu_l1_voltage`, `dtsu_grid_power`
and so on. Unverified sensors are included without a tag, because tagging them
would split each series in two the day its flag is flipped.

`influx.url` is used as-is. Point it at VictoriaMetrics `/write`, InfluxDB v1
`/write?db=...` or InfluxDB v2 `/api/v2/write?org=...&bucket=...` (plus
`token`). Batches are sent every `batch_interval`. A failed batch is retried;
one the server refuses with a 4xx status is dropped.

## Build and test

```sh
cargo test                  # unit tests: decoding, maps, framing, polling, writes, web, database
cargo clippy --all-targets
```

The web server's database tests create a throwaway database per test, so
they need a local PostgreSQL where your user may create databases.
`.cargo/config.toml` points `DATABASE_URL` at
`postgres:///postgres?host=/var/run/postgresql`; export your own
`DATABASE_URL` to use another server. Once, on the build machine:

```sh
sudo -u postgres createuser --createdb "$USER"
```

## Cross-compiling for the Pi (aarch64)

The bridge and the web server are built as **static musl binaries**, so they
do not depend on the Pi's glibc version. Rust's bundled `rust-lld` links it, which
`.cargo/config.toml` configures, so no cross toolchain, Docker or zig is
needed:

```sh
rustup target add aarch64-unknown-linux-musl     # once
cargo build --release --target aarch64-unknown-linux-musl -p modbus-bridge -p web
# -> target/aarch64-unknown-linux-musl/release/{modbus-bridge,wattledger-web}
```

(`cross build --release --target aarch64-unknown-linux-musl` also works if you
prefer Docker.)

## Deployment

Two scripts in `deploy/` handle deployment. The repository's `config/` is the
source of truth. Each deploy overwrites the matching files in
`/etc/wattledger` on the Pi, so make config changes in the repository, not on
the Pi.

| Script | Runs on | Does |
|---|---|---|
| `deploy/deploy.sh [user@host]` | build machine | builds the aarch64 binary, copies it with `config/`, the unit and `install.sh` to `~/wattledger` on the Pi, then installs and restarts the bridge if it is already installed |
| `sudo ~/wattledger/install.sh` | Pi | full install, safe to re-run (see below) |

The host defaults to `$WATTLEDGER_PI`, or `pi@raspberrypi.local` if that is
unset. `--no-build` skips `cargo build`. You need SSH access to the Pi and
`rsync` on both machines. If sudo on the Pi asks for a password, `deploy.sh`
prompts for it.

### First install

```sh
deploy/deploy.sh pi@raspberrypi.local              # copies files; says the bridge isn't installed yet
ssh -t pi@raspberrypi.local sudo ~/wattledger/install.sh
```

`install.sh` must run on a 64-bit Raspberry Pi OS (aarch64). It:

1. Installs `mosquitto`, `mosquitto-clients`, `postgresql`, `rsync` and
   `curl` with apt, plus `mbpoll` if apt has it, and enables Mosquitto and
   PostgreSQL. It creates the system user, PostgreSQL role and database
   `wattledger` (the web server creates its tables itself).
2. Installs **VictoriaMetrics** if it is missing. It takes the latest
   `victoria-metrics-linux-arm64` release from GitHub and checks the tarball
   against the release's SHA-256 checksums. The binary goes to
   `/usr/local/bin/victoria-metrics-prod`, the data to
   `/var/lib/victoria-metrics`, and it runs as `victoriametrics.service`
   under a `victoriametrics` system user. An existing install is kept. Set
   `VM_VERSION` to change it.
3. Adds the invoking user to `dialout`, so `--dry-run` works by hand.
4. Installs the bridge binary, syncs the config to `/etc/wattledger`,
   installs `wattledger-bridge.service`, enables it and restarts it. It warns
   if a configured serial port is missing, or if a port is not a `ttyACM`
   device (the unit only allows `char-ttyACM`).
5. Installs `wattledger-web` and `wattledger-web.service` (user
   `wattledger`), enables it and restarts it.

Environment overrides (pass them through sudo, e.g.
`sudo VM_RETENTION=10y ~/wattledger/install.sh`):

| Variable | Default | Meaning |
|---|---|---|
| `VM_VERSION` | latest, only if not installed | VictoriaMetrics version to install or switch to, e.g. `v1.154.0` |
| `VM_RETENTION` | `5y` | data retention; VictoriaMetrics' own default is only 1 month |
| `VM_LISTEN` | `127.0.0.1:8428` | HTTP address. Set `:8428` to open vmui (`/vmui`) from other machines. There is no authentication. |

Each run of `install.sh` rewrites `victoriametrics.service`, so pass the same
overrides every time you run it.

Domoticz is not installed by the script. In Domoticz, add the "MQTT Auto
Discovery Client Gateway" hardware, pointed at `127.0.0.1:1883` with the
discovery prefix `homeassistant`.

### Checking readings by hand

```sh
ssh pi@raspberrypi.local
sudo systemctl stop wattledger-bridge         # the serial ports are opened exclusively
modbus-bridge -c /etc/wattledger/bridge.toml --once --dry-run
sudo systemctl start wattledger-bridge
```

### Updates

```sh
deploy/deploy.sh                                   # build, copy, install, restart
ssh pi@raspberrypi.local journalctl -u wattledger-bridge -f
```

Under the hood this runs `sudo ~/wattledger/install.sh --bridge-only`, which
leaves packages, PostgreSQL and VictoriaMetrics alone and updates both
services. If the Pi was installed before the web server existed, run the full
install once (`sudo ~/wattledger/install.sh`) to get PostgreSQL and the
database; until then the update skips the web service with a warning.

The unit runs the bridge under a dynamic unprivileged user with access to
`ttyACM` devices only, and restarts it 5 s after any exit.

To reset an adapter by hand (the bridge reopens the port by itself):

```sh
sudo wattledger-usb-reset /dev/serial/by-id/usb-1a86_USB_Single_Serial_5ACB127910-if00
journalctl -u wattledger-usb-reset    # resets the bridge asked for
``` Set
`WATTLEDGER_LOG=debug` in the unit for more detail.
