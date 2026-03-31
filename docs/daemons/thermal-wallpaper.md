# thermal-wallpaper

Animated WGSL thermal shader wallpaper daemon.

## What it does
Renders a simplex-noise heat field on the desktop background via
wlr-layer-shell. The shader is modulated by real-time system metrics:
low CPU/memory load produces cool blue drifting noise, high load produces
hot red turbulence. Uses the thermal gradient LUT from thermal-core.

## Socket / Pidfile
- Pidfile: `/run/user/$UID/thermal/wallpaper.pid`
- No socket (renders directly via Wayland)

## CLI Usage
```
thermal-wallpaper
```
No flags.

## Dependencies
- **Needs**: Wayland compositor with wlr-layer-shell, wgpu-compatible GPU
- **Needed by**: nothing (standalone visual component)

## Troubleshooting
- If wallpaper is black or missing, check wlr-layer-shell support
- On NVIDIA after DPMS resume, `conn.flush()` errors are non-fatal and self-recover
- High GPU usage: the shader runs continuously; check with `nvidia-smi` or `intel_gpu_top`
