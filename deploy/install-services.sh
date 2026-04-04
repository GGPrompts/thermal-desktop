#!/bin/bash
# Install Thermal Desktop deploy configs
# Symlinks systemd units, kitty, and hyprland configs into ~/.config/

set -euo pipefail

DEPLOY="$HOME/projects/thermal-desktop/deploy"

# ── App config symlinks ───────────────────────────────────
link_config() {
    local src="$1" dest="$2"
    mkdir -p "$(dirname "$dest")"
    if [ -L "$dest" ]; then
        echo "  skip $(basename "$dest") (already linked)"
    elif [ -e "$dest" ]; then
        echo "  WARN $dest exists and is not a symlink — skipping"
    else
        ln -s "$src" "$dest"
        echo "  link $(basename "$dest") -> $src"
    fi
}

echo "Linking app configs..."
link_config "$DEPLOY/kitty" "$HOME/.config/kitty"
link_config "$DEPLOY/hypr/hyprland.conf" "$HOME/.config/hypr/hyprland.conf"
link_config "$DEPLOY/hypr/hypridle.conf" "$HOME/.config/hypr/hypridle.conf"

# ── Systemd unit symlinks ─────────────────────────────────
SYSTEMD_TARGET="$HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_TARGET"

echo ""
echo "Linking systemd units..."
for f in "$DEPLOY/systemd"/*.{service,target,slice}; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    if [ -L "$SYSTEMD_TARGET/$name" ]; then
        echo "  skip $name (already linked)"
    elif [ -e "$SYSTEMD_TARGET/$name" ]; then
        echo "  WARN $name exists and is not a symlink — skipping"
    else
        ln -s "$f" "$SYSTEMD_TARGET/$name"
        echo "  link $name"
    fi
done

echo ""
echo "Reloading systemd user daemon..."
systemctl --user daemon-reload

echo ""
echo "Enabling thermal.target..."
systemctl --user enable thermal.target

# Enable core services (not dispatcher — that's opt-in)
CORE_SERVICES=(
    thermal-conductor.service
    thermal-audio.service
    swww-daemon.service
)

echo ""
echo "Enabling core services..."
for svc in "${CORE_SERVICES[@]}"; do
    systemctl --user enable "$svc" 2>/dev/null && echo "  enabled $svc" || echo "  skip $svc (already enabled or missing)"
done

echo ""
echo "Done. Optional services (enable manually if wanted):"
echo "  systemctl --user enable thermal-dispatcher.service"
echo ""
echo "To start now:  systemctl --user start thermal.target"
echo "To check:      systemctl --user status 'thermal-*'"
