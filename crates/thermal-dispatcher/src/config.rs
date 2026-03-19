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
