#!/bin/bash
# Install Thermal Desktop systemd user services
# Symlinks service files from dotfiles into ~/.config/systemd/user/
# and enables the thermal.target

set -euo pipefail

DOTFILES="$HOME/projects/thermal-desktop/deploy/systemd"
TARGET="$HOME/.config/systemd/user"

mkdir -p "$TARGET"

echo "Linking service files..."
for f in "$DOTFILES"/*.{service,target,slice}; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    if [ -L "$TARGET/$name" ]; then
        echo "  skip $name (already linked)"
    elif [ -e "$TARGET/$name" ]; then
        echo "  WARN $name exists and is not a symlink — skipping"
    else
        ln -s "$f" "$TARGET/$name"
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
