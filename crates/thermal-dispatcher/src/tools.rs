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

    tools.push(tool(
        "system_metrics",
        "Get current system resource usage: CPU load, RAM, and GPU utilization/memory. Useful for capacity-aware agent scheduling.",
        json!({ "type": "object", "properties": {} }),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Tool list shape
    // -----------------------------------------------------------------------

    #[test]
    fn build_tool_schemas_returns_nonempty_list() {
        let tools = build_tool_schemas();
        assert!(!tools.is_empty(), "tool list must not be empty");
    }

    #[test]
    fn every_tool_has_name_description_input_schema() {
        let tools = build_tool_schemas();
        for t in &tools {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            assert!(!name.is_empty(), "tool has no name: {t}");
            assert!(
                t.get("description").and_then(|v| v.as_str()).is_some(),
                "tool '{name}' missing description"
            );
            assert!(
                t.get("input_schema").is_some(),
                "tool '{name}' missing input_schema"
            );
        }
    }

    #[test]
    fn input_schema_has_type_object() {
        let tools = build_tool_schemas();
        for t in &tools {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let schema_type = t
                .get("input_schema")
                .and_then(|s| s.get("type"))
                .and_then(|v| v.as_str());
            assert_eq!(
                schema_type,
                Some("object"),
                "tool '{name}' input_schema.type should be \"object\""
            );
        }
    }

    // -----------------------------------------------------------------------
    // Specific desktop tool names present
    // -----------------------------------------------------------------------

    fn tool_names() -> Vec<String> {
        build_tool_schemas()
            .into_iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect()
    }

    #[test]
    fn desktop_tools_present() {
        let names = tool_names();
        for expected in &[
            "screenshot",
            "click",
            "type_text",
            "key_combo",
            "scroll",
            "list_windows",
            "focus_window",
            "move_window",
            "list_workspaces",
            "active_window",
            "open_app",
            "open_browser",
            "open_files",
            "open_terminal",
            "spawn_claude",
            "claude_status",
            "kill_claude",
            "clipboard_get",
            "clipboard_set",
            "notify",
            "system_metrics",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing tool: {expected}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Beads namespaced tool names present
    // -----------------------------------------------------------------------

    #[test]
    fn beads_tools_present() {
        let names = tool_names();
        for expected in &[
            "beads:list",
            "beads:show",
            "beads:stats",
            "beads:create",
            "beads:close",
            "beads:ready",
            "beads:update",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing beads tool: {expected}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Beads tool colon-namespace parsing (name format)
    // -----------------------------------------------------------------------

    #[test]
    fn beads_tool_names_contain_colon() {
        let names = tool_names();
        let beads_tools: Vec<&String> = names.iter().filter(|n| n.starts_with("beads:")).collect();
        assert!(
            !beads_tools.is_empty(),
            "expected at least one beads: namespaced tool"
        );
        for name in &beads_tools {
            let parts: Vec<&str> = name.splitn(2, ':').collect();
            assert_eq!(
                parts.len(),
                2,
                "beads tool '{name}' should split into 2 parts"
            );
            assert_eq!(parts[0], "beads");
            assert!(!parts[1].is_empty(), "beads subcommand should not be empty");
        }
    }

    // -----------------------------------------------------------------------
    // Required fields in specific tool schemas
    // -----------------------------------------------------------------------

    fn find_tool(name: &str) -> Value {
        build_tool_schemas()
            .into_iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
            .unwrap_or_else(|| panic!("tool '{name}' not found"))
    }

    fn required_fields(tool: &Value) -> Vec<String> {
        tool.get("input_schema")
            .and_then(|s| s.get("required"))
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn click_requires_x_and_y() {
        let t = find_tool("click");
        let req = required_fields(&t);
        assert!(req.contains(&"x".to_string()), "click should require x");
        assert!(req.contains(&"y".to_string()), "click should require y");
    }

    #[test]
    fn type_text_requires_text() {
        let t = find_tool("type_text");
        let req = required_fields(&t);
        assert!(
            req.contains(&"text".to_string()),
            "type_text should require text"
        );
    }

    #[test]
    fn key_combo_requires_combo() {
        let t = find_tool("key_combo");
        let req = required_fields(&t);
        assert!(req.contains(&"combo".to_string()));
    }

    #[test]
    fn focus_window_requires_selector() {
        let t = find_tool("focus_window");
        let req = required_fields(&t);
        assert!(req.contains(&"selector".to_string()));
    }

    #[test]
    fn open_app_requires_command() {
        let t = find_tool("open_app");
        let req = required_fields(&t);
        assert!(req.contains(&"command".to_string()));
    }

    #[test]
    fn kill_claude_requires_session_id() {
        let t = find_tool("kill_claude");
        let req = required_fields(&t);
        assert!(req.contains(&"session_id".to_string()));
    }

    #[test]
    fn clipboard_set_requires_text() {
        let t = find_tool("clipboard_set");
        let req = required_fields(&t);
        assert!(req.contains(&"text".to_string()));
    }

    #[test]
    fn notify_requires_message() {
        let t = find_tool("notify");
        let req = required_fields(&t);
        assert!(req.contains(&"message".to_string()));
    }

    #[test]
    fn beads_show_requires_issue_id() {
        let t = find_tool("beads:show");
        let req = required_fields(&t);
        assert!(req.contains(&"issue_id".to_string()));
    }

    #[test]
    fn beads_close_requires_issue_id() {
        let t = find_tool("beads:close");
        let req = required_fields(&t);
        assert!(req.contains(&"issue_id".to_string()));
    }

    #[test]
    fn beads_ready_requires_issue_id() {
        let t = find_tool("beads:ready");
        let req = required_fields(&t);
        assert!(req.contains(&"issue_id".to_string()));
    }

    #[test]
    fn beads_create_requires_title() {
        let t = find_tool("beads:create");
        let req = required_fields(&t);
        assert!(req.contains(&"title".to_string()));
    }

    #[test]
    fn beads_update_requires_issue_id() {
        let t = find_tool("beads:update");
        let req = required_fields(&t);
        assert!(req.contains(&"issue_id".to_string()));
    }

    // -----------------------------------------------------------------------
    // Optional-field tools have no required array (or empty)
    // -----------------------------------------------------------------------

    #[test]
    fn screenshot_has_no_required_fields() {
        let t = find_tool("screenshot");
        let req = required_fields(&t);
        assert!(req.is_empty(), "screenshot should have no required fields");
    }

    #[test]
    fn list_windows_has_no_required_fields() {
        let t = find_tool("list_windows");
        let req = required_fields(&t);
        assert!(req.is_empty());
    }

    #[test]
    fn system_metrics_has_no_required_fields() {
        let t = find_tool("system_metrics");
        let req = required_fields(&t);
        assert!(req.is_empty());
    }

    #[test]
    fn beads_list_has_no_required_fields() {
        let t = find_tool("beads:list");
        let req = required_fields(&t);
        assert!(req.is_empty(), "beads:list should have no required fields");
    }

    // -----------------------------------------------------------------------
    // Property definitions exist for key fields
    // -----------------------------------------------------------------------

    fn properties(tool: &Value) -> Vec<String> {
        tool.get("input_schema")
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object())
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default()
    }

    #[test]
    fn scroll_has_direction_and_clicks_properties() {
        let t = find_tool("scroll");
        let props = properties(&t);
        assert!(props.contains(&"direction".to_string()));
        assert!(props.contains(&"clicks".to_string()));
    }

    #[test]
    fn notify_has_urgency_property() {
        let t = find_tool("notify");
        let props = properties(&t);
        assert!(props.contains(&"urgency".to_string()));
    }

    #[test]
    fn beads_list_has_project_and_status_properties() {
        let t = find_tool("beads:list");
        let props = properties(&t);
        assert!(props.contains(&"project".to_string()));
        assert!(props.contains(&"status".to_string()));
    }

    // -----------------------------------------------------------------------
    // Enum constraints on click.button
    // -----------------------------------------------------------------------

    #[test]
    fn click_button_enum_contains_left_right_middle() {
        let t = find_tool("click");
        let enum_vals = t
            .get("input_schema")
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get("button"))
            .and_then(|b| b.get("enum"))
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(enum_vals.contains(&"left".to_string()));
        assert!(enum_vals.contains(&"right".to_string()));
        assert!(enum_vals.contains(&"middle".to_string()));
    }

    // -----------------------------------------------------------------------
    // Tool schemas are valid JSON (no panic on serialization round-trip)
    // -----------------------------------------------------------------------

    #[test]
    fn tool_schemas_round_trip_json() {
        let tools = build_tool_schemas();
        let serialized = serde_json::to_string(&tools).expect("serialization failed");
        let deserialized: Vec<Value> =
            serde_json::from_str(&serialized).expect("deserialization failed");
        assert_eq!(tools.len(), deserialized.len());
    }
}
