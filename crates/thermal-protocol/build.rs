use ggl_build::{RustCodegenConfig, TypeOverrideConfig};
use std::collections::HashMap;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();

    let mut type_overrides = HashMap::new();

    // Most types get PartialEq + Eq + Hash via per-type overrides (not global)
    // because SessionState contains Option<f64> which doesn't support Eq/Hash.
    let eq_hash = TypeOverrideConfig {
        extra_derives: vec!["PartialEq".into(), "Eq".into(), "Hash".into()],
        extra_attributes: vec![],
    };
    for name in [
        "AgentId",
        "TaskState",
        "ToolArgs",
        "ToolDetails",
        "Layout",
        "AgentState",
        "PaneInfo",
        "ConductorConfig",
    ] {
        type_overrides.insert(name.into(), eq_hash.clone());
    }

    // ClaudeStatus: JSON values are snake_case ("tool_use", "awaiting_input").
    // Default impl (Idle) is in ggl_types.rs because ggl doesn't emit #[default].
    type_overrides.insert(
        "ClaudeStatus".into(),
        TypeOverrideConfig {
            extra_derives: vec!["PartialEq".into(), "Eq".into(), "Hash".into()],
            extra_attributes: vec![r#"#[serde(rename_all = "snake_case")]"#.into()],
        },
    );

    // SessionState: needs #[serde(default)] so missing fields deserialize to
    // Default values (the poller reads partial JSON from state files).
    // No Eq/Hash because context_percent is Option<f64>.
    type_overrides.insert(
        "SessionState".into(),
        TypeOverrideConfig {
            extra_derives: vec![],
            extra_attributes: vec![r#"#[serde(default)]"#.into()],
        },
    );

    let config = RustCodegenConfig {
        extra_derives: vec![],
        // No serde tag needed — TaskState unit variants work without it,
        // and AgentId is a struct (tag only applies to enums).
        enum_serde_tag: None,
        type_overrides,
        ..Default::default()
    };
    ggl_build::build_ggl_with_config("schemas", &out_dir, &config).expect("ggl codegen failed");
}
