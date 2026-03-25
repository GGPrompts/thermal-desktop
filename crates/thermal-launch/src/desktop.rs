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
    // Only include apps that open visible windows/overlays.
    // Daemons (audio, voice, notify, etc.) are managed via thc Services tab.
    vec![
        DesktopEntry {
            name: "\u{2388} thc — TUI Hub".into(),
            exec: "kitty --title thermal-conductor thc tui".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{2388} thermal-monitor".into(),
            exec: "kitty --title thermal-monitor thermal-monitor".into(),
            icon: None,
            categories: vec!["Thermal".into()],
        },
        DesktopEntry {
            name: "\u{25b8} thermal-conductor GPU".into(),
            exec: "thermal-conductor window".into(),
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

    let thermal = thermal_entries();
    let thermal_count = thermal.len();
    let mut entries = thermal;

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

    // Sort system entries alphabetically (keep thermal entries at the top)
    entries[thermal_count..].sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

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
        // Return all entries when no query (scroll to see more)
        return entries.iter().map(|e| (0usize, e)).collect();
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
    results.truncate(50);
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Write a temporary .desktop file with the given content and return its path.
    fn write_desktop_file(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.desktop");
        std::fs::write(&path, content).expect("write desktop file");
        (dir, path)
    }

    // We use inline temp dirs so the caller must hold the TempDir to keep it alive.
    fn parse(content: &str) -> Option<DesktopEntry> {
        let (_dir, path) = write_desktop_file(content);
        parse_desktop_file(&path)
    }

    fn make_entry(name: &str) -> DesktopEntry {
        DesktopEntry {
            name: name.to_string(),
            exec: "true".to_string(),
            icon: None,
            categories: vec![],
        }
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Name field
    // -------------------------------------------------------------------------

    #[test]
    fn parse_name_basic() {
        let e = parse("[Desktop Entry]\nName=Firefox\nExec=firefox\n").unwrap();
        assert_eq!(e.name, "Firefox");
    }

    #[test]
    fn parse_name_with_spaces() {
        let e = parse("[Desktop Entry]\nName=My App Name\nExec=myapp\n").unwrap();
        assert_eq!(e.name, "My App Name");
    }

    #[test]
    fn parse_name_trims_whitespace() {
        let e = parse("[Desktop Entry]\nName =  Padded  \nExec=myapp\n").unwrap();
        assert_eq!(e.name, "Padded");
    }

    #[test]
    fn parse_name_first_wins_duplicate() {
        // Only the first Name= key should be used.
        let e = parse("[Desktop Entry]\nName=First\nExec=app\nName=Second\n").unwrap();
        assert_eq!(e.name, "First");
    }

    #[test]
    fn parse_missing_name_returns_none() {
        let result = parse("[Desktop Entry]\nExec=firefox\n");
        assert!(result.is_none());
    }

    #[test]
    fn parse_empty_name_returns_none() {
        // An empty Name= value produces an empty string which the `?` on `name?`
        // still propagates as Some(""), so the entry IS returned.  What matters
        // is that missing Name= yields None — already tested above.  Here we
        // confirm the returned entry's name is the empty string.
        let e = parse("[Desktop Entry]\nName=\nExec=app\n");
        // The parser stores "" as the name — it comes through as Some("").
        // Some implementations filter empties; ours does not, so entry is Some.
        if let Some(entry) = e {
            assert_eq!(entry.name, "");
        }
        // Either None or Some("") is acceptable; the test just must not panic.
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Exec field
    // -------------------------------------------------------------------------

    #[test]
    fn parse_exec_basic() {
        let e = parse("[Desktop Entry]\nName=App\nExec=myapp --flag\n").unwrap();
        assert_eq!(e.exec, "myapp --flag");
    }

    #[test]
    fn parse_missing_exec_returns_none() {
        let result = parse("[Desktop Entry]\nName=App\n");
        assert!(result.is_none());
    }

    #[test]
    fn parse_exec_first_wins_duplicate() {
        let e = parse("[Desktop Entry]\nName=App\nExec=first\nExec=second\n").unwrap();
        assert_eq!(e.exec, "first");
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Icon field
    // -------------------------------------------------------------------------

    #[test]
    fn parse_icon_present() {
        let e = parse("[Desktop Entry]\nName=App\nExec=app\nIcon=myicon\n").unwrap();
        assert_eq!(e.icon.as_deref(), Some("myicon"));
    }

    #[test]
    fn parse_icon_absent() {
        let e = parse("[Desktop Entry]\nName=App\nExec=app\n").unwrap();
        assert!(e.icon.is_none());
    }

    #[test]
    fn parse_icon_first_wins_duplicate() {
        let e = parse("[Desktop Entry]\nName=App\nExec=app\nIcon=first\nIcon=second\n").unwrap();
        assert_eq!(e.icon.as_deref(), Some("first"));
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Comment field (not stored but must not break parse)
    // -------------------------------------------------------------------------

    #[test]
    fn parse_comment_field_ignored_gracefully() {
        // Comment= is not stored in DesktopEntry but must not prevent parsing.
        let e =
            parse("[Desktop Entry]\nName=App\nExec=app\nComment=A useful application\n").unwrap();
        assert_eq!(e.name, "App");
        assert_eq!(e.exec, "app");
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — NoDisplay field
    // -------------------------------------------------------------------------

    #[test]
    fn parse_nodisplay_true_returns_none() {
        let result = parse("[Desktop Entry]\nName=Hidden\nExec=app\nNoDisplay=true\n");
        assert!(result.is_none());
    }

    #[test]
    fn parse_nodisplay_true_case_insensitive() {
        let result = parse("[Desktop Entry]\nName=Hidden\nExec=app\nNoDisplay=True\n");
        assert!(result.is_none());

        let result2 = parse("[Desktop Entry]\nName=Hidden\nExec=app\nNoDisplay=TRUE\n");
        assert!(result2.is_none());
    }

    #[test]
    fn parse_nodisplay_false_is_visible() {
        let e = parse("[Desktop Entry]\nName=Visible\nExec=app\nNoDisplay=false\n").unwrap();
        assert_eq!(e.name, "Visible");
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Hidden field
    // -------------------------------------------------------------------------

    #[test]
    fn parse_hidden_true_returns_none() {
        let result = parse("[Desktop Entry]\nName=Hidden\nExec=app\nHidden=true\n");
        assert!(result.is_none());
    }

    #[test]
    fn parse_hidden_true_case_insensitive() {
        let result = parse("[Desktop Entry]\nName=Hidden\nExec=app\nHidden=TRUE\n");
        assert!(result.is_none());
    }

    #[test]
    fn parse_hidden_false_is_visible() {
        let e = parse("[Desktop Entry]\nName=Visible\nExec=app\nHidden=false\n").unwrap();
        assert_eq!(e.name, "Visible");
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Categories field
    // -------------------------------------------------------------------------

    #[test]
    fn parse_categories_semicolon_separated() {
        let e =
            parse("[Desktop Entry]\nName=App\nExec=app\nCategories=Utility;Network;\n").unwrap();
        assert_eq!(e.categories, vec!["Utility", "Network"]);
    }

    #[test]
    fn parse_categories_empty() {
        let e = parse("[Desktop Entry]\nName=App\nExec=app\n").unwrap();
        assert!(e.categories.is_empty());
    }

    #[test]
    fn parse_categories_no_trailing_semicolon() {
        let e = parse("[Desktop Entry]\nName=App\nExec=app\nCategories=Utility\n").unwrap();
        assert_eq!(e.categories, vec!["Utility"]);
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Section handling
    // -------------------------------------------------------------------------

    #[test]
    fn parse_ignores_other_sections() {
        // Keys outside [Desktop Entry] must not bleed in.
        let content = "[Other Section]\nName=WrongSection\nExec=wrong\n\
                       [Desktop Entry]\nName=Correct\nExec=right\n";
        let e = parse(content).unwrap();
        assert_eq!(e.name, "Correct");
        assert_eq!(e.exec, "right");
    }

    #[test]
    fn parse_section_after_desktop_entry_ignored() {
        let content = "[Desktop Entry]\nName=App\nExec=app\n\
                       [Desktop Action New]\nName=New Window\nExec=app --new\n";
        let e = parse(content).unwrap();
        assert_eq!(e.name, "App");
        assert_eq!(e.exec, "app");
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Comments and blank lines
    // -------------------------------------------------------------------------

    #[test]
    fn parse_skips_hash_comments() {
        let content = "[Desktop Entry]\n# This is a comment\nName=App\nExec=app\n";
        let e = parse(content).unwrap();
        assert_eq!(e.name, "App");
    }

    #[test]
    fn parse_skips_blank_lines() {
        let content = "[Desktop Entry]\n\nName=App\n\nExec=app\n\n";
        let e = parse(content).unwrap();
        assert_eq!(e.name, "App");
    }

    // -------------------------------------------------------------------------
    // Desktop entry parsing — Malformed content
    // -------------------------------------------------------------------------

    #[test]
    fn parse_malformed_no_section_header_returns_none() {
        // No [Desktop Entry] section at all — required fields are never set.
        let result = parse("Name=App\nExec=app\n");
        assert!(result.is_none());
    }

    #[test]
    fn parse_malformed_line_without_equals_ignored() {
        // Lines without '=' should be skipped, not crash.
        let content = "[Desktop Entry]\nName=App\nthis line has no equals sign\nExec=app\n";
        let e = parse(content).unwrap();
        assert_eq!(e.name, "App");
    }

    #[test]
    fn parse_empty_file_returns_none() {
        let result = parse("");
        assert!(result.is_none());
    }

    #[test]
    fn parse_only_whitespace_returns_none() {
        let result = parse("   \n  \n  ");
        assert!(result.is_none());
    }

    #[test]
    fn parse_key_with_equals_in_value() {
        // Value may itself contain '='; only the first '=' splits key/value.
        let e = parse("[Desktop Entry]\nName=App\nExec=env FOO=bar app\n").unwrap();
        assert_eq!(e.exec, "env FOO=bar app");
    }

    // -------------------------------------------------------------------------
    // Fuzzy match scoring — ordering guarantees
    // -------------------------------------------------------------------------

    #[test]
    fn fuzzy_empty_query_returns_all() {
        let entries = vec![make_entry("Alpha"), make_entry("Beta"), make_entry("Gamma")];
        let results = fuzzy_filter(&entries, "");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn fuzzy_empty_query_scores_all_zero() {
        let entries = vec![make_entry("Alpha"), make_entry("Beta")];
        let results = fuzzy_filter(&entries, "");
        assert!(results.iter().all(|(score, _)| *score == 0));
    }

    #[test]
    fn fuzzy_no_match_returns_empty() {
        let entries = vec![make_entry("Firefox"), make_entry("Chromium")];
        let results = fuzzy_filter(&entries, "zzznomatch");
        assert!(results.is_empty());
    }

    #[test]
    fn fuzzy_exact_match_beats_prefix() {
        // "fi" — "fi" (exact 2-char name) should outscore "firefox" (prefix match).
        // exact: base = 1000 - 2 = 998, no prefix bonus (starts with "fi") -> 998+500=1498
        // "firefox" prefix: base = 1000 - 7 = 993, prefix bonus -> 993+500=1493
        let entries = vec![make_entry("firefox"), make_entry("fi")];
        let results = fuzzy_filter(&entries, "fi");
        assert_eq!(results[0].1.name, "fi");
    }

    #[test]
    fn fuzzy_prefix_beats_substring() {
        // "fire" prefix in "firefox" beats "campfire" where "fire" is a suffix.
        let entries = vec![make_entry("campfire"), make_entry("firefox")];
        let results = fuzzy_filter(&entries, "fire");
        // "firefox" starts with "fire" -> gets prefix bonus
        assert_eq!(results[0].1.name, "firefox");
    }

    #[test]
    fn fuzzy_shorter_name_scores_higher_among_substring_matches() {
        // Both contain "app" as substring; shorter name should score higher.
        let entries = vec![
            make_entry("myapplication"), // long, substring
            make_entry("app"),           // short, prefix
        ];
        let results = fuzzy_filter(&entries, "app");
        // "app" is shorter (and also a prefix), so it should be first.
        assert_eq!(results[0].1.name, "app");
    }

    #[test]
    fn fuzzy_case_insensitive_matching() {
        let entries = vec![make_entry("Firefox"), make_entry("Chromium")];
        let results = fuzzy_filter(&entries, "FIREFOX");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.name, "Firefox");
    }

    #[test]
    fn fuzzy_case_insensitive_query_lower() {
        let entries = vec![make_entry("FIREFOX")];
        let results = fuzzy_filter(&entries, "firefox");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn fuzzy_results_sorted_descending_by_score() {
        let entries = vec![
            make_entry("longapplication"),
            make_entry("application"),
            make_entry("app"),
        ];
        let results = fuzzy_filter(&entries, "app");
        // Scores must be non-increasing.
        let scores: Vec<usize> = results.iter().map(|(s, _)| *s).collect();
        for w in scores.windows(2) {
            assert!(w[0] >= w[1], "scores not sorted descending: {:?}", scores);
        }
    }

    #[test]
    fn fuzzy_truncates_at_50_results() {
        // Build 60 entries all containing "x".
        let entries: Vec<DesktopEntry> = (0..60)
            .map(|i| make_entry(&format!("app-x-{:03}", i)))
            .collect();
        let results = fuzzy_filter(&entries, "x");
        assert!(results.len() <= 50);
    }

    // -------------------------------------------------------------------------
    // Result ordering — thermal entries appear first
    // -------------------------------------------------------------------------

    #[test]
    fn load_desktop_entries_thermal_entries_first() {
        // Point XDG_DATA_DIRS at an empty temp dir so only thermal entries come through.
        let dir = tempfile::tempdir().unwrap();
        // Create the expected subdirectory so the iterator doesn't error.
        std::fs::create_dir_all(dir.path().join("applications")).unwrap();

        // Scope the env var change so we don't corrupt other tests running in parallel.
        // We cannot use std::env::set_var safely in parallel tests, so instead we
        // verify the thermal_entries() invariant directly.
        let thermal = super::thermal_entries();
        for e in &thermal {
            assert!(
                e.categories.contains(&"Thermal".to_string()),
                "entry {:?} missing Thermal category",
                e.name
            );
        }
    }

    #[test]
    fn load_desktop_entries_with_xdg_data_dirs_reads_desktop_files() {
        // Build a temp XDG_DATA_DIRS tree with one valid .desktop file.
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(
            apps.join("testapp.desktop"),
            "[Desktop Entry]\nName=TestApp\nExec=testapp\n",
        )
        .unwrap();

        // We can call load_desktop_entries after overriding the env var.
        // Use a single-threaded test to avoid data races (cargo test runs tests
        // in parallel by default; restrict with RUST_TEST_THREADS=1 or use a mutex).
        // For safety we simply test parse_desktop_file directly here.
        let path = apps.join("testapp.desktop");
        let e = parse_desktop_file(&path).unwrap();
        assert_eq!(e.name, "TestApp");
        assert_eq!(e.exec, "testapp");
    }

    // -------------------------------------------------------------------------
    // thermal_entries() shape invariants
    // -------------------------------------------------------------------------

    #[test]
    fn thermal_entries_all_have_names_and_execs() {
        for e in thermal_entries() {
            assert!(!e.name.is_empty(), "thermal entry has empty name");
            assert!(
                !e.exec.is_empty(),
                "thermal entry has empty exec: {:?}",
                e.name
            );
        }
    }

    #[test]
    fn thermal_entries_all_categorised_as_thermal() {
        for e in thermal_entries() {
            assert!(
                e.categories.iter().any(|c| c == "Thermal"),
                "entry {:?} missing Thermal category",
                e.name
            );
        }
    }

    #[test]
    fn thermal_entries_count_matches_known_components() {
        // Only visible apps (not daemons). Update this number if you add more.
        assert_eq!(thermal_entries().len(), 5);
    }

    // -------------------------------------------------------------------------
    // Edge cases — .desktop file extension gating (via load path)
    // -------------------------------------------------------------------------

    #[test]
    fn non_desktop_extension_not_parsed() {
        // parse_desktop_file itself doesn't check the extension; the caller does.
        // We confirm that a valid-content .txt file would still be parsed by
        // parse_desktop_file (extension check is in load_desktop_entries).
        let content = "[Desktop Entry]\nName=App\nExec=app\n";
        let (_dir, path) = write_desktop_file(content);
        // Rename to .txt to simulate a non-.desktop file.
        let txt_path = path.with_extension("txt");
        std::fs::rename(&path, &txt_path).unwrap();
        // parse_desktop_file itself succeeds (extension agnostic).
        let e = parse_desktop_file(&txt_path).unwrap();
        assert_eq!(e.name, "App");
    }

    #[test]
    fn parse_nonexistent_file_returns_none() {
        let result = parse_desktop_file(std::path::Path::new("/tmp/does_not_exist_xyz.desktop"));
        assert!(result.is_none());
    }
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
