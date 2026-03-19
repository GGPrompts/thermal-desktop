//! Tool registry — maps tool names to handlers and provides definitions.

pub mod hyprland;
pub mod input;
pub mod screenshot;

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
