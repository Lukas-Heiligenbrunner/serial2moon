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

# NOTE: systemd is not running as PID 1 in this build chroot, so `systemctl enable`
# can't be relied on (get.docker.com's own `systemctl enable --now` warns and no-ops
# here). We create the unit symlinks by hand instead — that always works offline.

# Enable a unit at boot by linking it into multi-user.target.wants (offline-safe).
enable_unit() {
    local unit="$1" d
    mkdir -p /etc/systemd/system/multi-user.target.wants
    for d in /etc/systemd/system /usr/lib/systemd/system /lib/systemd/system; do
        if [ -e "$d/$unit" ]; then
            ln -sf "$d/$unit" "/etc/systemd/system/multi-user.target.wants/$unit"
            return 0
        fi
    done
    echo "warning: unit $unit not found to enable" >&2
}

# ModemManager grabs USB-serial devices on plug-in and fights the printer — mask it
# (symlink to /dev/null = masked, regardless of whether it's installed).
ln -sf /dev/null /etc/systemd/system/ModemManager.service

# Install the stack.
install -d /opt/serial2moon
cp -r "$SRC/image/files/opt/serial2moon/." /opt/serial2moon/
cp -r "$SRC/docker" /opt/serial2moon/docker
chmod +x /opt/serial2moon/start.sh

# Boot-partition config (editable from any PC after flashing).
BOOTDIR=/boot/firmware
[ -d "$BOOTDIR" ] || BOOTDIR=/boot
cp "$SRC/image/files/boot/serial2moon.conf" "$BOOTDIR/serial2moon.conf"

# Bring the stack up at boot. Enable Docker (+ containerd) and our service.
cp "$SRC/image/files/etc/systemd/system/serial2moon.service" /etc/systemd/system/serial2moon.service
enable_unit docker.service
enable_unit containerd.service
enable_unit serial2moon.service

# Enable SSH for headless debugging. The `ssh` flag on the boot partition is the canonical
# Raspberry Pi OS way to start sshd at boot; we also enable the unit directly.
# NOTE: a login user must still be set via Raspberry Pi Imager's OS customization (gear
# icon) or a userconf.txt on the boot partition — RPi OS ships no default user.
apt-get install -y --no-install-recommends openssh-server || true
enable_unit ssh.service
touch "$BOOTDIR/ssh"

apt-get clean
rm -rf /var/lib/apt/lists/*
echo "serial2moon provisioning complete"
