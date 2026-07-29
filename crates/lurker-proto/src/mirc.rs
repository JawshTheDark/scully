// IRC inline formatting codes.
//
// IRC carries styling as in-band control characters. A client that does not
// strip them renders literal control glyphs (the `▯` boxes an unhandled MOTD
// colour chart produces), so parsing is not optional even for a client that
// chooses to draw everything plain.
//
//   0x02 bold      0x1D italic     0x1F underline   0x1E strikethrough
//   0x11 monospace 0x16 reverse    0x0F reset
//   0x03 colour    — `NN[,NN]`, 1–2 decimal digits each, 0–98
//   0x04 hex       — `RRGGBB[,RRGGBB]`
//
// The colour index table is the 99-entry mIRC palette: 0–15 classic, 16–98
// extended, 99 meaning "default".

/// Control characters, named so the parser reads as the spec does.
pub mod code {
    pub const BOLD: char = '\u{02}';
    pub const COLOR: char = '\u{03}';
    pub const HEX_COLOR: char = '\u{04}';
    pub const RESET: char = '\u{0F}';
    pub const MONOSPACE: char = '\u{11}';
    pub const REVERSE: char = '\u{16}';
    pub const ITALIC: char = '\u{1D}';
    pub const STRIKETHROUGH: char = '\u{1E}';
    pub const UNDERLINE: char = '\u{1F}';
}

/// True for any character that is a formatting control rather than content.
pub fn is_format_code(c: char) -> bool {
    matches!(
        c,
        code::BOLD
            | code::COLOR
            | code::HEX_COLOR
            | code::RESET
            | code::MONOSPACE
            | code::REVERSE
            | code::ITALIC
            | code::STRIKETHROUGH
            | code::UNDERLINE
    )
}

/// The 99-entry mIRC colour palette. Index 99 means "default", so it has no
/// entry here and [`color_hex`] returns `None` for it.
pub const PALETTE: [&str; 99] = [
    // 0–15: the classic palette.
    "#ffffff", "#000000", "#00007f", "#009300", "#ff0000", "#7f0000", "#9c009c", "#fc7f00",
    "#ffff00", "#00fc00", "#009393", "#00ffff", "#0000fc", "#ff00ff", "#7f7f7f", "#d2d2d2",
    // 16–27
    "#470000", "#472100", "#474700", "#324700", "#004700", "#00472c", "#004747", "#002747",
    "#000047", "#2e0047", "#470047", "#47002a",
    // 28–39
    "#740000", "#743a00", "#747400", "#517400", "#007400", "#007449", "#007474", "#004074",
    "#000074", "#4b0074", "#740074", "#740045",
    // 40–51
    "#b50000", "#b56300", "#b5b500", "#7db500", "#00b500", "#00b571", "#00b5b5", "#0063b5",
    "#0000b5", "#7500b5", "#b500b5", "#b5006b",
    // 52–63
    "#ff0000", "#ff8c00", "#ffff00", "#b2ff00", "#00ff00", "#00ffa0", "#00ffff", "#008cff",
    "#0000ff", "#a500ff", "#ff00ff", "#ff0098",
    // 64–75
    "#ff5959", "#ffb459", "#ffff71", "#cfff60", "#6fff6f", "#65ffc9", "#6dffff", "#59b4ff",
    "#5959ff", "#c459ff", "#ff66ff", "#ff59bc",
    // 76–87
    "#ff9c9c", "#ffd39c", "#ffff9c", "#e2ff9c", "#9cff9c", "#9cffdb", "#9cffff", "#9cd3ff",
    "#9c9cff", "#dc9cff", "#ff9cff", "#ff94d3",
    // 88–98: greyscale ramp.
    "#000000", "#131313", "#282828", "#363636", "#4d4d4d", "#656565", "#818181", "#9f9f9f",
    "#bcbcbc", "#e2e2e2", "#ffffff",
];

/// Hex for a palette index, or `None` for 99 ("default") and anything beyond.
pub fn color_hex(index: u8) -> Option<&'static str> {
    PALETTE.get(index as usize).copied()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub monospace: bool,
    /// Swap foreground and background at render time.
    pub reverse: bool,
    /// Palette index, 0–98.
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    /// `RRGGBB` from the 0x04 hex-colour code, without the leading `#`.
    pub fg_hex: Option<String>,
    pub bg_hex: Option<String>,
}

impl Style {
    pub fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// Resolved foreground as a CSS hex string, honouring `reverse`.
    pub fn fg_color(&self) -> Option<String> {
        let (fg, fg_hex) = if self.reverse {
            (self.bg, self.bg_hex.as_ref())
        } else {
            (self.fg, self.fg_hex.as_ref())
        };
        fg_hex
            .map(|h| format!("#{h}"))
            .or_else(|| fg.and_then(color_hex).map(str::to_string))
    }

    /// Resolved background as a CSS hex string, honouring `reverse`.
    pub fn bg_color(&self) -> Option<String> {
        let (bg, bg_hex) = if self.reverse {
            (self.fg, self.fg_hex.as_ref())
        } else {
            (self.bg, self.bg_hex.as_ref())
        };
        bg_hex
            .map(|h| format!("#{h}"))
            .or_else(|| bg.and_then(color_hex).map(str::to_string))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
}

/// Read 1–2 decimal digits, returning the value and how many chars were used.
fn take_digits(chars: &[char], from: usize) -> Option<(u8, usize)> {
    let mut value: u8 = 0;
    let mut used = 0;
    while used < 2 {
        match chars.get(from + used).and_then(|c| c.to_digit(10)) {
            Some(d) => {
                value = value * 10 + d as u8;
                used += 1;
            }
            None => break,
        }
    }
    (used > 0).then_some((value, used))
}

/// Read exactly six hex digits.
fn take_hex(chars: &[char], from: usize) -> Option<(String, usize)> {
    let hex: String = chars.iter().skip(from).take(6).collect();
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some((hex, 6))
}

/// Split `input` into styled segments, dropping the control characters.
///
/// Segments are emitted only where the style actually changes, so plain text
/// yields exactly one segment.
pub fn parse(input: &str) -> Vec<Segment> {
    let chars: Vec<char> = input.chars().collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut style = Style::default();
    let mut buffer = String::new();
    let mut i = 0;

    let flush = |buffer: &mut String, style: &Style, segments: &mut Vec<Segment>| {
        if !buffer.is_empty() {
            segments.push(Segment { text: std::mem::take(buffer), style: style.clone() });
        }
    };

    while i < chars.len() {
        let c = chars[i];
        match c {
            code::BOLD => {
                flush(&mut buffer, &style, &mut segments);
                style.bold = !style.bold;
                i += 1;
            }
            code::ITALIC => {
                flush(&mut buffer, &style, &mut segments);
                style.italic = !style.italic;
                i += 1;
            }
            code::UNDERLINE => {
                flush(&mut buffer, &style, &mut segments);
                style.underline = !style.underline;
                i += 1;
            }
            code::STRIKETHROUGH => {
                flush(&mut buffer, &style, &mut segments);
                style.strikethrough = !style.strikethrough;
                i += 1;
            }
            code::MONOSPACE => {
                flush(&mut buffer, &style, &mut segments);
                style.monospace = !style.monospace;
                i += 1;
            }
            code::REVERSE => {
                flush(&mut buffer, &style, &mut segments);
                style.reverse = !style.reverse;
                i += 1;
            }
            code::RESET => {
                flush(&mut buffer, &style, &mut segments);
                style = Style::default();
                i += 1;
            }
            code::COLOR => {
                flush(&mut buffer, &style, &mut segments);
                i += 1;
                match take_digits(&chars, i) {
                    None => {
                        // A bare 0x03 clears colour but leaves other attributes.
                        style.fg = None;
                        style.bg = None;
                        style.fg_hex = None;
                        style.bg_hex = None;
                    }
                    Some((fg, used)) => {
                        i += used;
                        style.fg = Some(fg);
                        style.fg_hex = None;
                        // A comma only introduces a background when digits
                        // follow it; otherwise it is literal text, as in
                        // "\x034,hello".
                        if chars.get(i) == Some(&',') {
                            if let Some((bg, used)) = take_digits(&chars, i + 1) {
                                i += 1 + used;
                                style.bg = Some(bg);
                                style.bg_hex = None;
                            }
                        }
                    }
                }
            }
            code::HEX_COLOR => {
                flush(&mut buffer, &style, &mut segments);
                i += 1;
                match take_hex(&chars, i) {
                    None => {
                        style.fg_hex = None;
                        style.bg_hex = None;
                    }
                    Some((fg, used)) => {
                        i += used;
                        style.fg_hex = Some(fg);
                        style.fg = None;
                        if chars.get(i) == Some(&',') {
                            if let Some((bg, used)) = take_hex(&chars, i + 1) {
                                i += 1 + used;
                                style.bg_hex = Some(bg);
                                style.bg = None;
                            }
                        }
                    }
                }
            }
            _ => {
                buffer.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buffer, &style, &mut segments);
    segments
}

/// The text with all formatting removed — for notifications, search and any
/// surface that cannot render style.
pub fn strip(input: &str) -> String {
    if !input.chars().any(is_format_code) {
        return input.to_string();
    }
    parse(input).into_iter().map(|s| s.text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_one_segment_and_survives_untouched() {
        let segs = parse("hello world");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "hello world");
        assert!(segs[0].style.is_plain());
    }

    #[test]
    fn strip_removes_every_control_character() {
        // The MOTD colour chart that rendered as boxes.
        let raw = "\u{03}1,0 00 \u{03}0,1 01 \u{02}bold\u{0F} plain";
        let stripped = strip(raw);
        assert_eq!(stripped, " 00  01 bold plain");
        assert!(!stripped.chars().any(is_format_code));
    }

    #[test]
    fn bold_toggles_rather_than_sets() {
        let segs = parse("a\u{02}b\u{02}c");
        assert_eq!(segs.len(), 3);
        assert!(!segs[0].style.bold);
        assert!(segs[1].style.bold);
        assert!(!segs[2].style.bold, "a second bold code turns it back off");
    }

    #[test]
    fn parses_foreground_and_background_indices() {
        let segs = parse("\u{03}4,12red on blue");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].style.fg, Some(4));
        assert_eq!(segs[0].style.bg, Some(12));
        assert_eq!(segs[0].style.fg_color().as_deref(), Some("#ff0000"));
        assert_eq!(segs[0].style.bg_color().as_deref(), Some("#0000fc"));
    }

    #[test]
    fn single_digit_colour_is_accepted() {
        let segs = parse("\u{03}4red");
        assert_eq!(segs[0].style.fg, Some(4));
        assert_eq!(segs[0].text, "red");
    }

    #[test]
    fn a_comma_without_digits_stays_literal_text() {
        // "\x034,hello" is colour 4 followed by ",hello" — not a background.
        let segs = parse("\u{03}4,hello");
        assert_eq!(segs[0].style.fg, Some(4));
        assert_eq!(segs[0].style.bg, None);
        assert_eq!(segs[0].text, ",hello");
    }

    #[test]
    fn a_bare_colour_code_clears_colour_but_keeps_attributes() {
        let segs = parse("\u{02}\u{03}4red\u{03}still bold");
        let last = segs.last().unwrap();
        assert_eq!(last.text, "still bold");
        assert_eq!(last.style.fg, None);
        assert!(last.style.bold, "0x03 must not clear bold");
    }

    #[test]
    fn reset_clears_everything() {
        let segs = parse("\u{02}\u{1F}\u{03}4styled\u{0F}plain");
        let last = segs.last().unwrap();
        assert_eq!(last.text, "plain");
        assert!(last.style.is_plain());
    }

    #[test]
    fn parses_hex_colours() {
        let segs = parse("\u{04}FF8800,001122hex");
        assert_eq!(segs[0].style.fg_hex.as_deref(), Some("FF8800"));
        assert_eq!(segs[0].style.bg_hex.as_deref(), Some("001122"));
        assert_eq!(segs[0].style.fg_color().as_deref(), Some("#FF8800"));
    }

    #[test]
    fn reverse_swaps_foreground_and_background() {
        let segs = parse("\u{03}4,12\u{16}swapped");
        let style = &segs[0].style;
        assert_eq!(style.fg_color().as_deref(), Some("#0000fc"), "fg takes the bg colour");
        assert_eq!(style.bg_color().as_deref(), Some("#ff0000"));
    }

    #[test]
    fn two_digit_indices_reach_the_extended_palette() {
        let segs = parse("\u{03}98,88extended");
        assert_eq!(segs[0].style.fg, Some(98));
        assert_eq!(segs[0].style.fg_color().as_deref(), Some("#ffffff"));
        assert_eq!(segs[0].style.bg_color().as_deref(), Some("#000000"));
    }

    #[test]
    fn index_99_means_default_and_has_no_colour() {
        assert_eq!(color_hex(99), None);
        let segs = parse("\u{03}99default");
        assert_eq!(segs[0].style.fg, Some(99));
        assert_eq!(segs[0].style.fg_color(), None);
    }

    #[test]
    fn palette_covers_every_defined_index() {
        assert_eq!(PALETTE.len(), 99);
        assert!(PALETTE.iter().all(|c| c.len() == 7 && c.starts_with('#')));
    }

    #[test]
    fn pango_markup_carries_colours_and_escapes_xml() {
        let markup = to_pango_markup("\u{03}4red & <b>\u{03} plain");
        assert!(markup.contains("foreground=\"#ff0000\""), "{markup}");
        assert!(markup.contains("red &amp; &lt;b&gt;"), "content must be escaped: {markup}");
        assert!(markup.ends_with(" plain"), "trailing plain text unwrapped: {markup}");
    }

    #[test]
    fn truncated_codes_do_not_panic() {
        for raw in ["\u{03}", "\u{04}", "\u{03}1,", "\u{04}FF88", "\u{03}4,1"] {
            let _ = parse(raw);
            let _ = strip(raw);
        }
    }
}

/// Render parsed segments as Pango markup, for surfaces that take markup
/// rather than TextTags (labels: the topic display, previews).
pub fn to_pango_markup(input: &str) -> String {
    let mut out = String::new();
    for seg in parse(input) {
        let escaped = glib_escape(&seg.text);
        if seg.style.is_plain() {
            out.push_str(&escaped);
            continue;
        }
        let mut attrs = String::new();
        if let Some(fg) = seg.style.fg_color() {
            attrs.push_str(&format!(" foreground=\"{fg}\""));
        }
        if let Some(bg) = seg.style.bg_color() {
            attrs.push_str(&format!(" background=\"{bg}\""));
        }
        if seg.style.bold {
            attrs.push_str(" weight=\"bold\"");
        }
        if seg.style.italic {
            attrs.push_str(" style=\"italic\"");
        }
        if seg.style.underline {
            attrs.push_str(" underline=\"single\"");
        }
        if seg.style.strikethrough {
            attrs.push_str(" strikethrough=\"true\"");
        }
        out.push_str(&format!("<span{attrs}>{escaped}</span>"));
    }
    out
}

/// Minimal markup escaping (Pango markup is XML).
fn glib_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
