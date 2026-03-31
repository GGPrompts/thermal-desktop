# thermal-notify

GPU-rendered notification daemon implementing org.freedesktop.Notifications.

## What it does
Serves as a drop-in notification daemon via D-Bus. Renders thermal-styled
notification popups on a Wayland layer-shell overlay. Urgency is mapped to
heat level: low = blue, normal = green, critical = red. Plays notification
sounds via rodio (PipeWire-compatible).

## Socket / Pidfile
- Pidfile: `/run/user/$UID/thermal/notify.pid`
- D-Bus: owns `org.freedesktop.Notifications` on the session bus
- No Unix socket

## CLI Usage
```
thermal-notify [--volume <0-100>]
```
| Flag | Description |
|------|-------------|
| `--volume <0-100>` | Notification sound volume (default: 100) |

## Dependencies
- **Needs**: Wayland compositor with wlr-layer-shell, D-Bus session bus, PipeWire (for sound)
- **Needed by**: any application sending desktop notifications

## Troubleshooting
- If notifications do not appear, check that no other notification daemon (dunst, mako) is running
- Verify D-Bus ownership: `busctl --user list | grep Notifications`
- On NVIDIA, GPU context clashes may cause rendering issues
