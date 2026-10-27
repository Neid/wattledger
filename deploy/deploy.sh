#!/usr/bin/env bash
# Builds the bridge for the Pi and ships it. Run it on the build machine from
# anywhere in the repository:
#
#   deploy/deploy.sh [user@host]      # default: $WATTLEDGER_PI or pi@raspberrypi.local
#   deploy/deploy.sh --no-build [user@host]
#
# It copies the binary, config/, the systemd units, the USB reset helper and
# install.sh to
# ~/wattledger on the Pi. If the bridge is already installed there, it then
# runs "install.sh --bridge-only", which installs the binary, unit and config
# and restarts the service (sudo may ask for your password). The first time,
# run the full install on the Pi instead: sudo ~/wattledger/install.sh

set -euo pipefail

TARGET=aarch64-unknown-linux-musl
REMOTE_DIR=wattledger   # relative to the remote user's home

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

build=true
pi="${WATTLEDGER_PI:-pi@raspberrypi.local}"
for arg in "$@"; do
    case "$arg" in
        --no-build) build=false ;;
        -h | --help) sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
        -*) die "unknown option: $arg (see --help)" ;;
        *) pi="$arg" ;;
    esac
done

cd "$(dirname "$(readlink -f "$0")")/.."
bin="target/$TARGET/release/modbus-bridge"

if $build; then
    log "Building $bin"
    cargo build --release --target "$TARGET" -p modbus-bridge
fi
[[ -f $bin ]] || die "$bin not found; build it or drop --no-build"

log "Copying files to $pi:~/$REMOTE_DIR"
ssh "$pi" "mkdir -p $REMOTE_DIR"
rsync -az "$bin" deploy/install.sh deploy/wattledger-bridge.service \
    deploy/wattledger-usb-reset deploy/wattledger-usb-reset.path deploy/wattledger-usb-reset.service \
    "$pi:$REMOTE_DIR/"
rsync -az --delete config/ "$pi:$REMOTE_DIR/config/"

if ssh "$pi" 'test -f /etc/systemd/system/wattledger-bridge.service'; then
    log "Updating the installed bridge"
    ssh -t "$pi" "sudo ~/$REMOTE_DIR/install.sh --bridge-only"
else
    log "The bridge is not installed on $pi yet. Run the full install there once:"
    echo "    ssh -t $pi sudo ~/$REMOTE_DIR/install.sh"
fi
