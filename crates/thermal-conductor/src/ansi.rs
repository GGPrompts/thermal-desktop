/// Represents a styled character from terminal output
#[derive(Debug, Clone)]
pub struct StyledChar {
    pub ch: char,
    pub fg: AnsiColor,
    pub bg: AnsiColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum AnsiColor {
    Default,
    Indexed(u8),         // 0-255
    Rgb(u8, u8, u8),     // 24-bit
}

/// Parse a string with ANSI escapes into styled characters
/// This is a simplified parser for rendering captured tmux output
pub fn parse_ansi_styled(input: &str) -> Vec<Vec<StyledChar>> {
    let mut lines: Vec<Vec<StyledChar>> = Vec::new();
    let mut current_line: Vec<StyledChar> = Vec::new();

    let mut fg = AnsiColor::Default;
    let mut bg = AnsiColor::Default;
    let mut bold = false;
    let mut italic = false;
    let mut underline = false;

    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next(); // consume '['
                    let mut params = String::new();
                    while let Some(&next) = chars.peek() {
                        if next.is_alphabetic() || next == 'm' {
                            chars.next();
                            if next == 'm' {
                                // SGR sequence — parse color/style params
                                let codes: Vec<u32> = params
                                    .split(';')
                                    .filter_map(|s| s.parse().ok())
                                    .collect();

                                let mut i = 0;
                                while i < codes.len() {
                                    match codes[i] {
                                        0 => {
                                            fg = AnsiColor::Default;
                                            bg = AnsiColor::Default;
                                            bold = false;
                                            italic = false;
                                            underline = false;
                                        }
                                        1 => bold = true,
                                        3 => italic = true,
                                        4 => underline = true,
                                        22 => bold = false,
                                        23 => italic = false,
                                        24 => underline = false,
                                        30..=37 => fg = AnsiColor::Indexed((codes[i] - 30) as u8),
                                        38 => {
                                            if i + 1 < codes.len() && codes[i + 1] == 2 {
                                                // 24-bit color: 38;2;R;G;B
                                                if i + 4 < codes.len() {
                                                    fg = AnsiColor::Rgb(
                                                        codes[i + 2] as u8,
                                                        codes[i + 3] as u8,
                                                        codes[i + 4] as u8,
                                                    );
                                                    i += 4;
                                                }
                                            } else if i + 1 < codes.len() && codes[i + 1] == 5 {
                                                // 256 color: 38;5;N
                                                if i + 2 < codes.len() {
                                                    fg = AnsiColor::Indexed(codes[i + 2] as u8);
                                                    i += 2;
                                                }
                                            }
                                        }
                                        39 => fg = AnsiColor::Default,
                                        40..=47 => bg = AnsiColor::Indexed((codes[i] - 40) as u8),
                                        48 => {
                                            if i + 1 < codes.len() && codes[i + 1] == 2 {
                                                if i + 4 < codes.len() {
                                                    bg = AnsiColor::Rgb(
                                                        codes[i + 2] as u8,
                                                        codes[i + 3] as u8,
                                                        codes[i + 4] as u8,
                                                    );
                                                    i += 4;
                                                }
                                            } else if i + 1 < codes.len() && codes[i + 1] == 5 {
                                                if i + 2 < codes.len() {
                                                    bg = AnsiColor::Indexed(codes[i + 2] as u8);
                                                    i += 2;
                                                }
                                            }
                                        }
                                        49 => bg = AnsiColor::Default,
                                        90..=97 => fg = AnsiColor::Indexed((codes[i] - 90 + 8) as u8),
                                        100..=107 => bg = AnsiColor::Indexed((codes[i] - 100 + 8) as u8),
                                        _ => {}
                                    }
                                    i += 1;
                                }
                            }
                            break;
                        }
                        chars.next();
                        params.push(next);
                    }
                }
            }
            '\n' => {
                lines.push(std::mem::take(&mut current_line));
            }
            '\r' => {} // Skip carriage returns
            _ => {
                current_line.push(StyledChar {
                    ch: c,
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                });
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}
