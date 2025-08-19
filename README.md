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

## Development Environment

A complete development environment with Moonraker and Mainsail is available using Docker Compose. This setup runs uart2moon in test mode (no real serial device required) and provides a full web interface for testing.

### Quick Start

1. Validate your setup (optional):
   ```bash
   ./test-env.sh
   ```

2. Start the development environment:
   ```bash
   docker-compose up -d
   ```

3. Access the interfaces:
   - **Mainsail Web Interface**: http://localhost:8080
   - **Moonraker API**: http://localhost:7125

4. Stop the environment:
   ```bash
   docker-compose down
   ```

### What's Included

- **uart2moon**: Runs in test mode, creating a mock printer interface
- **Moonraker**: Provides the JSON-RPC API that Mainsail uses
- **Mainsail**: Modern web interface for printer control

### Testing the Setup

You can test the connection by sending commands through Mainsail or directly to the Moonraker API:

```bash
# Test via Moonraker API
curl -X POST http://localhost:7125/printer/gcode/script \
     -H "Content-Type: application/json" \
     -d '{"script": "G28"}'

# Check printer status
curl http://localhost:7125/printer/info
```

### Development Workflow

1. Make changes to the Rust code
2. Rebuild the container:
   ```bash
   docker-compose build uart2moon
   docker-compose up -d
   ```
3. Test changes through the Mainsail interface

### Troubleshooting

- **Mainsail shows "Printer not connected"**: Wait a few seconds for uart2moon to start and create the socket
- **Port conflicts**: Modify the ports in `docker-compose.yml` if 8080 or 7125 are already in use
- **Build failures**: Ensure you have Docker and Docker Compose installed

## License

[Add your license information here]