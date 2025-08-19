# Uart2Moon

A Rust application that creates a bridge between a 3D printer's USB UART interface and Moonraker's Unix socket, allowing modern tools like Mainsail to control legacy printers.

## Overview

Uart2Moon acts as a translation layer that:
- Creates a Unix socket that mimics Klipper's interface
- Connects to a 3D printer via USB UART
- Translates between Moonraker's JSON-RPC protocol and G-code commands
- Enables use of modern printer interfaces with legacy firmware

## Installation

1. Clone the repository:
   ```bash
   git clone https://github.com/Lukas-Heiligenbrunner/uart2moon.git
   cd uart2moon
   ```

2. Build the application:
   ```bash
   cargo build --release
   ```

## Usage

### Basic Usage
```bash
./target/release/Uart2Moon --device /dev/ttyUSB0 --socket-path /tmp/printer
```

### Command Line Options
- `--socket-path` (or `-s`): Path where the Unix socket will be created (default: `/tmp/printer`)
- `--device` (or `-d`): Serial device path (default: `/dev/ttyUSB0`)
- `--baud-rate` (or `-b`): Serial communication baud rate (default: `115200`)
- `--test-mode` (or `-t`): Run in test mode without connecting to serial device
- `--verbose` (or `-v`): Enable verbose logging

### Test Mode
For development and testing without hardware:
```bash
./target/release/Uart2Moon --test-mode --verbose
```

## Configuration with Moonraker

Configure Moonraker to use the Unix socket created by Uart2Moon by setting the `klippy_uds_address` in your `moonraker.conf`:

```ini
[server]
klippy_uds_address: /tmp/printer
```

## Supported Klipper Commands

The application currently supports these Klipper protocol methods:
- `info` - Returns printer information
- `objects/list` - Lists available printer objects  
- `objects/query` - Queries printer status
- `gcode/script` - Executes G-code commands
- `emergency_stop` - Emergency stop (sends M112)

## Protocol Translation

Uart2Moon translates between:
- **Input**: Moonraker's JSON-RPC over Unix socket
- **Output**: G-code commands over USB UART

Example translation:
```json
// Input from Moonraker
{"id": 1, "method": "gcode/script", "params": {"script": "G28"}}

// Translated to printer
G28

// Response from printer  
ok

// Response to Moonraker
{"id": 1, "result": {}, "error": null}
```

## Testing

Test the socket communication manually:
```bash
# Start uart2moon in test mode
./target/release/Uart2Moon --test-mode

# In another terminal, test commands
echo '{"id": 1, "method": "info"}' | nc -U /tmp/printer
echo '{"id": 2, "method": "gcode/script", "params": {"script": "G28"}}' | nc -U /tmp/printer
```

## Architecture

```
┌─────────────┐    Unix Socket    ┌─────────────┐    USB UART    ┌─────────────┐
│  Moonraker  │ ←─────────────── │ Uart2Moon   │ ─────────────→ │ 3D Printer  │
│  (Mainsail) │   JSON-RPC       │             │    G-code      │   Legacy FW │
└─────────────┘                  └─────────────┘                 └─────────────┘
```

## Requirements

- Rust 1.70+ 
- tokio runtime for async I/O
- Access to serial device (typically requires being in `dialout` group on Linux)

## License

[Add your license information here]