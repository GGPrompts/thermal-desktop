//! Tool registry — maps tool names to handlers and provides definitions.

pub mod claude;
pub mod hyprland;
pub mod input;
pub mod launcher;
pub mod screenshot;
pub mod utility;

use anyhow::Result;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;

use crate::mcp::{ToolDefinition, ToolResult};

/// Type alias for async tool handler functions.
type ToolFn = fn(Value) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send>>;

/// A registered tool with its definition and handler.
struct RegisteredTool {
    definition: ToolDefinition,
    handler: ToolFn,
}

/// The tool registry. Holds all available tools.
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

impl ToolRegistry {
    /// Build the registry with all tools.
    pub fn new() -> Self {
        let mut registry = Self { tools: Vec::new() };
        registry.register_all();
        registry
    }

    /// Get all tool definitions for tools/list.
    pub fn definitions(&self) -> Vec<&ToolDefinition> {
        self.tools.iter().map(|t| &t.definition).collect()
    }

    /// Call a tool by name.
    pub async fn call(&self, name: &str, args: Value) -> Result<ToolResult> {
        for tool in &self.tools {
            if tool.definition.name == name {
                return (tool.handler)(args).await;
            }
        }
        Ok(ToolResult::error(format!("unknown tool: {name}")))
    }

    fn register_all(&mut self) {
        self.register(
            "screenshot",
            "Take a screenshot of the screen. Returns the image as base64-encoded PNG. Optionally capture a specific region.",
            json!({
                "type": "object",
                "properties": {
                    "region": {
                        "type": "string",
                        "description": "Region to capture as \"X,Y WxH\" (e.g. \"100,200 800x600\"). Omit for full screen."
                    }
                }
            }),
            |args| Box::pin(screenshot::screenshot(args)),
        );

        self.register(
            "click",
            "Click the mouse at screen coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "X coordinate"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Y coordinate"
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "Mouse button (default: left)"
                    }
                },
                "required": ["x", "y"]
            }),
            |args| Box::pin(input::click(args)),
        );

        self.register(
            "type_text",
            "Type text using the keyboard. Types the exact string provided.",
            json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to type"
                    }
                },
                "required": ["text"]
            }),
            |args| Box::pin(input::type_text(args)),
        );

        self.register(
            "key_combo",
            "Press a keyboard shortcut. Supports modifier+key combos like \"ctrl+s\", \"alt+tab\", \"super+1\".",
            json!({
                "type": "object",
                "properties": {
                    "combo": {
                        "type": "string",
                        "description": "Key combination (e.g. \"ctrl+s\", \"alt+tab\", \"super+1\", \"ctrl+shift+t\")"
                    }
                },
                "required": ["combo"]
            }),
            |args| Box::pin(input::key_combo(args)),
        );

        self.register(
            "scroll",
            "Scroll the mouse wheel at screen coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "X coordinate"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Y coordinate"
                    },
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
            |args| Box::pin(input::scroll(args)),
        );

        self.register(
            "list_windows",
            "List all open windows with their class, title, workspace, position, size, and focus state.",
            json!({
                "type": "object",
                "properties": {}
            }),
            |args| Box::pin(hyprland::list_windows(args)),
        );

        self.register(
            "focus_window",
            "Focus (bring to front) a window by class name, title, or Hyprland address.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "Window selector: class name (e.g. \"kitty\"), title substring, or address (e.g. \"address:0x...\")"
                    }
                },
                "required": ["selector"]
            }),
            |args| Box::pin(hyprland::focus_window(args)),
        );

        self.register(
            "move_window",
            "Move a window to a pixel position or workspace.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "Window selector (class, title, or address)"
                    },
                    "x": {
                        "type": "integer",
                        "description": "Target X position (for pixel move)"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Target Y position (for pixel move)"
                    },
                    "workspace": {
                        "type": "string",
                        "description": "Target workspace ID or name (e.g. \"2\", \"special:scratchpad\")"
                    }
                },
                "required": ["selector"]
            }),
            |args| Box::pin(hyprland::move_window(args)),
        );

        self.register(
            "list_workspaces",
            "List all Hyprland workspaces with their IDs, names, and window counts.",
            json!({
                "type": "object",
                "properties": {}
            }),
            |args| Box::pin(hyprland::list_workspaces(args)),
        );

        self.register(
            "active_window",
            "Get information about the currently focused window.",
            json!({
                "type": "object",
                "properties": {}
            }),
            |args| Box::pin(hyprland::active_window(args)),
        );

        // --- App launching tools ---

        self.register(
            "open_app",
            "Launch an application via Hyprland, optionally on a specific workspace.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command to execute (e.g. \"gimp\", \"obs\", \"spotify\")"
                    },
                    "workspace": {
                        "type": "string",
                        "description": "Workspace to move the app to after launching (e.g. \"2\", \"special:scratchpad\")"
                    }
                },
                "required": ["command"]
            }),
            |args| Box::pin(launcher::open_app(args)),
        );

        self.register(
            "open_browser",
            "Open Firefox browser, optionally with a URL.",
            json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to open (e.g. \"https://example.com\"). Omit to open a new window."
                    }
                }
            }),
            |args| Box::pin(launcher::open_browser(args)),
        );

        self.register(
            "open_files",
            "Open Thunar file manager, optionally at a specific path.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory or file path to open (e.g. \"/home/user/Documents\")"
                    }
                }
            }),
            |args| Box::pin(launcher::open_files(args)),
        );

        self.register(
            "open_terminal",
            "Open a kitty terminal, optionally with a working directory.",
            json!({
                "type": "object",
                "properties": {
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the terminal (e.g. \"/home/user/projects\")"
                    }
                }
            }),
            |args| Box::pin(launcher::open_terminal(args)),
        );

        // --- Claude orchestration tools ---

        self.register(
            "spawn_claude",
            "Spawn one or more Claude coding sessions via thermal-conductor.",
            json!({
                "type": "object",
                "properties": {
                    "count": {
                        "type": "integer",
                        "description": "Number of Claude sessions to spawn (default: 1)"
                    },
                    "project": {
                        "type": "string",
                        "description": "Project directory to associate with the sessions"
                    }
                }
            }),
            |args| Box::pin(claude::spawn_claude(args)),
        );

        self.register(
            "claude_status",
            "Get status of all running Claude sessions.",
            json!({
                "type": "object",
                "properties": {}
            }),
            |args| Box::pin(claude::claude_status(args)),
        );

        self.register(
            "kill_claude",
            "Kill a Claude session by its session ID.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to kill (get IDs from claude_status)"
                    }
                },
                "required": ["session_id"]
            }),
            |args| Box::pin(claude::kill_claude(args)),
        );

        // --- Utility tools ---

        self.register(
            "clipboard_get",
            "Get the 20 most recent clipboard history entries via cliphist.",
            json!({
                "type": "object",
                "properties": {}
            }),
            |args| Box::pin(utility::clipboard_get(args)),
        );

        self.register(
            "clipboard_set",
            "Copy text to the Wayland clipboard via wl-copy.",
            json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to copy to the clipboard"
                    }
                },
                "required": ["text"]
            }),
            |args| Box::pin(utility::clipboard_set(args)),
        );

        self.register(
            "notify",
            "Send a desktop notification via notify-send.",
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
                        "description": "Notification urgency level (default: normal)"
                    }
                },
                "required": ["message"]
            }),
            |args| Box::pin(utility::notify(args)),
        );
    }

    fn register(
        &mut self,
        name: &str,
        description: &str,
        input_schema: Value,
        handler: ToolFn,
    ) {
        self.tools.push(RegisteredTool {
            definition: ToolDefinition {
                name: name.into(),
                description: description.into(),
                input_schema,
            },
            handler,
        });
    }
}
