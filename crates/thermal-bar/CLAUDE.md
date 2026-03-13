# thermal-bar

Wayland layer-shell status bar — a FLIR instrument panel.

## What This Does
A GPU-rendered status bar anchored to the top of the screen via wlr-layer-shell. Shows real-time system data (CPU temp, GPU temp, memory, network) and thermal-conductor agent status overview.

## Architecture
- Wayland layer-shell surface (top layer, exclusive zone)
- wgpu rendering with glyphon text
- Reads /sys/class/thermal/, /proc/stat for system metrics
- D-Bus client to thermal-conductor for agent status
- Configurable modules (left/center/right)

## Development
```bash
cargo run -p thermal-bar
```
