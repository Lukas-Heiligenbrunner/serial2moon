#!/usr/bin/env bash
# Boot-time launcher for the serial2moon stack. Loads the pre-baked container images
# (once) and brings the compose stack up. Invoked by serial2moon.service.
#
# serial2moon's own settings live in an editable file (Mainsail: Configuration Files) at
# /opt/printer_data/config/serial2moon.conf, which serial2moon reads itself — see compose.
set -euo pipefail
cd /opt/serial2moon

# Load pre-baked images on first boot (offline).
if [ ! -f images/.loaded ] && compgen -G "images/*.tar" >/dev/null; then
    for tar in images/*.tar; do
        echo "Loading $tar"
        docker load -i "$tar"
    done
    touch images/.loaded
fi

# The only thing computed here: the printer name shown in Mainsail (used to set the
# container hostnames via compose variable substitution).
echo "PRINTER_NAME=$(hostname)" >.env

# Start (idempotent; containers also auto-restart via 'restart: unless-stopped').
docker compose up -d
