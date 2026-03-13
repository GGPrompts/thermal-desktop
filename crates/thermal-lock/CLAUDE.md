# thermal-lock

GPU-rendered lock screen with thermal heat-map effect.

## What This Does
A lock screen using ext-session-lock-v1 Wayland protocol. GPU-rendered heat-map shader over blurred screenshot. Custom auth UI with thermal styling.

## Architecture
- ext-session-lock-v1 protocol for secure locking
- wgpu fragment shader for heat-map effect
- PAM authentication
- Display: time (green readout), date (muted), auth input

## Development
```bash
cargo run -p thermal-lock
```
