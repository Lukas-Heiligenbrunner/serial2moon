#!/usr/bin/env bash
# Boot-time launcher for the serial2moon stack. Loads the pre-baked container images
# (once), turns the user's /boot config into a .env + optional serial overlay, and brings
# the compose stack up. Invoked by serial2moon.service.
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

# 3) Generate .env consumed by compose / the binary (clap reads S2M_* from the env).
{
    echo "S2M_EXTRUDER_MAX_TEMP=${EXTRUDER_MAX_TEMP}"
    echo "S2M_BED_MAX_TEMP=${BED_MAX_TEMP}"
} >.env

if [ -n "${SERIAL_DEVICE}" ] && [ -e "${SERIAL_DEVICE}" ]; then
    echo "serial2moon: using printer at ${SERIAL_DEVICE}"
    {
        echo "S2M_TRANSPORT=serial"
        echo "S2M_SERIAL_PORT=/dev/printer"
        echo "SERIAL_DEVICE=${SERIAL_DEVICE}"
        [ -n "${SERIAL_BAUD}" ] && echo "S2M_BAUD=${SERIAL_BAUD}"
    } >>.env
    cp -f compose.serial.yml docker-compose.override.yml
else
    echo "serial2moon: no serial device configured/found -> mock mode"
    echo "S2M_TRANSPORT=mock" >>.env
    rm -f docker-compose.override.yml
fi

# 4) Start (idempotent; containers also auto-restart via 'restart: unless-stopped').
docker compose up -d
