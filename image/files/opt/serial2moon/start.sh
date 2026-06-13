#!/usr/bin/env bash
# Boot-time launcher for the serial2moon stack. Loads the pre-baked container images
# (once), turns the user's /boot config into a .env, and brings the compose stack up.
# Invoked by serial2moon.service. serial2moon always runs in serial mode and autodetects
# the printer; the optional overrides below just pin a device/baud if needed.
set -euo pipefail
cd /opt/serial2moon

# 1) Load pre-baked images on first boot (offline).
if [ ! -f images/.loaded ] && compgen -G "images/*.tar" >/dev/null; then
    for tar in images/*.tar; do
        echo "Loading $tar"
        docker load -i "$tar"
    done
    touch images/.loaded
fi

# 2) Read user config from the boot partition (editable without reflashing).
CONF=/boot/firmware/serial2moon.conf
[ -f "$CONF" ] || CONF=/boot/serial2moon.conf
SERIAL_DEVICE=""
SERIAL_BAUD=""
EXTRUDER_MAX_TEMP=300
BED_MAX_TEMP=120
# shellcheck disable=SC1090
[ -f "$CONF" ] && source "$CONF"

# 3) Generate .env (clap reads S2M_* from the env). Transport is fixed to serial in
#    compose.yml; SERIAL_DEVICE/SERIAL_BAUD are optional pins (else auto).
{
    echo "S2M_EXTRUDER_MAX_TEMP=${EXTRUDER_MAX_TEMP}"
    echo "S2M_BED_MAX_TEMP=${BED_MAX_TEMP}"
    # Shown as the printer name in Mainsail (Moonraker container hostname).
    echo "PRINTER_NAME=$(hostname)"
    # /dev is bind-mounted whole, so the host device path is valid inside the container.
    [ -n "${SERIAL_DEVICE}" ] && echo "S2M_SERIAL_PORT=${SERIAL_DEVICE}"
    [ -n "${SERIAL_BAUD}" ] && echo "S2M_BAUD=${SERIAL_BAUD}"
} >.env

# 4) Start (idempotent; containers also auto-restart via 'restart: unless-stopped').
docker compose up -d
