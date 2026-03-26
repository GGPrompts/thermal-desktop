//! Keyboard event to PTY byte encoding.
//!
//! Platform-agnostic key types and encoding logic. Converts a [`KeyCode`] +
//! [`Modifiers`] pair into the byte sequences that a terminal emulator sends
//! over a PTY.  Covers printable text, control characters, cursor keys,
//! editing keys, and function keys using standard xterm escape sequences.

// ── Key types ────────────────────────────────────────────────────────────────

/// Platform-agnostic key code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Enter,
    Backspace,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Char(char),
}

/// Modifier key state.
#[derive(Debug, Clone, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

// ── Encoding ─────────────────────────────────────────────────────────────────

/// Encode a key press into the bytes that should be written to the PTY.
///
/// Returns `None` for keys that have no PTY representation (e.g. bare
/// modifier presses).
pub fn encode_key(key: &KeyCode, mods: &Modifiers) -> Option<Vec<u8>> {
    // ── Ctrl+letter ────────────────────────────────────────────────────
    // Must be checked *before* the printable-text path so that
    // Ctrl+C emits 0x03 rather than the literal 'c'.
    if mods.ctrl {
        if let KeyCode::Char(ch) = key {
            if let Some(byte) = ctrl_char_byte(*ch) {
                return Some(vec![byte]);
            }
        }
    }

    // ── Special / non-printable keys ───────────────────────────────────
    if let Some(bytes) = encode_special(key) {
        return Some(bytes);
    }

    // ── Function keys ──────────────────────────────────────────────────
    if let Some(bytes) = encode_fkey(key) {
        return Some(bytes);
    }

    // ── Alt/Meta + key -> ESC prefix ────────────────────────────────────
    // When Alt is held (without Ctrl), prepend \x1b to the key byte.
    if mods.alt && !mods.ctrl {
        if let KeyCode::Char(ch) = key {
            let mut s = String::new();
            s.push(*ch);
            let mut bytes = Vec::with_capacity(1 + s.len());
            bytes.push(0x1b);
            bytes.extend_from_slice(s.as_bytes());
            return Some(bytes);
        }
    }

    // ── Printable text (no Ctrl held) ──────────────────────────────────
    if !mods.ctrl {
        if let KeyCode::Char(ch) = key {
            let mut s = String::new();
            s.push(*ch);
            return Some(s.into_bytes());
        }
    }

    None
}

// ── Ctrl + letter -> control byte ────────────────────────────────────────────

/// Map a character (a-z, A-Z) or one of the special Ctrl-combos (@, [, \, ],
/// ^, _) to the single control byte `(ascii_value & 0x1F)`.
fn ctrl_char_byte(ch: char) -> Option<u8> {
    let code = ch as u32;
    // Lowercase a-z
    if (0x61..=0x7a).contains(&code) {
        return Some((code as u8) & 0x1f);
    }
    // Uppercase A-Z -- same result after masking
    if (0x41..=0x5a).contains(&code) {
        return Some((code as u8) & 0x1f);
    }
    // Ctrl+@ -> NUL (0x00), Ctrl+[ -> ESC (0x1b), Ctrl+\ -> 0x1c,
    // Ctrl+] -> 0x1d, Ctrl+^ -> 0x1e, Ctrl+_ -> 0x1f
    match code {
        0x40 => Some(0x00), // @
        0x5b => Some(0x1b), // [
        0x5c => Some(0x1c), // backslash
        0x5d => Some(0x1d), // ]
        0x5e => Some(0x1e), // ^
        0x5f => Some(0x1f), // _
        _ => None,
    }
}

// ── Special keys -> escape sequences ─────────────────────────────────────────

fn encode_special(key: &KeyCode) -> Option<Vec<u8>> {
    let bytes: &[u8] = match key {
        KeyCode::Enter     => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab       => b"\t",
        KeyCode::Escape    => b"\x1b",

        // Cursor movement
        KeyCode::ArrowUp    => b"\x1b[A",
        KeyCode::ArrowDown  => b"\x1b[B",
        KeyCode::ArrowRight => b"\x1b[C",
        KeyCode::ArrowLeft  => b"\x1b[D",

        // Editing keys
        KeyCode::Home     => b"\x1b[H",
        KeyCode::End      => b"\x1b[F",
        KeyCode::Insert   => b"\x1b[2~",
        KeyCode::Delete   => b"\x1b[3~",
        KeyCode::PageUp   => b"\x1b[5~",
        KeyCode::PageDown => b"\x1b[6~",

        _ => return None,
    };
    Some(bytes.to_vec())
}

// ── Function keys -> escape sequences ────────────────────────────────────────

fn encode_fkey(key: &KeyCode) -> Option<Vec<u8>> {
    let seq: &[u8] = match key {
        KeyCode::F1  => b"\x1bOP",
        KeyCode::F2  => b"\x1bOQ",
        KeyCode::F3  => b"\x1bOR",
        KeyCode::F4  => b"\x1bOS",
        KeyCode::F5  => b"\x1b[15~",
        KeyCode::F6  => b"\x1b[17~",
        KeyCode::F7  => b"\x1b[18~",
        KeyCode::F8  => b"\x1b[19~",
        KeyCode::F9  => b"\x1b[20~",
        KeyCode::F10 => b"\x1b[21~",
        KeyCode::F11 => b"\x1b[23~",
        KeyCode::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn no_mods() -> Modifiers {
        Modifiers { ctrl: false, alt: false, shift: false }
    }

    fn ctrl() -> Modifiers {
        Modifiers { ctrl: true, ..no_mods() }
    }

    fn alt() -> Modifiers {
        Modifiers { alt: true, ..no_mods() }
    }

    fn ctrl_alt() -> Modifiers {
        Modifiers { ctrl: true, alt: true, ..no_mods() }
    }

    // ── Printable text ──────────────────────────────────────────────────

    #[test]
    fn printable_ascii() {
        assert_eq!(encode_key(&KeyCode::Char('a'), &no_mods()), Some(b"a".to_vec()));
    }

    #[test]
    fn printable_shifted() {
        let mods = Modifiers { shift: true, ..no_mods() };
        assert_eq!(encode_key(&KeyCode::Char('A'), &mods), Some(b"A".to_vec()));
    }

    #[test]
    fn printable_utf8_multibyte() {
        assert_eq!(
            encode_key(&KeyCode::Char('\u{00e9}'), &no_mods()),
            Some("\u{00e9}".as_bytes().to_vec())
        );
    }

    // ── Special keys ────────────────────────────────────────────────────

    #[test]
    fn enter_key() {
        assert_eq!(encode_key(&KeyCode::Enter, &no_mods()), Some(b"\r".to_vec()));
    }

    #[test]
    fn backspace_key() {
        assert_eq!(encode_key(&KeyCode::Backspace, &no_mods()), Some(b"\x7f".to_vec()));
    }

    #[test]
    fn tab_key() {
        assert_eq!(encode_key(&KeyCode::Tab, &no_mods()), Some(b"\t".to_vec()));
    }

    #[test]
    fn escape_key() {
        assert_eq!(encode_key(&KeyCode::Escape, &no_mods()), Some(b"\x1b".to_vec()));
    }

    // ── Arrow keys ──────────────────────────────────────────────────────

    #[test]
    fn arrow_up() {
        assert_eq!(encode_key(&KeyCode::ArrowUp, &no_mods()), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn arrow_down() {
        assert_eq!(encode_key(&KeyCode::ArrowDown, &no_mods()), Some(b"\x1b[B".to_vec()));
    }

    #[test]
    fn arrow_right() {
        assert_eq!(encode_key(&KeyCode::ArrowRight, &no_mods()), Some(b"\x1b[C".to_vec()));
    }

    #[test]
    fn arrow_left() {
        assert_eq!(encode_key(&KeyCode::ArrowLeft, &no_mods()), Some(b"\x1b[D".to_vec()));
    }

    // ── Editing keys ────────────────────────────────────────────────────

    #[test]
    fn home_key() {
        assert_eq!(encode_key(&KeyCode::Home, &no_mods()), Some(b"\x1b[H".to_vec()));
    }

    #[test]
    fn end_key() {
        assert_eq!(encode_key(&KeyCode::End, &no_mods()), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn delete_key() {
        assert_eq!(encode_key(&KeyCode::Delete, &no_mods()), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn insert_key() {
        assert_eq!(encode_key(&KeyCode::Insert, &no_mods()), Some(b"\x1b[2~".to_vec()));
    }

    #[test]
    fn page_up() {
        assert_eq!(encode_key(&KeyCode::PageUp, &no_mods()), Some(b"\x1b[5~".to_vec()));
    }

    #[test]
    fn page_down() {
        assert_eq!(encode_key(&KeyCode::PageDown, &no_mods()), Some(b"\x1b[6~".to_vec()));
    }

    // ── Ctrl + letter ───────────────────────────────────────────────────

    #[test]
    fn ctrl_c() {
        assert_eq!(encode_key(&KeyCode::Char('c'), &ctrl()), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_d() {
        assert_eq!(encode_key(&KeyCode::Char('d'), &ctrl()), Some(vec![0x04]));
    }

    #[test]
    fn ctrl_a() {
        assert_eq!(encode_key(&KeyCode::Char('a'), &ctrl()), Some(vec![0x01]));
    }

    #[test]
    fn ctrl_z() {
        assert_eq!(encode_key(&KeyCode::Char('z'), &ctrl()), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_l_uppercase() {
        assert_eq!(encode_key(&KeyCode::Char('L'), &ctrl()), Some(vec![0x0c]));
    }

    // ── Function keys ───────────────────────────────────────────────────

    #[test]
    fn f1() {
        assert_eq!(encode_key(&KeyCode::F1, &no_mods()), Some(b"\x1bOP".to_vec()));
    }

    #[test]
    fn f5() {
        assert_eq!(encode_key(&KeyCode::F5, &no_mods()), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn f12() {
        assert_eq!(encode_key(&KeyCode::F12, &no_mods()), Some(b"\x1b[24~".to_vec()));
    }

    // ── Ignored keys ────────────────────────────────────────────────────

    #[test]
    fn no_special_no_char_returns_none() {
        // A key code with no encoding and no char -- should return None.
        // Verify that Ctrl on a non-letter char returns None.
        assert_eq!(encode_key(&KeyCode::Char('\u{ffff}'), &ctrl()), None);
    }

    // ── Alt/Meta + key ──────────────────────────────────────────────────

    #[test]
    fn alt_b_word_back() {
        assert_eq!(encode_key(&KeyCode::Char('b'), &alt()), Some(vec![0x1b, b'b']));
    }

    #[test]
    fn alt_f_word_forward() {
        assert_eq!(encode_key(&KeyCode::Char('f'), &alt()), Some(vec![0x1b, b'f']));
    }

    #[test]
    fn alt_d_kill_word() {
        assert_eq!(encode_key(&KeyCode::Char('d'), &alt()), Some(vec![0x1b, b'd']));
    }

    #[test]
    fn alt_dot_last_arg() {
        assert_eq!(encode_key(&KeyCode::Char('.'), &alt()), Some(vec![0x1b, b'.']));
    }

    #[test]
    fn alt_uppercase_b() {
        let mods = Modifiers { alt: true, shift: true, ..no_mods() };
        assert_eq!(encode_key(&KeyCode::Char('B'), &mods), Some(vec![0x1b, b'B']));
    }

    #[test]
    fn ctrl_alt_does_not_add_esc_prefix() {
        // Ctrl+Alt+C -> Ctrl+C (0x03), not ESC+'c'
        assert_eq!(encode_key(&KeyCode::Char('c'), &ctrl_alt()), Some(vec![0x03]));
    }

    #[test]
    fn alt_non_char_returns_none() {
        // Alt held with a non-char key that has no special encoding -- but
        // our special keys DO encode, so test with a function key that
        // encodes regardless. Instead verify that Alt doesn't break F1.
        assert_eq!(encode_key(&KeyCode::F1, &alt()), Some(b"\x1bOP".to_vec()));
    }
}
