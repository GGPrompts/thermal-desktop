//! Desktop entry parser — reads XDG .desktop files and returns app metadata.

use std::fs;

/// Metadata parsed from a .desktop file.
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
    pub icon: Option<String>,
    pub categories: Vec<String>,
}

/// Built-in thermal desktop component entries.
///
/// These always appear at the top of the launcher so thermal tools
/// are always one keystroke away.
fn thermal_entries() -> Vec<DesktopEntry> {
    vec![
        DesktopEntry {
            name: "\u{2388} thermal-monitor".into(),
            exec: "kitty --title thermal-monitor thermal-monitor".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{25b8} thermal-conductor".into(),
            exec: "thermal-conductor window".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{2581} thermal-bar".into(),
            exec: "pkill -x thermal-bar; sleep 0.3; thermal-bar".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{25a3} thermal-hud".into(),
            exec: "thermal-hud".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{266b} thermal-audio".into(),
            exec: "thermal-audio".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{1f512} thermal-lock".into(),
            exec: "thermal-lock".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
    ]
}

/// Read and parse .desktop files from XDG_DATA_DIRS.
///
/// Falls back to `/usr/local/share:/usr/share` if the env var is not set.
/// Built-in thermal entries are prepended so they always appear first.
pub fn load_desktop_entries() -> Vec<DesktopEntry> {
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());

    let mut entries = thermal_entries();

    for dir in data_dirs.split(':') {
        let apps_dir = format!("{}/applications", dir);
        let read_dir = match fs::read_dir(&apps_dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }

            if let Some(de) = parse_desktop_file(&path) {
                entries.push(de);
            }
        }
    }

    entries
}

/// Fuzzy-filter desktop entries by query string.
///
/// Returns up to 8 `(score, entry)` pairs sorted descending by score.
/// Matching is case-insensitive substring: shorter names score higher,
/// with a bonus for exact prefix matches.
pub fn fuzzy_filter<'a>(
    entries: &'a [DesktopEntry],
    query: &str,
) -> Vec<(usize, &'a DesktopEntry)> {
    if query.is_empty() {
        // Return first 8 entries with equal score when no query
        return entries.iter().take(8).map(|e| (0usize, e)).collect();
    }

    let query_lower = query.to_lowercase();
    let mut results: Vec<(usize, &DesktopEntry)> = entries
        .iter()
        .filter_map(|entry| {
            let name_lower = entry.name.to_lowercase();
            if name_lower.contains(&query_lower) {
                // Score: base = 1000 - name.len() (shorter = higher)
                let base: usize = 1000usize.saturating_sub(entry.name.len());
                // Prefix bonus: +500 if name starts with the query
                let prefix_bonus = if name_lower.starts_with(&query_lower) {
                    500
                } else {
                    0
                };
                Some((base + prefix_bonus, entry))
            } else {
                None
            }
        })
        .collect();

    // Sort descending by score
    results.sort_by(|a, b| b.0.cmp(&a.0));
    results.truncate(8);
    results
}

/// Parse a single .desktop file. Returns `None` if required fields are missing
/// or the entry should not be displayed.
fn parse_desktop_file(path: &std::path::Path) -> Option<DesktopEntry> {
    let content = fs::read_to_string(path).ok()?;

    let mut in_desktop_entry = false;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();
    let mut no_display = false;
    let mut hidden = false;

    for line in content.lines() {
        let line = line.trim();

        // Section headers
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on first '='
        let Some(eq_pos) = line.find('=') else {
            continue;
        };
        let key = line[..eq_pos].trim();
        let value = line[eq_pos + 1..].trim();

        match key {
            "Name" => {
                if name.is_none() {
                    name = Some(value.to_string());
                }
            }
            "Exec" => {
                if exec.is_none() {
                    exec = Some(value.to_string());
                }
            }
            "Icon" => {
                if icon.is_none() {
                    icon = Some(value.to_string());
                }
            }
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
            }
            "NoDisplay" => {
                no_display = value.eq_ignore_ascii_case("true");
            }
            "Hidden" => {
                hidden = value.eq_ignore_ascii_case("true");
            }
            // Localised Name variants take priority if they exist
            k if k.starts_with("Name[") => {
                // Only override if we haven't set a localised version yet — keep first match
            }
            _ => {}
        }
    }

    if no_display || hidden {
        return None;
    }

    Some(DesktopEntry {
        name: name?,
        exec: exec?,
        icon,
        categories,
    })
}
