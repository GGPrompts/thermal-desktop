//! Shared profile configuration for spawn profiles.
//!
//! Profiles are stored in `~/.config/thermal/profiles.toml` (or `./config/profiles.toml`
//! for development). This module provides types, loading, and saving.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Profile config types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileConfig {
    #[serde(default)]
    pub default_cwd: Option<String>,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default = "default_count")]
    pub count: u32,
    /// If true, create a git worktree per session to avoid file-edit conflicts.
    #[serde(default)]
    pub git_worktree: bool,
}

pub fn default_count() -> u32 {
    1
}

/// Expand `~/` prefix to $HOME.
pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

/// The path to the user config file.
pub fn user_config_path() -> String {
    expand_tilde("~/.config/thermal/profiles.toml")
}

/// Load profiles from config file. Search order:
/// 1. ./config/profiles.toml (dev)
/// 2. ~/.config/thermal/profiles.toml (user)
pub fn load_profiles() -> (Option<String>, Vec<Profile>) {
    let candidates = ["config/profiles.toml".to_string(), user_config_path()];

    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = toml::from_str::<ProfileConfig>(&content) {
                return (config.default_cwd, config.profiles);
            }
        }
    }

    // Fallback: single "Custom" profile
    (None, vec![fallback_profile()])
}

/// The default fallback profile when no config exists.
pub fn fallback_profile() -> Profile {
    Profile {
        name: "Custom".into(),
        command: None,
        cwd: None,
        icon: Some("\u{26a1}".into()), // lightning bolt
        count: 1,
        git_worktree: false,
    }
}

/// Save profiles to `~/.config/thermal/profiles.toml`.
/// Preserves any existing `default_cwd` from the loaded config.
pub fn save_profiles(default_cwd: Option<&str>, profiles: &[Profile]) -> Result<(), String> {
    let config = ProfileConfig {
        default_cwd: default_cwd.map(String::from),
        profiles: profiles.to_vec(),
    };

    let toml_str = toml::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize profiles: {}", e))?;

    let path = user_config_path();

    // Ensure parent directory exists.
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    std::fs::write(&path, toml_str).map_err(|e| format!("Failed to write {}: {}", path, e))?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_leaves_absolute_path_unchanged() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let result = expand_tilde("/absolute/path");
        assert_eq!(result, "/absolute/path");
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let result = expand_tilde("~/projects/foo");
        assert_eq!(result, "/home/testuser/projects/foo");
    }

    #[test]
    fn expand_tilde_leaves_tilde_only_unchanged() {
        let result = expand_tilde("~");
        assert_eq!(result, "~");
    }

    #[test]
    fn expand_tilde_leaves_relative_path_unchanged() {
        let result = expand_tilde("relative/path");
        assert_eq!(result, "relative/path");
    }

    #[test]
    fn expand_tilde_empty_string_unchanged() {
        let result = expand_tilde("");
        assert_eq!(result, "");
    }

    #[test]
    fn expand_tilde_deep_path() {
        unsafe {
            std::env::set_var("HOME", "/home/builder");
        }
        let result = expand_tilde("~/a/b/c/d.toml");
        assert_eq!(result, "/home/builder/a/b/c/d.toml");
    }

    fn parse_config(toml_str: &str) -> ProfileConfig {
        toml::from_str(toml_str).expect("TOML should parse")
    }

    #[test]
    fn toml_minimal_profile_required_field_only() {
        let t = r#"
[[profile]]
name = "My Profile"
"#;
        let cfg = parse_config(t);
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "My Profile");
        assert!(p.command.is_none());
        assert!(p.cwd.is_none());
        assert!(p.icon.is_none());
        assert_eq!(p.count, 1);
        assert!(!p.git_worktree);
    }

    #[test]
    fn toml_full_profile_all_fields() {
        let t = "[[profile]]\nname = \"Full Profile\"\ncommand = \"claude\"\ncwd = \"~/projects/myapp\"\nicon = \"\u{1f680}\"\ncount = 4\ngit_worktree = true\n";
        let cfg = parse_config(t);
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "Full Profile");
        assert_eq!(p.command.as_deref(), Some("claude"));
        assert_eq!(p.cwd.as_deref(), Some("~/projects/myapp"));
        assert_eq!(p.count, 4);
        assert!(p.git_worktree);
    }

    #[test]
    fn toml_multiple_profiles() {
        let t = r#"
[[profile]]
name = "Alpha"
count = 1

[[profile]]
name = "Beta"
count = 3
cwd = "/tmp"
"#;
        let cfg = parse_config(t);
        assert_eq!(cfg.profiles.len(), 2);
        assert_eq!(cfg.profiles[0].name, "Alpha");
        assert_eq!(cfg.profiles[1].name, "Beta");
        assert_eq!(cfg.profiles[1].count, 3);
        assert_eq!(cfg.profiles[1].cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn toml_default_cwd_field() {
        let t = r#"
default_cwd = "/srv/projects"

[[profile]]
name = "Dev"
"#;
        let cfg = parse_config(t);
        assert_eq!(cfg.default_cwd.as_deref(), Some("/srv/projects"));
        assert_eq!(cfg.profiles[0].name, "Dev");
    }

    #[test]
    fn toml_empty_profiles_list() {
        let t = "";
        let cfg: ProfileConfig = toml::from_str(t).expect("empty TOML should parse");
        assert!(cfg.profiles.is_empty());
        assert!(cfg.default_cwd.is_none());
    }

    #[test]
    fn toml_default_count_is_one() {
        assert_eq!(default_count(), 1);
    }

    #[test]
    fn toml_profile_with_tilde_cwd() {
        let t = r#"
[[profile]]
name = "Home"
cwd = "~/code"
"#;
        let cfg = parse_config(t);
        assert_eq!(cfg.profiles[0].cwd.as_deref(), Some("~/code"));
    }

    #[test]
    fn fallback_profile_has_expected_values() {
        let p = fallback_profile();
        assert_eq!(p.name, "Custom");
        assert_eq!(p.count, 1);
        assert!(!p.git_worktree);
    }

    #[test]
    fn round_trip_serialize_deserialize() {
        let profiles = vec![Profile {
            name: "Test".into(),
            command: Some("claude".into()),
            cwd: Some("~/projects".into()),
            icon: Some("\u{1f525}".into()),
            count: 2,
            git_worktree: true,
        }];
        let config = ProfileConfig {
            default_cwd: Some("/home/user".into()),
            profiles,
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: ProfileConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.profiles.len(), 1);
        assert_eq!(parsed.profiles[0].name, "Test");
        assert_eq!(parsed.default_cwd.as_deref(), Some("/home/user"));
    }
}
