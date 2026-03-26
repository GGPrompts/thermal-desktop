//! Keyboard event → PTY byte encoding.
//!
//! Converts SCTK [`KeyEvent`] + [`Modifiers`] into the byte sequences that a
//! terminal emulator sends over a PTY.  Covers printable text, control
//! characters, cursor keys, editing keys, and function keys using standard
//! xterm escape sequences.

use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

/// Encode an SCTK key‑press event into the bytes that should be written to
/// the PTY.
///
/// Returns `None` for keys that have no PTY representation (e.g. bare
/// modifier presses, Caps Lock, Num Lock, etc.).
pub fn encode_key(event: &KeyEvent, modifiers: &Modifiers) -> Option<Vec<u8>> {
    let sym = event.keysym;

    // ── Ctrl+letter  ────────────────────────────────────────────────────────
    // Must be checked *before* the utf8 / printable‑text path so that
    // Ctrl+C emits 0x03 rather than the literal 'c'.
    if modifiers.ctrl
        && let Some(byte) = ctrl_keysym_byte(sym)
    {
        return Some(vec![byte]);
    }

    // ── Special / non‑printable keys ────────────────────────────────────────
    if let Some(bytes) = encode_special(sym) {
        return Some(bytes);
    }

    // ── Function keys ───────────────────────────────────────────────────────
    if let Some(bytes) = encode_fkey(sym) {
        return Some(bytes);
    }

    // ── Alt/Meta + key → ESC prefix ─────────────────────────────────────────
    // When Alt is held (without Ctrl), prepend \x1b to the key byte.
    // This enables shell keybinds like Alt+B (word-back), Alt+F (word-forward),
    // and editor meta-key combos.
    if modifiers.alt && !modifiers.ctrl
        && let Some(ref text) = event.utf8
        && !text.is_empty()
    {
        let mut bytes = Vec::with_capacity(1 + text.len());
        bytes.push(0x1b);
        bytes.extend_from_slice(text.as_bytes());
        return Some(bytes);
    }

    // ── Printable text (no Ctrl held) ───────────────────────────────────────
    if !modifiers.ctrl
        && let Some(ref text) = event.utf8
        && !text.is_empty()
    {
        return Some(text.as_bytes().to_vec());
    }

    None
}

// ── Ctrl + letter → control byte ────────────────────────────────────────────

/// Map a Keysym that corresponds to a letter (a–z, A–Z) or one of the
/// special Ctrl‑combos (@, [, \, ], ^, _) to the single control byte
/// `(ascii_value & 0x1F)`.
fn ctrl_keysym_byte(sym: Keysym) -> Option<u8> {
    let raw: u32 = sym.into();
    // Lowercase a‑z  (Keysym 0x61..=0x7a)
    if (0x61..=0x7a).contains(&raw) {
        return Some((raw as u8) & 0x1f);
    }
    // Uppercase A‑Z  (Keysym 0x41..=0x5a) — same result after masking
    if (0x41..=0x5a).contains(&raw) {
        return Some((raw as u8) & 0x1f);
    }
    // Ctrl+@ → NUL (0x00), Ctrl+[ → ESC (0x1b), Ctrl+\ → 0x1c,
    // Ctrl+] → 0x1d, Ctrl+^ → 0x1e, Ctrl+_ → 0x1f
    match raw {
        0x40 => Some(0x00), // @
        0x5b => Some(0x1b), // [
        0x5c => Some(0x1c), // backslash
        0x5d => Some(0x1d), // ]
        0x5e => Some(0x1e), // ^
        0x5f => Some(0x1f), // _
        _ => None,
    }
}

// ── Special keys → escape sequences ─────────────────────────────────────────

fn encode_special(sym: Keysym) -> Option<Vec<u8>> {
    let bytes: &[u8] = match sym {
        Keysym::Return => b"\r",
        Keysym::BackSpace => b"\x7f",
        Keysym::Tab => b"\t",
        Keysym::Escape => b"\x1b",

        // Cursor movement
        Keysym::Up => b"\x1b[A",
        Keysym::Down => b"\x1b[B",
        Keysym::Right => b"\x1b[C",
        Keysym::Left => b"\x1b[D",

        // Editing keys
        Keysym::Home => b"\x1b[H",
        Keysym::End => b"\x1b[F",
        Keysym::Insert => b"\x1b[2~",
        Keysym::Delete => b"\x1b[3~",
        Keysym::Page_Up => b"\x1b[5~",
        Keysym::Page_Down => b"\x1b[6~",

        _ => return None,
    };
    Some(bytes.to_vec())
}

// ── Function keys → escape sequences ────────────────────────────────────────

fn encode_fkey(sym: Keysym) -> Option<Vec<u8>> {
    let seq: &[u8] = match sym {
        Keysym::F1 => b"\x1bOP",
        Keysym::F2 => b"\x1bOQ",
        Keysym::F3 => b"\x1bOR",
        Keysym::F4 => b"\x1bOS",
        Keysym::F5 => b"\x1b[15~",
        Keysym::F6 => b"\x1b[17~",
        Keysym::F7 => b"\x1b[18~",
        Keysym::F8 => b"\x1b[19~",
        Keysym::F9 => b"\x1b[20~",
        Keysym::F10 => b"\x1b[21~",
        Keysym::F11 => b"\x1b[23~",
        Keysym::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
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
        // Alt+B → ESC + 'b' (readline word-back)
        let ev = key(Keysym::new(0x62), Some("b"));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'b']));
    }

    #[test]
    fn alt_f_word_forward() {
        // Alt+F → ESC + 'f' (readline word-forward)
        let ev = key(Keysym::new(0x66), Some("f"));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'f']));
    }

    #[test]
    fn alt_d_kill_word() {
        // Alt+D → ESC + 'd' (readline kill-word)
        let ev = key(Keysym::new(0x64), Some("d"));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'd']));
    }

    #[test]
    fn alt_dot_last_arg() {
        // Alt+. → ESC + '.' (readline yank-last-arg)
        let ev = key(Keysym::new(0x2e), Some("."));
        assert_eq!(encode_key(&ev, &alt()), Some(vec![0x1b, b'.']));
    }

    #[test]
    fn alt_uppercase_b() {
        // Alt+Shift+B → ESC + 'B'
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
        // Ctrl+Alt+C → Ctrl+C (0x03), not ESC+'c'
        assert_eq!(encode_key(&ev, &ctrl_alt()), Some(vec![0x03]));
    }

    #[test]
    fn alt_no_utf8_returns_none() {
        // Alt held with a keysym that has no utf8 and no special encoding
        let ev = key(Keysym::new(0xffffff), None);
        assert_eq!(encode_key(&ev, &alt()), None);
    }
}
