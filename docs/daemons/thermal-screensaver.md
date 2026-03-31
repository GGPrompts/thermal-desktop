# thermal-screensaver

Idle-triggered thermal fluid simulation overlay.

## What it does
Activates after a configurable idle timeout using the ext-idle-notify-v1
Wayland protocol. Renders a fullscreen reaction-diffusion WGSL shader on a
wlr-layer-shell overlay. Any keyboard or mouse input dismisses it. Includes
a 5-minute watchdog for keyboard grab release to mitigate NVIDIA DPMS issues.

## Socket / Pidfile
- Pidfile: `/run/user/$UID/thermal/screensaver.pid`
- No socket (activates via Wayland idle protocol)

## CLI Usage
```
thermal-screensaver [--timeout <SECONDS>]
```
| Flag | Description |
|------|-------------|
| `--timeout <SECONDS>` | Idle timeout before activation (default: 300) |

## Dependencies
- **Needs**: Wayland compositor with ext-idle-notify-v1 and wlr-layer-shell
- **Needed by**: nothing (standalone visual component)

## Troubleshooting
- If screensaver does not activate, verify ext-idle-notify-v1 support: `wayland-info | grep idle`
- On NVIDIA, DPMS resume can cause hangs; hypridle uses brightness 0 as a workaround
- If keyboard grab is stuck, the 5-minute watchdog will auto-release
