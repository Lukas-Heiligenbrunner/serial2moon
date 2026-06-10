#!/usr/bin/env bash
# Runs INSIDE the Raspberry Pi OS image (emulated chroot via arm-runner-action).
# Installs Docker, disables ModemManager, and bakes in the serial2moon stack + the
# pre-saved container image tars, so the Pi boots straight to a working UI, offline.
set -euo pipefail

# Repo was copied into the image at this path (see image.yml copy_repository_path).
SRC="$(cd "$(dirname "$0")/.." && pwd)"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl

# Docker Engine + compose plugin (convenience script picks the right arch/repo).
curl -fsSL https://get.docker.com | sh

# ModemManager grabs USB-serial devices on plug-in and fights the printer — kill it.
systemctl disable ModemManager.service 2>/dev/null || true
systemctl mask ModemManager.service 2>/dev/null || true

# Install the stack.
install -d /opt/serial2moon
cp -r "$SRC/image/files/opt/serial2moon/." /opt/serial2moon/
cp -r "$SRC/docker" /opt/serial2moon/docker
chmod +x /opt/serial2moon/start.sh

# Boot-partition config (editable from any PC after flashing).
BOOTDIR=/boot/firmware
[ -d "$BOOTDIR" ] || BOOTDIR=/boot
cp "$SRC/image/files/boot/serial2moon.conf" "$BOOTDIR/serial2moon.conf"

# systemd service: bring the stack up at boot.
cp "$SRC/image/files/etc/systemd/system/serial2moon.service" /etc/systemd/system/serial2moon.service
systemctl enable docker.service 2>/dev/null || true
systemctl enable serial2moon.service 2>/dev/null || true

apt-get clean
rm -rf /var/lib/apt/lists/*
echo "serial2moon provisioning complete"
