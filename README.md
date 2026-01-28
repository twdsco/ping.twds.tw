# MTR Matrix

Real-time MTR (traceroute) monitoring from multiple source IPs to multiple destinations, displayed in a web-based matrix view.

<img width="1496" height="730" alt="截圖 2026-01-28 18 35 01" src="https://github.com/user-attachments/assets/0d1e556e-e518-4287-af10-4bd2b0b7291c" />

## Features

- **Multi-source MTR**: Run MTR from multiple source IPs simultaneously
- **Real-time updates**: WebSocket-based live updates
- **Matrix display**: Sources as columns, destinations as rows
- **Hover details**: TTL, IP, PTR (reverse DNS), latency stats, packet loss
- **Shared destinations**: Continuous monitoring to configured destinations
- **Custom destinations**: Users can add temporary destinations (60s duration)
- **Dynamic interface binding**: Automatically detects interface for each source IP

## Requirements

- Linux (uses raw sockets)
- Root privileges or `CAP_NET_RAW` capability
- Rust 1.70+

## Installation

```bash
cargo build --release
```

## Configuration

Edit `config.toml`:

```toml
custom_duration_secs = 60
max_custom_per_user = 10
max_custom_global = 75

[[sources]]
name = "source1"
ip = "192.168.1.1"

[[sources]]
name = "source2"
ip = "192.168.1.2"

[[destinations]]
host = "1.1.1.1"

[[destinations]]
host = "8.8.8.8"
```

## Network Setup

Run the setup script to configure IPs and routing:

```bash
sudo ./setup-network.sh
```

This script:
- Adds source IPs to the interface
- Configures policy routing via iptables marking
- Sets up the gateway route

## Running

```bash
sudo ./target/release/mtr-matrix
```

Access the web UI at `http://localhost:10000`

## Usage

- The matrix shows average latency to each destination from each source
- Hover over a cell to see detailed hop-by-hop information
- Use the input field to add custom destinations (limited per user and globally)
- "N/A" indicates the destination is unreachable from that source

## License

MIT
