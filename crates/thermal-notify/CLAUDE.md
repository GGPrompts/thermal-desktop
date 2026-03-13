# thermal-notify

D-Bus notification daemon with thermal urgency mapping.

## What This Does
Implements org.freedesktop.Notifications D-Bus interface. Renders thermal-styled notification popups. Urgency mapped to heat level: low=blue, normal=green, critical=red.

## Architecture
- zbus D-Bus server implementing Notifications spec
- Wayland layer-shell surface (overlay layer) for popup rendering
- wgpu rendering with thermal palette
- Auto-dismiss with configurable timeout
- PipeWire audio for notification sounds via rodio

## Development
```bash
cargo run -p thermal-notify
```
