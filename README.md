# serial2moon

Run the modern Klipper web frontends (**Mainsail** / **Fluidd**) on a **legacy Marlin
printer** — no firmware change.

serial2moon is a small Rust daemon for a Raspberry Pi (or any Linux host). It talks
**Marlin G-code over USB serial** to the printer while pretending to be a **Klipper API
server** on a Unix socket. Moonraker connects to that socket thinking it's Klipper, so the
whole Mainsail/Fluidd + Moonraker stack just works against a printer that only speaks plain
Marlin G-code.

```
 Mainsail/Fluidd ──HTTP/WS──▶ Moonraker ──UDS (Klipper API)──▶ serial2moon ──USB serial──▶ Marlin printer
```

Tested end-to-end on an **Original Prusa i3 MK3S** with real Moonraker + Mainsail.

## What works

- Live temperatures, fan, and toolhead position / homed axes (tracked from the G-code we
  send, since Marlin doesn't stream position).
- Print **start / pause / resume / cancel** with byte-accurate progress, plus pause/cancel
  parking (lift + present bed).
- File upload, print queue, and the console — driven straight from Mainsail.
- Pause/resume/cancel from the **printer's own LCD** too.
- **Steel sheets** (Prusa `M850`) auto-discovered and shown as selectable macro buttons.
- **Klipper / Firmware restart** from the UI, and optional **host reboot/shutdown** macros.
- Robust serial: line numbers + checksums, automatic resend on a lost/garbled line, and
  automatic reconnect if the printer is unplugged or power-cycled.
- A built-in **mock printer**, so you can try the whole stack with no hardware.

## Getting started

Pick the option that fits you. Most people want option 1.

### 1. Flash the ready-made Raspberry Pi image (easiest)

A complete image with serial2moon + Moonraker + Mainsail pre-installed and starting at boot.

1. Download `serial2moon-rpi-arm64.img.xz` from the
   [**Releases**](https://github.com/Lukas-Heiligenbrunner/serial2moon/releases) page.
2. Flash it with [Raspberry Pi Imager](https://www.raspberrypi.com/software/). In the
   **⚙ OS customisation** step, set a username + password, your Wi-Fi, a hostname, and
   enable **SSH**.
3. Plug the printer into a USB port, boot the Pi, and open `http://<hostname>/` for Mainsail
   (Moonraker is on `:7125`).

The printer is **autodetected** — no config needed in the common case. For tuning options
and how host reboot/shutdown works, see [`image/README.md`](image/README.md).

Targets **arm64** (Pi 3 / 4 / 5 / Zero 2 W). To update, re-flash the latest release.

### 2. Docker Compose on an existing Pi / Linux host

If you already run Docker, you can bring up the same stack yourself. A ready-made
multi-arch image is published to `ghcr.io/lukas-heiligenbrunner/serial2moon`.

**Try it with the mock printer (no hardware):**

```bash
git clone https://github.com/Lukas-Heiligenbrunner/serial2moon.git
cd serial2moon
docker compose up --build
# Mainsail:  http://localhost:8088
# Moonraker: http://localhost:7125
```

Upload a `.gcode` in Mainsail and print it — progress, pause/resume, and temperatures all
work against the simulated printer.

**Drive a real printer:** edit `compose.yml` — in the `serial2moon` service swap the mock
`command:` for the commented `serial` block and pass your device, e.g.:

```yaml
    command:
      - "--transport=serial"
      - "--serial-port=/dev/printer"
      - "--uds-path=/opt/printer_data/run/klippy.sock"
      - "--gcode-dir=/opt/printer_data/gcodes"
    devices:
      - "/dev/serial/by-id/usb-YOUR-PRINTER:/dev/printer"
```

Use a `/dev/serial/by-id/...` path so it survives USB re-enumeration. Omit the baud to
autodetect. (The image in option 1 uses a production-tuned version of this same compose
file at `image/files/opt/serial2moon/compose.yml`.)

### 3. Run the binary directly (advanced / development)

```bash
# Mock printer, no hardware:
cargo run -- --transport mock --uds-path /tmp/klippy_uds --gcode-dir ./gcodes

# Real printer:
cargo run -- --transport serial \
  --serial-port /dev/serial/by-id/usb-YOUR-PRINTER \
  --uds-path /home/pi/printer_data/comms/klippy.sock \
  --gcode-dir /home/pi/printer_data/gcodes
```

Then point Moonraker at the socket with `klippy_uds_address: <your --uds-path>`.

## Configuration

Set options via CLI flags or environment variables (a `.env` file is auto-loaded; see
[`.env.example`](.env.example)). On the prebuilt image, the same settings live in an
editable `serial2moon.conf` shown next to `moonraker.conf` in Mainsail.

| Flag / env | Default | Meaning |
|---|---|---|
| `--transport` / `S2M_TRANSPORT` | `mock` | `mock` or `serial` |
| `--serial-port` / `S2M_SERIAL_PORT` | autodetect | serial device (serial mode) |
| `--baud` / `S2M_BAUD` | autodetect | serial baud (probes + gates on `M115`) |
| `--uds-path` / `S2M_UDS` | `/tmp/klippy_uds` | socket Moonraker connects to |
| `--gcode-dir` / `S2M_GCODE_DIR` | `./gcodes` | gcode files (must match Moonraker) |
| `--bed-size` / `S2M_BED_SIZE` | `220,220,250` | advertised X,Y,Z limits |
| `--extruder-max-temp` / `S2M_EXTRUDER_MAX_TEMP` | `300` | bounds the UI temp input |
| `--bed-max-temp` / `S2M_BED_MAX_TEMP` | `120` | bounds the UI temp input |
| `--pause-lift` / `S2M_PAUSE_LIFT` | `5` | Z lift (mm) on pause/cancel |
| `--pause-retract` / `S2M_PAUSE_RETRACT` | `1` | retract (mm) on pause/cancel |
| `--log-dir` / `S2M_LOG_DIR` | — | also write logs to this dir |
| `--log-max-size-mb` / `S2M_LOG_MAX_SIZE_MB` | `20` | rotate `serial2moon.log` at this size |
| `--log-max-files` / `S2M_LOG_MAX_FILES` | `10` | rotated logs kept, gzip-compressed (`0` = none) |
| `RUST_LOG` | `info,serial2moon=debug` | log verbosity |

## Limitations

- Marlin G-code only; developed and tested against Prusa firmware (MK3S). Other Marlin
  printers should work but aren't verified.
- Klipper-only features (pressure advance, input shaping, bed mesh, etc.) are accepted but
  do nothing — they don't apply to a Marlin printer.
- Single extruder + single bed.
- G-code is streamed one line at a time (depth-1 handshake). A lost line/`ok` is recovered
  by resending, which can cause a brief pause; very dense moves over a slow/noisy USB link
  may not feed fast enough.
- **No power-loss recovery** — an interrupted print can't be resumed.

## Development

```bash
cargo test       # unit tests + a real-PTY serial integration test
cargo clippy
cargo fmt
```

Layout: `src/state` is the single source of truth (one actor task, `watch` snapshots).
`src/transport` is the byte stream (`serial.rs` real port, `mock.rs` in-process Marlin sim).
`src/serial_session` is the sole writer to the printer (priority inbox + `ok` handshake).
`src/klipper_api` is the Unix-socket server (0x03 framing, method dispatch, subscription
deltas). `src/gcode` translates commands; `src/print_job` streams files to the printer.

## License

[MIT](LICENSE) © Lukas Heiligenbrunner
