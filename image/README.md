# serial2moon Raspberry Pi image

A flash-and-go Raspberry Pi OS (arm64) image with the whole stack — serial2moon +
Moonraker + Mainsail — pre-installed and started at boot. No terminal setup required.

## Use it (for end users)

1. Download `serial2moon-rpi-arm64.img.xz` from the project's **Releases**.
2. Flash it with Raspberry Pi Imager and use the **⚙ OS-customization** step to set a
   username + password, enable **SSH**, and configure **Wi-Fi**/hostname.
3. Connect your printer's USB cable, boot the Pi, and open `http://<pi-hostname>/`
   (Mainsail). Moonraker is on `:7125`.

serial2moon **autodetects** the printer among the connected USB serial devices — no
configuration needed in the common case. If no printer is found it reports an error and
keeps retrying (there is no demo/mock mode in this image).

Optional tuning lives in **`serial2moon.conf`**, editable right in **Mainsail → Machine →
Configuration Files** (next to `moonraker.conf`). It's created automatically on first boot.
After editing, **restart serial2moon** to apply (reboot, or `sudo systemctl restart serial2moon`):
```
RUST_LOG=info                # set "debug" to log every serial line + much more
S2M_EXTRUDER_MAX_TEMP=300    # match your firmware; bounds the UI temp inputs
S2M_BED_MAX_TEMP=120
S2M_PAUSE_LIFT=5             # pause/cancel lift (mm); S2M_PAUSE_RETRACT for retract
#S2M_SERIAL_PORT=...         # pin a /dev/serial/by-id/... path (else autodetect)
#S2M_BAUD=115200             # pin a baud (else autodetect via M115)
```
Steel sheets are auto-discovered from the printer (`M850`) and appear as macro buttons.

**Restarting:**
- **Restart / Firmware Restart** (the dropdown by the emergency-stop button) work as
  expected: *Restart* re-initializes the printer over the existing link, *Firmware Restart*
  drops and reopens the serial connection.
- **Reboot / shut down the Pi:** use the **`HOST_REBOOT`** / **`HOST_SHUTDOWN`** macro
  buttons. (Mainsail's own *Machine → Power* host buttons can't work here — Moonraker runs
  in a container and refuses to reboot the host from inside one. serial2moon instead drops a
  request that a privileged host-side systemd unit acts on.) Anyone with UI access can
  trigger these, just like the physical power button.

Logs are written to Moonraker's logs dir, so `serial2moon.log` is downloadable from
Mainsail's **Machine → Logfiles**.

Targets **arm64** (Pi 3 / 4 / 5 / Zero 2 W).

## Updating

The serial2moon image is baked into the `.img` (it isn't pulled from a registry), so to
update, **re-flash the latest released `.img`**.

## How it's built

`../.github/workflows/image.yml` runs on a published release:

1. Builds the arm64 serial2moon image via the multi-arch `Dockerfile` (cross-compiled in
   the builder stage) and saves it + Moonraker + Mainsail as `*.tar` into
   `files/opt/serial2moon/images/` (so the Pi loads them offline at first boot).
2. Uses [`pguyot/arm-runner-action`](https://github.com/pguyot/arm-runner-action) to run
   `provision.sh` inside a Raspberry Pi OS image (installs Docker, disables ModemManager,
   installs the stack + `serial2moon.service`).
3. Compresses and attaches `serial2moon-rpi-arm64.img.xz` to the release.

## Layout

- `provision.sh` — runs inside the image at build time.
- `files/opt/serial2moon/` — the stack: `compose.yml`, `start.sh`
  (boot launcher), and `images/` (baked container tars).
- `serial2moon.conf` — created in Moonraker's config dir on first run, edited via Mainsail.
- `files/etc/systemd/system/serial2moon.service` — starts the stack at boot.

## Build locally

You don't need CI. On an x86-64 Linux box with Docker + buildx:

```bash
mkdir -p image/files/opt/serial2moon/images
# arm64 serial2moon image -> loadable tar (cross-compiled inside the Dockerfile)
docker buildx build --platform linux/arm64 -f Dockerfile -t serial2moon:bundled \
  -o type=docker,dest=image/files/opt/serial2moon/images/serial2moon.tar .
docker pull --platform linux/arm64 mkuf/moonraker:latest && docker save mkuf/moonraker:latest -o image/files/opt/serial2moon/images/moonraker.tar
docker pull --platform linux/arm64 ghcr.io/mainsail-crew/mainsail:latest && docker save ghcr.io/mainsail-crew/mainsail:latest -o image/files/opt/serial2moon/images/mainsail.tar
# then run pguyot/arm-runner-action's CLI, or replicate provision.sh against a mounted Pi OS image.
```
(The CI workflow is the supported path; local baking needs loop-mount/qemu privileges.)
