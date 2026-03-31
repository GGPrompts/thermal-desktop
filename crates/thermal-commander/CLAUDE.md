# thermal-commander

MCP server for Wayland/Hyprland desktop control.

## What This Does
JSON-RPC 2.0 over stdio. Provides pane capture (`kitty @ get-text`), click, type, window management, system metrics. Used by thermal-messages for `@system` route and by thermal-dispatcher for `read()` tool.
