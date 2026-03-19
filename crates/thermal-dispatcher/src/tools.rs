//! Tool schema definitions for the Haiku system prompt.
//!
//! These mirror the tools available in thermal-commander and beads,
//! formatted as Anthropic tool-use schema objects.

use serde_json::{Value, json};

/// Build the complete list of tool schemas for the Anthropic API.
pub fn build_tool_schemas() -> Vec<Value> {
    let mut tools = Vec::new();

    // --- thermal-commander: Desktop control tools ---

    tools.push(tool(
        "screenshot",
        "Take a screenshot of the screen or a region. Returns a text description of what was captured.",
        json!({
            "type": "object",
            "properties": {
                "region": {
                    "type": "string",
                    "description": "Region to capture as \"X,Y WxH\" (e.g. \"100,200 800x600\"). Omit for full screen."
                }
            }
        }),
    ));

    tools.push(tool(
        "click",
        "Click the mouse at screen coordinates.",
        json!({
            "type": "object",
            "properties": {
                "x": { "type": "integer", "description": "X coordinate" },
                "y": { "type": "integer", "description": "Y coordinate" },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button (default: left)"
                }
            },
            "required": ["x", "y"]
        }),
    ));

    tools.push(tool(
        "type_text",
        "Type text using the keyboard.",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to type" }
            },
            "required": ["text"]
        }),
    ));

    tools.push(tool(
        "key_combo",
        "Press a keyboard shortcut like \"ctrl+s\", \"alt+tab\", \"super+1\".",
        json!({
            "type": "object",
            "properties": {
                "combo": {
                    "type": "string",
                    "description": "Key combination (e.g. \"ctrl+s\", \"alt+tab\")"
                }
            },
            "required": ["combo"]
        }),
    ));

    tools.push(tool(
        "scroll",
        "Scroll the mouse wheel at screen coordinates.",
        json!({
            "type": "object",
            "properties": {
                "x": { "type": "integer", "description": "X coordinate" },
                "y": { "type": "integer", "description": "Y coordinate" },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down"],
                    "description": "Scroll direction (default: up)"
                },
                "clicks": {
                    "type": "integer",
                    "description": "Number of scroll clicks (default: 3)"
                }
            },
            "required": ["x", "y"]
        }),
    ));

    // --- Window management ---

    tools.push(tool(
        "list_windows",
        "List all open windows with class, title, workspace, and focus state.",
        json!({ "type": "object", "properties": {} }),
    ));

    tools.push(tool(
        "focus_window",
        "Focus a window by class name, title substring, or Hyprland address.",
        json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "Window selector (class name, title, or address)"
                }
            },
            "required": ["selector"]
        }),
    ));

    tools.push(tool(
        "move_window",
        "Move a window to a pixel position or workspace.",
        json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "Window selector (class, title, or address)"
                },
                "x": { "type": "integer", "description": "Target X position" },
                "y": { "type": "integer", "description": "Target Y position" },
                "workspace": {
                    "type": "string",
                    "description": "Target workspace ID or name"
                }
            },
            "required": ["selector"]
        }),
    ));

    tools.push(tool(
        "list_workspaces",
        "List all Hyprland workspaces with IDs, names, and window counts.",
        json!({ "type": "object", "properties": {} }),
    ));

    tools.push(tool(
        "active_window",
        "Get information about the currently focused window.",
        json!({ "type": "object", "properties": {} }),
    ));

    // --- App launching ---

    tools.push(tool(
        "open_app",
        "Launch an application, optionally on a specific workspace.",
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command to execute (e.g. \"gimp\", \"obs\", \"spotify\")"
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace to launch on"
                }
            },
            "required": ["command"]
        }),
    ));

    tools.push(tool(
        "open_browser",
        "Open Firefox browser, optionally with a URL.",
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to open. Omit for a new window."
                }
            }
        }),
    ));

    tools.push(tool(
        "open_files",
        "Open the file manager, optionally at a specific path.",
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory or file path to open"
                }
            }
        }),
    ));

    tools.push(tool(
        "open_terminal",
        "Open a terminal, optionally with a working directory.",
        json!({
            "type": "object",
            "properties": {
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the terminal"
                }
            }
        }),
    ));

    // --- Claude orchestration ---

    tools.push(tool(
        "spawn_claude",
        "Spawn one or more Claude coding sessions via thermal-conductor.",
        json!({
            "type": "object",
            "properties": {
                "count": {
                    "type": "integer",
                    "description": "Number of sessions to spawn (default: 1)"
                },
                "project": {
                    "type": "string",
                    "description": "Project directory for the sessions"
                }
            }
        }),
    ));

    tools.push(tool(
        "claude_status",
        "Get status of all running Claude coding sessions.",
        json!({ "type": "object", "properties": {} }),
    ));

    tools.push(tool(
        "kill_claude",
        "Kill a Claude session by its session ID.",
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID to kill"
                }
            },
            "required": ["session_id"]
        }),
    ));

    // --- Utility ---

    tools.push(tool(
        "clipboard_get",
        "Get the 20 most recent clipboard history entries.",
        json!({ "type": "object", "properties": {} }),
    ));

    tools.push(tool(
        "clipboard_set",
        "Copy text to the clipboard.",
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to copy to clipboard"
                }
            },
            "required": ["text"]
        }),
    ));

    tools.push(tool(
        "notify",
        "Send a desktop notification.",
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Notification message text"
                },
                "urgency": {
                    "type": "string",
                    "enum": ["low", "normal", "critical"],
                    "description": "Urgency level (default: normal)"
                }
            },
            "required": ["message"]
        }),
    ));

    // --- Beads issue tracking ---

    tools.push(tool(
        "beads:list",
        "List beads issues, optionally filtered by status or project.",
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Filter by project prefix (e.g. \"therm\")"
                },
                "status": {
                    "type": "string",
                    "enum": ["open", "claimed", "ready", "blocked", "closed"],
                    "description": "Filter by status"
                }
            }
        }),
    ));

    tools.push(tool(
        "beads:show",
        "Show details of a specific bead/issue by ID.",
        json!({
            "type": "object",
            "properties": {
                "issue_id": {
                    "type": "string",
                    "description": "Bead issue ID (e.g. \"therm-abc1\")"
                }
            },
            "required": ["issue_id"]
        }),
    ));

    tools.push(tool(
        "beads:stats",
        "Show summary statistics for beads issues.",
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Filter by project prefix"
                }
            }
        }),
    ));

    tools.push(tool(
        "beads:create",
        "Create a new bead/issue.",
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Issue title"
                },
                "description": {
                    "type": "string",
                    "description": "Issue description"
                },
                "project": {
                    "type": "string",
                    "description": "Project prefix"
                }
            },
            "required": ["title"]
        }),
    ));

    tools.push(tool(
        "beads:close",
        "Close a bead/issue by ID.",
        json!({
            "type": "object",
            "properties": {
                "issue_id": {
                    "type": "string",
                    "description": "Bead issue ID to close"
                }
            },
            "required": ["issue_id"]
        }),
    ));

    tools.push(tool(
        "beads:ready",
        "Mark a bead/issue as ready (available for work).",
        json!({
            "type": "object",
            "properties": {
                "issue_id": {
                    "type": "string",
                    "description": "Bead issue ID to mark ready"
                }
            },
            "required": ["issue_id"]
        }),
    ));

    tools.push(tool(
        "beads:update",
        "Update a bead/issue title or description.",
        json!({
            "type": "object",
            "properties": {
                "issue_id": {
                    "type": "string",
                    "description": "Bead issue ID to update"
                },
                "title": {
                    "type": "string",
                    "description": "New title for the issue"
                },
                "description": {
                    "type": "string",
                    "description": "New description for the issue"
                }
            },
            "required": ["issue_id"]
        }),
    ));

    tools
}

/// Helper to create a tool schema in Anthropic's format.
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": input_schema,
    })
}
