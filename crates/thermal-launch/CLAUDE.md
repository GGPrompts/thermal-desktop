# thermal-launch

Fuzzy-search app launcher overlay — thermal targeting reticle aesthetic.

## What This Does
A Wayland layer-shell overlay that appears on keybind. Fuzzy-searches .desktop files and displays results with thermal styling. Targeting reticle UI.

## Architecture
- Wayland layer-shell surface (overlay layer)
- wgpu rendering with thermal gradient
- Reads .desktop files from XDG directories
- Fuzzy matching (nucleo or similar crate)
- Input handling for search + selection

## Development
```bash
cargo run -p thermal-launch
```
