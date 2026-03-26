//! Keyboard event -> PTY byte encoding (desktop / SCTK adapter).
//!
//! Converts SCTK [`KeyEvent`] + [`Modifiers`] into platform-agnostic
//! [`thermal_terminal::input::KeyCode`] + [`thermal_terminal::input::Modifiers`]
//! and delegates to the shared encoding logic.

use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

/// Encode an SCTK key-press event into the bytes that should be written to
/// the PTY.
///
/// Returns `None` for keys that have no PTY representation (e.g. bare
/// modifier presses, Caps Lock, Num Lock, etc.).
pub fn encode_key(event: &KeyEvent, modifiers: &Modifiers) -> Option<Vec<u8>> {
    // Convert SCTK Keysym to our platform-agnostic KeyCode.
    let key_code = keysym_to_keycode(event.keysym, event.utf8.as_deref())?;

    // Convert SCTK Modifiers to our platform-agnostic Modifiers.
    let mods = thermal_terminal::input::Modifiers {
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        shift: modifiers.shift,
    };

    thermal_terminal::input::encode_key(&key_code, &mods)
}

/// Convert an SCTK Keysym (+ optional utf8 text) to a platform-agnostic KeyCode.
///
/// Returns `None` for keys that have no KeyCode representation (bare modifiers,
/// unknown keysyms with no utf8 text).
fn keysym_to_keycode(
    sym: Keysym,
    utf8: Option<&str>,
) -> Option<thermal_terminal::input::KeyCode> {
    use thermal_terminal::input::KeyCode;

    // Check named keys first.
    let named = match sym {
        Keysym::Return    => Some(KeyCode::Enter),
        Keysym::BackSpace => Some(KeyCode::Backspace),
        Keysym::Tab       => Some(KeyCode::Tab),
        Keysym::Escape    => Some(KeyCode::Escape),
        Keysym::Up        => Some(KeyCode::ArrowUp),
        Keysym::Down      => Some(KeyCode::ArrowDown),
        Keysym::Right     => Some(KeyCode::ArrowRight),
        Keysym::Left      => Some(KeyCode::ArrowLeft),
        Keysym::Home      => Some(KeyCode::Home),
        Keysym::End       => Some(KeyCode::End),
        Keysym::Insert    => Some(KeyCode::Insert),
        Keysym::Delete    => Some(KeyCode::Delete),
        Keysym::Page_Up   => Some(KeyCode::PageUp),
        Keysym::Page_Down => Some(KeyCode::PageDown),
        Keysym::F1        => Some(KeyCode::F1),
        Keysym::F2        => Some(KeyCode::F2),
        Keysym::F3        => Some(KeyCode::F3),
        Keysym::F4        => Some(KeyCode::F4),
        Keysym::F5        => Some(KeyCode::F5),
        Keysym::F6        => Some(KeyCode::F6),
        Keysym::F7        => Some(KeyCode::F7),
        Keysym::F8        => Some(KeyCode::F8),
        Keysym::F9        => Some(KeyCode::F9),
        Keysym::F10       => Some(KeyCode::F10),
        Keysym::F11       => Some(KeyCode::F11),
        Keysym::F12       => Some(KeyCode::F12),
        _ => None,
    };

    if named.is_some() {
        return named;
    }

    // For letter/character keysyms, convert to KeyCode::Char.
    // First try to get the char from the keysym raw value (ASCII range).
    let raw: u32 = sym.into();
    if (0x20..=0x7e).contains(&raw) {
        return Some(KeyCode::Char(raw as u8 as char));
    }

    // Fall back to utf8 text from the event.
    if let Some(text) = utf8 {
        let mut chars = text.chars();
        if let Some(ch) = chars.next() {
            if chars.next().is_none() {
                // Single character.
                return Some(KeyCode::Char(ch));
            }
        }
    }

    None
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a KeyEvent with the given keysym and optional utf8 text.
    fn key(sym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent {
            time: 0,
            raw_code: 0,
            keysym: sym,
            utf8: utf8.map(String::from),
        }
    }

    /// Helper: default (no modifiers pressed).
    fn no_mods() -> Modifiers {
        Modifiers {
            ctrl: false,
            alt: false,
            shift: false,
            caps_lock: false,
            logo: false,
            num_lock: false,
        }
    }

    /// Helper: Ctrl held.
    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            ..no_mods()
        }
    }

    /// Helper: Alt held.
    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..no_mods()
        }
    }

    /// Helper: Ctrl+Alt held.
    fn ctrl_alt() -> Modifiers {
        Modifiers {
            ctrl: true,
            alt: true,
            ..no_mods()
        }
    }

    // ── Printable text ──────────────────────────────────────────────────────

    #[test]
    fn printable_ascii() {
        let ev = key(Keysym::new(0x61), Some("a")); // 'a'
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"a".to_vec()));
    }

    #[test]
    fn printable_shifted() {
        let ev = key(Keysym::new(0x41), Some("A")); // 'A'
        let mods = Modifiers {
            shift: true,
            ..no_mods()
        };
        assert_eq!(encode_key(&ev, &mods), Some(b"A".to_vec()));
    }

    #[test]
    fn printable_utf8_multibyte() {
        // e.g. a compose sequence producing a unicode character
        let ev = key(Keysym::new(0x01000000 | 0x00e9), Some("\u{00e9}")); // 'e' with acute
        assert_eq!(
            encode_key(&ev, &no_mods()),
            Some("\u{00e9}".as_bytes().to_vec())
        );
    }

    // ── Special keys ────────────────────────────────────────────────────────

    #[test]
    fn enter_key() {
        let ev = key(Keysym::Return, Some("\r"));
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\r".to_vec()));
    }

    #[test]
    fn backspace_key() {
        let ev = key(Keysym::BackSpace, Some("\x08"));
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x7f".to_vec()));
    }

    #[test]
    fn tab_key() {
        let ev = key(Keysym::Tab, Some("\t"));
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\t".to_vec()));
    }

    #[test]
    fn escape_key() {
        let ev = key(Keysym::Escape, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b".to_vec()));
    }

    // ── Arrow keys ──────────────────────────────────────────────────────────

    #[test]
    fn arrow_up() {
        let ev = key(Keysym::Up, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn arrow_down() {
        let ev = key(Keysym::Down, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[B".to_vec()));
    }

    #[test]
    fn arrow_right() {
        let ev = key(Keysym::Right, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[C".to_vec()));
    }

    #[test]
    fn arrow_left() {
        let ev = key(Keysym::Left, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[D".to_vec()));
    }

    // ── Editing keys ────────────────────────────────────────────────────────

    #[test]
    fn home_key() {
        let ev = key(Keysym::Home, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[H".to_vec()));
    }

    #[test]
    fn end_key() {
        let ev = key(Keysym::End, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn delete_key() {
        let ev = key(Keysym::Delete, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn insert_key() {
        let ev = key(Keysym::Insert, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[2~".to_vec()));
    }

    #[test]
    fn page_up() {
        let ev = key(Keysym::Page_Up, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[5~".to_vec()));
    }

    #[test]
    fn page_down() {
        let ev = key(Keysym::Page_Down, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[6~".to_vec()));
    }

    // ── Ctrl + letter ───────────────────────────────────────────────────────

    #[test]
    fn ctrl_c() {
        let ev = key(Keysym::new(0x63), Some("c")); // 'c'
        assert_eq!(encode_key(&ev, &ctrl()), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_d() {
        let ev = key(Keysym::new(0x64), Some("d")); // 'd'
        assert_eq!(encode_key(&ev, &ctrl()), Some(vec![0x04]));
    }

    #[test]
    fn ctrl_a() {
        let ev = key(Keysym::new(0x61), Some("a")); // 'a'
        assert_eq!(encode_key(&ev, &ctrl()), Some(vec![0x01]));
    }

    #[test]
    fn ctrl_z() {
        let ev = key(Keysym::new(0x7a), Some("z")); // 'z'
        assert_eq!(encode_key(&ev, &ctrl()), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_l_uppercase() {
        // Some keyboard layouts report uppercase keysym even with Ctrl
        let ev = key(Keysym::new(0x4c), Some("L")); // 'L'
        assert_eq!(encode_key(&ev, &ctrl()), Some(vec![0x0c]));
    }

    // ── Function keys ───────────────────────────────────────────────────────

    #[test]
    fn f1() {
        let ev = key(Keysym::F1, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1bOP".to_vec()));
    }

    #[test]
    fn f5() {
        let ev = key(Keysym::F5, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn f12() {
        let ev = key(Keysym::F12, None);
        assert_eq!(encode_key(&ev, &no_mods()), Some(b"\x1b[24~".to_vec()));
    }

    // ── Ignored keys ────────────────────────────────────────────────────────

    #[test]
    fn bare_modifier_returns_none() {
        // Shift_L keysym = 0xffe1
        let ev = key(Keysym::new(0xffe1), None);
        assert_eq!(encode_key(&ev, &no_mods()), None);
    }

    #[test]
    fn no_utf8_no_special_returns_none() {
        // Some unknown keysym with no utf8 text
        let ev = key(Keysym::new(0xffffff), None);
        assert_eq!(encode_key(&ev, &no_mods()), None);
    }

    // ── Alt/Meta + key ───────────────────────────────────────────────────

    #[test]
    fn alt_b_word_back() {
        // Alt+B -> ESC + 'b' (readline word-back)
        let ev = key(Keysym::new(0x62), Some("b"));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'b']));
    }

    #[test]
    fn alt_f_word_forward() {
        // Alt+F -> ESC + 'f' (readline word-forward)
        let ev = key(Keysym::new(0x66), Some("f"));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'f']));
    }

    #[test]
    fn alt_d_kill_word() {
        // Alt+D -> ESC + 'd' (readline kill-word)
        let ev = key(Keysym::new(0x64), Some("d"));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'd']));
    }

    #[test]
    fn alt_dot_last_arg() {
        // Alt+. -> ESC + '.' (readline yank-last-arg)
        let ev = key(Keysym::new(0x2e), Some("."));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'.']));
    }

    #[test]
    fn alt_uppercase_b() {
        // Alt+Shift+B -> ESC + 'B'
        let ev = key(Keysym::new(0x42), Some("B"));
        let mods = Modifiers {
            alt: true,
            shift: true,
            ..no_mods()
        };
        assert_eq!(encode_key(&ev, &mods), Some(vec![0x1b, b'B']));
    }

    #[test]
    fn ctrl_alt_does_not_add_esc_prefix() {
        // Ctrl+Alt should NOT use the Alt ESC-prefix path; Ctrl takes priority.
        let ev = key(Keysym::new(0x63), Some("c"));
        // Ctrl+Alt+C -> Ctrl+C (0x03), not ESC+'c'
        assert_eq!(encode_key(&ev, &ctrl_alt()), Some(vec![0x03]));
    }

    #[test]
    fn alt_no_utf8_returns_none() {
        // Alt held with a keysym that has no utf8 and no special encoding
        let ev = key(Keysym::new(0xffffff), None);
        assert_eq!(encode_key(&ev, &alt()), None);
    }
}
