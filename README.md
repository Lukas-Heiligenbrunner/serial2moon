# serial2moon

Run the modern Klipper web frontends (Mainsail / Fluidd) on a **legacy Marlin printer**.

serial2moon is a small Rust daemon for a Raspberry Pi (or any Linux host) that talks
**Marlin G-code over USB serial** to the printer, while presenting a **Klipper API server**
on a Unix domain socket. Moonraker connects to that socket believing it is talking to
Klipper — so the whole Mainsail/Fluidd + Moonraker stack works against a printer that
only speaks plain Marlin G-code, with no firmware change.

```
 Mainsail/Fluidd ──HTTP/WS──▶ Moonraker ──UDS (Klipper API)──▶ serial2moon ──USB serial──▶ Marlin printer
```

## Status

MVP. Working today, verified end-to-end against real Moonraker + Mainsail containers:

- Klipper API server over UDS: `info`, `objects/list`, `objects/query`, `objects/subscribe`
  (with per-connection field-level deltas), `gcode/script`, `gcode/subscribe_output`,
  `register_remote_method`, `emergency_stop`, `list_endpoints`.
- Live temperatures, targets, fan.
- **Live toolhead position + homed axes**, tracked optimistically from outgoing
  `G0/G1/G28/G90/G91/G92/M82/M83` (Marlin doesn't stream position).
- Print start / pause / resume / cancel, with byte-accurate progress.
- G-code translation: standard codes pass through; Klipper-isms
  (`SET_HEATER_TEMPERATURE`, `TURN_OFF_HEATERS`, `SET_FAN_SPEED`) are translated;
  unknown Klipper macros are **acked-and-logged** so the UI never hangs.
- Marlin serial handling: depth-1 `ok` flow control, `busy:`-aware timeouts, `M155`
  temperature autoreport, `M115` baud autodetection.
- **Resilience**: automatic serial reconnect with exponential backoff (survives USB
  re-enumeration / printer power-cycle); commands fail fast while offline instead of
  hanging the UI; mid-print printer reset (`start` banner) re-initializes in place and
  fails the active job cleanly.
- A built-in **mock printer** so you can run the whole thing with no hardware.

See `docs` in [`/home/lukas/.claude/plans/parallel-bouncing-pancake.md`](.) for the design,
the protocol notes, and documented MVP scope cuts.

## Quick start (no hardware)

```bash
cargo run -- --transport mock --uds-path /tmp/klippy_uds --gcode-dir ./gcodes
```

Then point a Moonraker at `klippy_uds_address: /tmp/klippy_uds`.

## Full test harness (Moonraker + Mainsail in Docker)

```bash
docker compose up --build
# open http://localhost:8088   (Mainsail)
# Moonraker API on http://localhost:7125
```

The three containers share the socket and gcode directories via named volumes. Confirm the
bridge is recognized:

```bash
curl -s http://localhost:7125/server/info | grep -o '"klippy_state": "[a-z]*"'
# -> "klippy_state": "ready"
```

Upload a `.gcode` in Mainsail and print it — progress, pause/resume, and temperatures all
work against the simulated printer.

## Using a real printer

```bash
cargo run -- --transport serial \
  --serial-port /dev/serial/by-id/usb-YOUR-PRINTER \
  --gcode-dir /home/pi/printer_data/gcodes \
  --uds-path /home/pi/printer_data/comms/klippy.sock
```

Omit `--baud` to autodetect (probes common bauds and gates on an `M115` `FIRMWARE_NAME:`
reply). Prefer a `/dev/serial/by-id/...` path so USB re-enumeration doesn't break it.

For the Docker harness with real hardware, see the commented `serial` block in
`compose.yml` (uncomment the `command:` and `devices:` entries).

## Configuration

CLI flags or environment (`.env` is auto-loaded). See `.env.example`. Notably:

| Flag / env | Default | Meaning |
|---|---|---|
| `--transport` / `S2M_TRANSPORT` | `mock` | `mock` or `serial` |
| `--uds-path` / `S2M_UDS` | `/tmp/klippy_uds` | socket Moonraker connects to |
| `--gcode-dir` / `S2M_GCODE_DIR` | `./gcodes` | gcode files (match Moonraker) |
| `--serial-port` / `S2M_SERIAL_PORT` | — | serial device (serial mode) |
| `--baud` / `S2M_BAUD` | autodetect | serial baud |
| `--bed-size` / `S2M_BED_SIZE` | `220,220,250` | advertised X,Y,Z limits |

## Development

```bash
cargo test      # unit tests + a real-PTY serial integration test
cargo clippy
```

## Architecture

`src/state` is the single source of truth (one actor task, `watch`-published snapshots).
`src/transport` abstracts the byte stream (`serial.rs` real port, `mock.rs` in-process
Marlin sim). `src/serial_session` is the sole writer to the printer with a priority inbox
and the `ok` handshake. `src/klipper_api` is the UDS server (`codec.rs` 0x03 framing,
`dispatch.rs` methods, `subscribe.rs` delta logic). `src/gcode` translates commands and
`src/print_job` streams files to the printer.
