//! Trust tier configuration.
//!
//! Loads tool-name-to-tier mappings from a TOML file. Tools not listed
//! in the config default to [`TrustTier::Confirm`].

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Execution policy for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustTier {
    /// Execute immediately — no user confirmation required.
    Auto,
    /// Show action plan on HUD, wait for user confirmation.
    Confirm,
    /// Reject outright, announce via thermal-audio.
    Block,
}

/// Parsed trust tier configuration.
pub struct TrustConfig {
    tiers: HashMap<String, TrustTier>,
}

/// Raw TOML file shape.
#[derive(Deserialize)]
struct TrustConfigFile {
    #[serde(default)]
    tiers: HashMap<String, String>,
}

/// Build a `TrustConfig` directly from an in-memory TOML string (test helper).
#[cfg(test)]
pub fn trust_config_from_str(content: &str) -> Result<TrustConfig> {
    let file: TrustConfigFile =
        toml::from_str(content).with_context(|| "parsing in-memory toml")?;
    let mut tiers = HashMap::new();
    for (tool_name, tier_str) in file.tiers {
        let tier = match tier_str.to_uppercase().as_str() {
            "AUTO" => TrustTier::Auto,
            "CONFIRM" => TrustTier::Confirm,
            "BLOCK" => TrustTier::Block,
            _ => TrustTier::Confirm,
        };
        tiers.insert(tool_name, tier);
    }
    Ok(TrustConfig { tiers })
}

impl TrustConfig {
    /// Load trust tier config from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: TrustConfigFile =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

        let mut tiers = HashMap::new();
        for (tool_name, tier_str) in file.tiers {
            let tier = match tier_str.to_uppercase().as_str() {
                "AUTO" => TrustTier::Auto,
                "CONFIRM" => TrustTier::Confirm,
                "BLOCK" => TrustTier::Block,
                other => {
                    tracing::warn!(
                        tool = %tool_name,
                        tier = %other,
                        "unknown trust tier, defaulting to CONFIRM"
                    );
                    TrustTier::Confirm
                }
            };
            tiers.insert(tool_name, tier);
        }

        Ok(Self { tiers })
    }

    /// Look up the trust tier for a tool. Defaults to [`TrustTier::Confirm`].
    pub fn tier_for(&self, tool_name: &str) -> TrustTier {
        self.tiers
            .get(tool_name)
            .copied()
            .unwrap_or(TrustTier::Confirm)
    }

    /// Number of explicitly configured tool mappings.
    pub fn tier_count(&self) -> usize {
        self.tiers.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // --- helpers ---

    fn config_from(toml: &str) -> TrustConfig {
        trust_config_from_str(toml).expect("parse failed")
    }

    // -----------------------------------------------------------------------
    // TrustTier TOML parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_auto_tier() {
        let cfg = config_from(r#"[tiers]\nscreenshot = "AUTO""#.replace("\\n", "\n").as_str());
        assert_eq!(cfg.tier_for("screenshot"), TrustTier::Auto);
    }

    #[test]
    fn parse_confirm_tier() {
        let cfg = config_from("[tiers]\nclick = \"CONFIRM\"");
        assert_eq!(cfg.tier_for("click"), TrustTier::Confirm);
    }

    #[test]
    fn parse_block_tier() {
        let cfg = config_from("[tiers]\nkill_claude = \"BLOCK\"");
        assert_eq!(cfg.tier_for("kill_claude"), TrustTier::Block);
    }

    #[test]
    fn tier_strings_are_case_insensitive() {
        let cfg = config_from("[tiers]\na = \"auto\"\nb = \"confirm\"\nc = \"block\"");
        assert_eq!(cfg.tier_for("a"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("b"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("c"), TrustTier::Block);
    }

    #[test]
    fn unknown_tier_string_defaults_to_confirm() {
        let cfg = config_from("[tiers]\nwhatever = \"BANANA\"");
        assert_eq!(cfg.tier_for("whatever"), TrustTier::Confirm);
    }

    #[test]
    fn empty_tiers_section_parses() {
        let cfg = config_from("[tiers]");
        assert_eq!(cfg.tier_count(), 0);
    }

    #[test]
    fn missing_tiers_section_parses() {
        let cfg = config_from("# no tiers here");
        assert_eq!(cfg.tier_count(), 0);
    }

    #[test]
    fn tier_count_matches_number_of_entries() {
        let cfg = config_from(
            "[tiers]\n\
             screenshot = \"AUTO\"\n\
             click = \"CONFIRM\"\n\
             kill_claude = \"BLOCK\"",
        );
        assert_eq!(cfg.tier_count(), 3);
    }

    // -----------------------------------------------------------------------
    // Tool classification: default for unknown tool
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_tool_defaults_to_confirm() {
        let cfg = config_from("[tiers]");
        assert_eq!(cfg.tier_for("totally_unknown_tool"), TrustTier::Confirm);
    }

    // -----------------------------------------------------------------------
    // Namespaced tools (beads:* style)
    // -----------------------------------------------------------------------

    #[test]
    fn namespaced_beads_tools_parse_as_auto() {
        let cfg = config_from(
            "[tiers]\n\
             \"beads:list\" = \"AUTO\"\n\
             \"beads:show\" = \"AUTO\"\n\
             \"beads:stats\" = \"AUTO\"\n\
             \"beads:create\" = \"AUTO\"\n\
             \"beads:update\" = \"AUTO\"\n\
             \"beads:close\" = \"AUTO\"\n\
             \"beads:claim\" = \"AUTO\"\n\
             \"beads:ready\" = \"AUTO\"\n\
             \"beads:blocked\" = \"AUTO\"\n\
             \"beads:reopen\" = \"AUTO\"",
        );
        for name in &[
            "beads:list",
            "beads:show",
            "beads:stats",
            "beads:create",
            "beads:update",
            "beads:close",
            "beads:claim",
            "beads:ready",
            "beads:blocked",
            "beads:reopen",
        ] {
            assert_eq!(cfg.tier_for(name), TrustTier::Auto, "{name} should be AUTO");
        }
    }

    #[test]
    fn namespaced_tool_not_in_config_defaults_to_confirm() {
        let cfg = config_from("[tiers]");
        assert_eq!(cfg.tier_for("beads:delete"), TrustTier::Confirm);
    }

    // -----------------------------------------------------------------------
    // Loading from the real config file on disk
    // -----------------------------------------------------------------------

    #[test]
    fn load_real_config_file() {
        // CARGO_MANIFEST_DIR points to crates/thermal-dispatcher, so two
        // parents up is the workspace root.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config_path = manifest
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("config/trust-tiers.toml"))
            .expect("workspace root not found");

        let cfg = TrustConfig::load(&config_path).expect("load failed");

        // Known AUTO tools from the real file
        assert_eq!(cfg.tier_for("screenshot"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("list_windows"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("active_window"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("list_workspaces"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("claude_status"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("clipboard_get"), TrustTier::Auto);

        // Known CONFIRM tools
        assert_eq!(cfg.tier_for("click"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("type_text"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("key_combo"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("scroll"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("focus_window"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("move_window"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("open_app"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("open_browser"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("open_files"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("open_terminal"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("clipboard_set"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("notify"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("spawn_claude"), TrustTier::Confirm);

        // Known BLOCK tools
        assert_eq!(cfg.tier_for("kill_claude"), TrustTier::Block);

        // Beads tools (all AUTO in real file)
        assert_eq!(cfg.tier_for("beads:list"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("beads:show"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("beads:close"), TrustTier::Auto);
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = TrustConfig::load(std::path::Path::new("/nonexistent/path/trust.toml"));
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // TrustTier derives: Copy, Clone, PartialEq
    // -----------------------------------------------------------------------

    #[test]
    fn trust_tier_copy() {
        let a = TrustTier::Auto;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn trust_tier_clone() {
        let a = TrustTier::Block;
        assert_eq!(a.clone(), TrustTier::Block);
    }

    #[test]
    fn trust_tier_debug() {
        assert!(format!("{:?}", TrustTier::Auto).contains("Auto"));
        assert!(format!("{:?}", TrustTier::Confirm).contains("Confirm"));
        assert!(format!("{:?}", TrustTier::Block).contains("Block"));
    }

    // -----------------------------------------------------------------------
    // Multiple tools in one TOML blob
    // -----------------------------------------------------------------------

    #[test]
    fn full_toml_blob_parses_correctly() {
        let toml = r#"
[tiers]
screenshot     = "AUTO"
list_windows   = "AUTO"
click          = "CONFIRM"
type_text      = "CONFIRM"
kill_claude    = "BLOCK"
"beads:list"   = "AUTO"
"beads:close"  = "AUTO"
"#;
        let cfg = config_from(toml);
        assert_eq!(cfg.tier_count(), 7);
        assert_eq!(cfg.tier_for("screenshot"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("click"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("kill_claude"), TrustTier::Block);
        assert_eq!(cfg.tier_for("beads:list"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("beads:close"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("not_listed"), TrustTier::Confirm);
    }
}
