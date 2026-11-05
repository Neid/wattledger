#!/usr/bin/env bash
# Installs wattledger on the Raspberry Pi. Run it on the Pi, as root, from the
# directory deploy.sh fills (default ~/wattledger):
#
#   sudo ~/wattledger/install.sh                 # full install, safe to re-run
#   sudo ~/wattledger/install.sh --bridge-only   # binaries, reset helper, units and config only (used by deploy.sh)
#
# A full install:
#   - installs Mosquitto and PostgreSQL (and mbpoll, if the distribution has
#     it) with apt, and creates the "wattledger" database, role and user;
#   - downloads VictoriaMetrics if it is missing, checks its SHA-256 and sets up
#     its systemd service;
#   - installs the bridge binary, its config and its systemd service, then
#     (re)starts it;
#   - installs the USB adapter reset helper and its systemd path unit;
#   - installs the web server (wattledger-web) and its systemd service.
#
# Environment overrides:
#   VM_VERSION=v1.154.0   VictoriaMetrics version to install or switch to
#                         (default: latest release, only when not yet installed)
#   VM_RETENTION=5y       how long VictoriaMetrics keeps data
#   VM_LISTEN=127.0.0.1:8428
#                         VictoriaMetrics HTTP address; use :8428 to reach it
#                         (and its vmui) from other machines. It has no auth.

set -euo pipefail

SRC="$(dirname "$(readlink -f "$0")")"
CONFIG_DIR=/etc/wattledger
BRIDGE_BIN=/usr/local/bin/modbus-bridge
BRIDGE_UNIT=wattledger-bridge.service
RESET_BIN=/usr/local/sbin/wattledger-usb-reset
RESET_UNITS=(wattledger-usb-reset.service wattledger-usb-reset.path)
WEB_BIN=/usr/local/bin/wattledger-web
WEB_UNIT=wattledger-web.service
# System user, PostgreSQL role and database of the web server (peer auth).
WEB_USER=wattledger

VM_BIN=/usr/local/bin/victoria-metrics-prod
VM_DATA=/var/lib/victoria-metrics
VM_UNIT=victoriametrics.service
VM_RETENTION="${VM_RETENTION:-5y}"
VM_LISTEN="${VM_LISTEN:-127.0.0.1:8428}"

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

TMP_DIRS=()
trap 'rm -rf "${TMP_DIRS[@]}"' EXIT

bridge_only=false
case "${1:-}" in
    "") ;;
    --bridge-only) bridge_only=true ;;
    -h | --help) sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
esac

[[ $EUID -eq 0 ]] || die "run as root: sudo $0 $*"
[[ $(uname -m) == aarch64 ]] || die "expected an aarch64 system, got $(uname -m) (64-bit Raspberry Pi OS needed)"
for f in modbus-bridge "$BRIDGE_UNIT" wattledger-usb-reset "${RESET_UNITS[@]}" config/bridge.toml \
    wattledger-web "$WEB_UNIT" config/web.toml; do
    [[ -e $SRC/$f ]] || die "$SRC/$f is missing; run deploy.sh on the build machine first"
done

install_packages() {
    log "Installing packages (mosquitto, postgresql, rsync, curl)"
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq mosquitto mosquitto-clients postgresql rsync curl ca-certificates
    if ! command -v mbpoll >/dev/null; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq mbpoll ||
            warn "mbpoll is not available from apt; it is only needed to verify registers by hand"
    fi
    systemctl enable --now mosquitto
    systemctl enable --now postgresql
}

# The web server connects over the Unix socket with peer authentication, so
# the system user, the role and the database share one name. The tables are
# created by the web server itself (embedded migrations).
setup_database() {
    id "$WEB_USER" >/dev/null 2>&1 ||
        useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$WEB_USER"
    if ! runuser -u postgres -- psql -Atqc "select 1 from pg_roles where rolname = '$WEB_USER'" | grep -q 1; then
        log "Creating PostgreSQL role $WEB_USER"
        runuser -u postgres -- createuser "$WEB_USER"
    fi
    if ! runuser -u postgres -- psql -Atqc "select 1 from pg_database where datname = '$WEB_USER'" | grep -q 1; then
        log "Creating PostgreSQL database $WEB_USER"
        runuser -u postgres -- createdb --owner "$WEB_USER" "$WEB_USER"
    fi
}

database_ready() {
    id "$WEB_USER" >/dev/null 2>&1 &&
        systemctl is-active --quiet postgresql &&
        runuser -u postgres -- psql -Atqc "select 1 from pg_database where datname = '$WEB_USER'" 2>/dev/null | grep -q 1
}

install_web() {
    log "Installing $WEB_BIN"
    install -m 755 "$SRC/wattledger-web" "$WEB_BIN"
    install -m 644 "$SRC/$WEB_UNIT" "/etc/systemd/system/$WEB_UNIT"
    systemctl daemon-reload
    if ! database_ready; then
        warn "PostgreSQL or the $WEB_USER database is missing; run the full install once: sudo $0"
        return
    fi
    systemctl enable "$WEB_UNIT"
    log "Restarting $WEB_UNIT"
    systemctl restart "$WEB_UNIT"
}

vm_installed_version() {
    # Prints e.g. "victoria-metrics-20261009-145800-tags-v1.154.0-0-g..."
    "$VM_BIN" -version 2>/dev/null | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true
}

install_victoriametrics() {
    local current target
    current="$(vm_installed_version)"
    if [[ -x $VM_BIN && -z ${VM_VERSION:-} ]]; then
        log "VictoriaMetrics ${current:-(unknown version)} is already installed (set VM_VERSION to change it)"
    else
        target="${VM_VERSION:-}"
        if [[ -z $target ]]; then
            # Fetch first, then parse: piping curl into "grep -m1" makes curl
            # fail with a broken pipe, which pipefail turns into an error.
            local release
            release="$(curl -fsSL https://api.github.com/repos/VictoriaMetrics/VictoriaMetrics/releases/latest)" ||
                die "could not look up the latest VictoriaMetrics release; set VM_VERSION=v1.x.y"
            target="$(sed -nE '/"tag_name"/{s/.*"tag_name": *"(v[^"]+)".*/\1/p;q;}' <<<"$release")"
            [[ -n $target ]] || die "no tag_name in the GitHub release response; set VM_VERSION=v1.x.y"
        fi
        if [[ $current == "$target" ]]; then
            log "VictoriaMetrics $target is already installed"
        else
            download_victoriametrics "$target"
        fi
    fi

    id victoriametrics >/dev/null 2>&1 ||
        useradd --system --no-create-home --home-dir "$VM_DATA" --shell /usr/sbin/nologin victoriametrics
    install -d -o victoriametrics -g victoriametrics -m 750 "$VM_DATA"

    log "Writing /etc/systemd/system/$VM_UNIT (retention $VM_RETENTION, listening on $VM_LISTEN)"
    cat >"/etc/systemd/system/$VM_UNIT" <<EOF
# Written by wattledger install.sh; re-running it overwrites this file.
[Unit]
Description=VictoriaMetrics time-series database
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=victoriametrics
Group=victoriametrics
ExecStart=$VM_BIN -storageDataPath=$VM_DATA -retentionPeriod=$VM_RETENTION -httpListenAddr=$VM_LISTEN
Restart=always
RestartSec=5
LimitNOFILE=65536

NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=$VM_DATA
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable "$VM_UNIT"
    systemctl restart "$VM_UNIT"
}

download_victoriametrics() {
    local version=$1 name url tmp
    name="victoria-metrics-linux-arm64-$version"
    url="https://github.com/VictoriaMetrics/VictoriaMetrics/releases/download/$version"
    tmp="$(mktemp -d)"
    TMP_DIRS+=("$tmp")

    log "Downloading VictoriaMetrics $version"
    curl -fsSL -o "$tmp/$name.tar.gz" "$url/$name.tar.gz"
    curl -fsSL -o "$tmp/checksums.txt" "$url/${name}_checksums.txt"
    (cd "$tmp" && grep " $name.tar.gz\$" checksums.txt | sha256sum --check --status) ||
        die "SHA-256 mismatch for $name.tar.gz"
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp" victoria-metrics-prod

    systemctl stop "$VM_UNIT" 2>/dev/null || true
    install -m 755 "$tmp/victoria-metrics-prod" "$VM_BIN"
    log "Installed $VM_BIN $(vm_installed_version)"
}

install_bridge() {
    log "Installing $BRIDGE_BIN"
    install -m 755 "$SRC/modbus-bridge" "$BRIDGE_BIN"

    # The repository's config/ is the source of truth: files are overwritten,
    # files that exist only on the Pi are left alone.
    log "Syncing config to $CONFIG_DIR"
    install -d -m 755 "$CONFIG_DIR"
    rsync -rt --chmod=D755,F644 "$SRC/config/" "$CONFIG_DIR/"

    # Root helper that resets a hung USB adapter when the bridge asks for it.
    log "Installing $RESET_BIN"
    install -m 755 "$SRC/wattledger-usb-reset" "$RESET_BIN"

    install -m 644 "$SRC/$BRIDGE_UNIT" "/etc/systemd/system/$BRIDGE_UNIT"
    local unit
    for unit in "${RESET_UNITS[@]}"; do
        install -m 644 "$SRC/$unit" "/etc/systemd/system/$unit"
    done
    systemctl daemon-reload
    systemctl enable "$BRIDGE_UNIT"
    systemctl enable --now wattledger-usb-reset.path

    # The unit only grants access to ttyACM devices.
    local port dev name
    while read -r port; do
        if [[ ! -e $port ]]; then
            warn "serial port $port not found; the bus will keep retrying"
        else
            dev="$(readlink -f "$port")"
            name="${dev#/dev/}"
            name="${name%%[0-9]*}"
            [[ $name == ttyACM ]] ||
                warn "$port is $dev, which the service may not open; add 'DeviceAllow=char-$name rw' to deploy/$BRIDGE_UNIT"
        fi
    done < <(sed -nE 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$CONFIG_DIR/bridge.toml")

    log "Restarting $BRIDGE_UNIT"
    systemctl restart "$BRIDGE_UNIT"
}

if ! $bridge_only; then
    install_packages
    setup_database
    install_victoriametrics
    if [[ -n ${SUDO_USER:-} && $SUDO_USER != root ]] && ! id -nG "$SUDO_USER" | grep -qw dialout; then
        log "Adding $SUDO_USER to dialout (for --dry-run by hand; log in again to apply)"
        usermod -aG dialout "$SUDO_USER"
    fi
fi
install_bridge
install_web

sleep 2
for unit in "$BRIDGE_UNIT" "$WEB_UNIT"; do
    systemctl is-active --quiet "$unit" ||
        warn "$unit is not running; see: journalctl -u $unit -n 50"
done
log "Done. Logs: journalctl -u $BRIDGE_UNIT -u $WEB_UNIT -f"
log "Web pages: http://$(hostname).local:8080/"
