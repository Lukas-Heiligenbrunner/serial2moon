# serial2moon Raspberry Pi image

A flash-and-go Raspberry Pi OS (arm64) image with the whole stack — serial2moon +
Moonraker + Mainsail — pre-installed and started at boot. No terminal setup required.

## Use it (for end users)

1. Download `serial2moon-rpi-arm64.img.xz` from the project's **Releases**.
2. Flash it with Raspberry Pi Imager (set Wi-Fi/hostname/SSH in the Imager's OS-customization
   step if you want them).
3. Before ejecting, open the **boot** partition and edit `serial2moon.conf`:
   ```
   SERIAL_DEVICE="/dev/serial/by-id/usb-...your-printer..."
   EXTRUDER_MAX_TEMP=300
   BED_MAX_TEMP=120
   ```
   Leave `SERIAL_DEVICE` empty to boot the **simulated** printer (great for a first test).
4. Boot the Pi and open `http://<pi-hostname>/` (Mainsail). Moonraker is on `:7125`.

To change settings later: edit `/boot/firmware/serial2moon.conf` and
`sudo systemctl restart serial2moon` (or reboot).

Targets **arm64** (Pi 3 / 4 / 5 / Zero 2 W).

## How it's built

`../.github/workflows/image.yml` runs on a published release:

1. Cross-compiles the arm64 binary (`cross`, static musl).
2. Builds the serial2moon image and saves it + Moonraker + Mainsail as `*.tar` into
   `files/opt/serial2moon/images/` (so the Pi loads them offline at first boot).
3. Uses [`pguyot/arm-runner-action`](https://github.com/pguyot/arm-runner-action) to run
   `provision.sh` inside a Raspberry Pi OS image (installs Docker, disables ModemManager,
   installs the stack + `serial2moon.service`).
4. Compresses and attaches `serial2moon-rpi-arm64.img.xz` to the release.

## Layout

- `provision.sh` — runs inside the image at build time.
- `files/opt/serial2moon/` — the stack: `compose.yml`, `compose.serial.yml`, `start.sh`
  (boot launcher), and `images/` (baked container tars).
- `files/boot/serial2moon.conf` — user config copied to the boot partition.
- `files/etc/systemd/system/serial2moon.service` — starts the stack at boot.

## Build locally

You don't need CI. On an x86-64 Linux box with Docker:

```bash
cargo install cross
cross build --release --target aarch64-unknown-linux-musl
mkdir -p dist/linux/arm64 && cp target/aarch64-unknown-linux-musl/release/serial2moon dist/linux/arm64/
mkdir -p image/files/opt/serial2moon/images
docker buildx build --platform linux/arm64 -f Dockerfile.pack -t serial2moon:bundled \
  -o type=docker,dest=image/files/opt/serial2moon/images/serial2moon.tar .
docker pull --platform linux/arm64 mkuf/moonraker:latest && docker save mkuf/moonraker:latest -o image/files/opt/serial2moon/images/moonraker.tar
docker pull --platform linux/arm64 ghcr.io/mainsail-crew/mainsail:latest && docker save ghcr.io/mainsail-crew/mainsail:latest -o image/files/opt/serial2moon/images/mainsail.tar
# then run pguyot/arm-runner-action's CLI, or replicate provision.sh against a mounted Pi OS image.
```
(The CI workflow is the supported path; local baking needs loop-mount/qemu privileges.)
