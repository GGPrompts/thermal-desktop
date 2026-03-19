# Thermal Desktop — Hotkeys

**Super = Windows key (mod)**

## Core
| Key | Action |
|-----|--------|
| Super + Enter | New terminal (kitty) |
| Super + Q | Close window |
| Super + D | App launcher (thermal-launch) |
| Super + E | File manager (thunar) |
| Super + F | Fullscreen |
| Super + V | Toggle floating |
| Super + M | Exit Hyprland |

## Navigation
| Key | Action |
|-----|--------|
| Super + h/j/k/l | Move focus (vim-style) |
| Super + Arrow keys | Move focus |
| Super + 1-0 | Switch workspace 1-10 |
| Super + ` | Next workspace |
| Super + Shift + ` | Previous workspace |
| Super + Mouse scroll | Cycle workspaces |

## Window Management
| Key | Action |
|-----|--------|
| Super + Shift + h/j/k/l | Move window |
| Super + Shift + Arrow keys | Move window |
| Super + Ctrl + Arrow keys | Resize window |
| Super + Hold left-click | Drag window |
| Super + Hold right-click | Resize window |
| Super + Shift + 1-0 | Move window to workspace |
| Super + Shift + 0 | Stash window to workspace 10 |
| Super + 0 | Check stashed windows |
| Super + P | Pseudo-tile |
| Super + J | Toggle split |

## Thermal Tools
| Key | Action |
|-----|--------|
| Super + D | Launcher with thermal components at top |
| Super + B | btop (system monitor) |
| Super + T | thermal-status (quick readout) |
| Super + N | Notification center (swaync) |
| Super + / | Cheatsheet |
| Super + \ | Push-to-talk voice input |

## Screenshots
| Key | Action |
|-----|--------|
| Print | Region select + swappy annotation |
| Shift + Print | Full screen + swappy |

## thermal-conductor Window
These work inside a `thermal-conductor window`:

| Key | Action |
|-----|--------|
| Ctrl + Shift + T | Toggle agent timeline bar |
| Ctrl + Shift + Enter | Inject selection to other windows |
| Ctrl + Shift + N | Spawn continuation session |
| Ctrl + Shift + Q | Close conductor window |
| Ctrl + Shift + C | Copy selection |
| Ctrl + Shift + V | Paste |
| Shift + PageUp/Down | Scroll through history |
| Shift + Home/End | Jump to top/bottom of scrollback |

## Disabled
| Key | Action | Reason |
|-----|--------|--------|
| Super + L | Lock screen (thermal-lock) | NVIDIA GPU context clash |
