// Pure input-editing logic: tab completion and history recall.
//
// Extracted from the window so it can be unit-tested without a display or
// synthetic keystrokes — injecting real key events into a live session proved
// both unreliable (focus-stealing prevention) and rude (the keys land in
// whatever actually has focus).

/// What the word under the cursor looks like it wants to become. Tab means
/// different things in different positions, and guessing wrong is worse than
/// not completing: completing `/wh` against the nicklist finds nothing and
/// feels broken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// A slash command, but only in the first column — `/` anywhere else is
    /// ordinary text (a URL, a fraction).
    Command,
    /// A channel name, by its sigil.
    Channel,
    /// Anything else: a nick.
    Nick,
}

/// Classify the word under the cursor: `(anchor, folded prefix, kind)`.
///
/// The prefix keeps a channel's sigil (it is part of the name) but drops a
/// command's leading slash, so it can be matched against the command table
/// directly.
///
/// Only `#` and `&` count as channel sigils here. RFC also allows `+` and `!`,
/// but `+` collides with mode arguments — `/mode +o<Tab>` should not go looking
/// for a channel called `+o`. That is the same trap that `/mode +t` fell into
/// in the command parser.
pub fn classify(text: &str, cursor: usize) -> (usize, String, Token) {
    let (start, prefix) = prefix_at(text, cursor);
    if start == 0 {
        if let Some(rest) = prefix.strip_prefix('/') {
            return (start, rest.to_string(), Token::Command);
        }
    }
    if prefix.starts_with('#') || prefix.starts_with('&') {
        return (start, prefix, Token::Channel);
    }
    (start, prefix, Token::Nick)
}

/// One tab-completion step over `text` with the cursor at `cursor` (both in
/// characters). Returns `(new_text, new_cursor)`; `start` is the completion
/// anchor, reused when cycling.
///
/// Suffix conventions, which are what make completion feel right rather than
/// merely correct: a nick at the start of a line is being *addressed*, so it
/// gets `": "`; everywhere else a single space is enough. Commands get their
/// slash back.
pub fn complete(
    text: &str,
    cursor: usize,
    start: usize,
    value: &str,
    kind: Token,
) -> (String, usize) {
    let head: String = text.chars().take(start).collect();
    let tail: String = text.chars().skip(cursor).collect();
    let insert = match kind {
        Token::Command => format!("/{value} "),
        Token::Channel => format!("{value} "),
        Token::Nick if start == 0 => format!("{value}: "),
        Token::Nick => format!("{value} "),
    };
    let new_cursor = head.chars().count() + insert.chars().count();
    (format!("{head}{insert}{tail}"), new_cursor)
}

/// Command names matching `prefix`, alphabetically. Case-insensitive; `prefix`
/// is already folded by [`classify`].
pub fn command_candidates(prefix: &str, known: &[&str]) -> Vec<String> {
    let mut out: Vec<String> =
        known.iter().filter(|c| c.starts_with(prefix)).map(|c| c.to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// The prefix under the cursor: `(start, folded_prefix)`.
pub fn prefix_at(text: &str, cursor: usize) -> (usize, String) {
    let upto: String = text.chars().take(cursor).collect();
    let start = upto
        .char_indices()
        .filter(|(_, c)| *c == ' ')
        .next_back()
        .map(|(i, _)| upto[..i].chars().count() + 1)
        .unwrap_or(0);
    let prefix: String = upto.chars().skip(start).collect();
    (start, prefix.to_ascii_lowercase())
}

/// Order completion candidates: recent speakers newest-first (the person you
/// are replying to), then members alphabetically; own nick and self-echoes
/// excluded; deduped case-insensitively.
pub fn candidates<'a>(
    prefix: &str,
    recent_speakers_newest_first: impl Iterator<Item = &'a str>,
    members_sorted: impl Iterator<Item = &'a str>,
    own_nick: Option<&str>,
) -> Vec<String> {
    let own = own_nick.map(|n| n.to_ascii_lowercase());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for nick in recent_speakers_newest_first.chain(members_sorted) {
        // Cheap rejection FIRST: this runs per keystroke over every member of
        // the channel, and the old shape lowercased (allocating) every nick
        // before testing the prefix — ~2500 allocations per keypress in a big
        // channel. A byte-length gate plus an ASCII case-insensitive prefix
        // compare rejects non-matches allocation-free; only survivors fold.
        if nick.len() < prefix.len() {
            continue;
        }
        let Some(head) = nick.get(..prefix.len()) else {
            continue; // prefix length lands mid-codepoint: cannot match
        };
        if !head.eq_ignore_ascii_case(prefix) {
            continue;
        }
        let folded = nick.to_ascii_lowercase();
        if Some(&folded) != own.as_ref() && seen.insert(folded) {
            out.push(nick.to_string());
        }
    }
    out
}

/// One step of history recall. `pos` is `None` when not recalling; `history`
/// is oldest-first. Returns the new position (`None` = restore the stash).
pub fn recall_step(pos: Option<usize>, len: usize, direction: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match (pos, direction) {
        (None, d) if d < 0 => Some(len - 1),
        (None, _) => None,
        (Some(0), d) if d < 0 => Some(0),
        (Some(i), d) if d < 0 => Some(i - 1),
        (Some(i), _) if i + 1 < len => Some(i + 1),
        (Some(_), _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completing_at_line_start_addresses_the_nick() {
        let (start, prefix) = prefix_at("rep", 3);
        assert_eq!((start, prefix.as_str()), (0, "rep"));
        let (text, cursor) = complete("rep", 3, start, "reprouser", Token::Nick);
        assert_eq!(text, "reprouser: ");
        assert_eq!(cursor, 11);
    }

    #[test]
    fn completing_mid_line_appends_a_space() {
        let text = "thanks rep";
        let (start, prefix) = prefix_at(text, 10);
        assert_eq!((start, prefix.as_str()), (7, "rep"));
        let (new, cursor) = complete(text, 10, start, "reprouser", Token::Nick);
        assert_eq!(new, "thanks reprouser ");
        assert_eq!(cursor, 17);
    }

    #[test]
    fn completion_preserves_the_tail_after_the_cursor() {
        // "rep|, hello" — completing must not eat ", hello".
        let text = "rep, hello";
        let (start, _) = prefix_at(text, 3);
        let (new, _) = complete(text, 3, start, "reprouser", Token::Nick);
        assert_eq!(new, "reprouser: , hello");
    }

    #[test]
    fn cycling_reuses_the_anchor() {
        // Second Tab replaces the first completion wholesale.
        let (text, cursor) = complete("alice: ", 7, 0, "alicia", Token::Nick);
        assert_eq!(text, "alicia: ");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn candidates_rank_recent_speakers_before_members_and_skip_self() {
        let got = candidates(
            "a",
            ["Anna", "alfred"].into_iter(),
            ["abbot", "alfred", "anna", "amiantos"].into_iter(),
            Some("Abbot"),
        );
        // Anna spoke most recently → first; alfred next; members follow,
        // deduped case-insensitively; own nick (abbot) excluded.
        assert_eq!(got, ["Anna", "alfred", "amiantos"]);
    }

    #[test]
    fn candidate_prefix_gate_survives_multibyte_nicks() {
        // The allocation-free prefix gate slices nicks at prefix.len() BYTES;
        // a boundary landing mid-codepoint must reject cleanly, not panic.
        let got = candidates("ab", ["ábc", "abc"].into_iter(), [].into_iter(), None);
        assert_eq!(got, ["abc"]);
    }

    #[test]
    fn unicode_prefixes_complete_without_panicking() {
        // Multi-byte characters before the word must not split a char
        // boundary. "héllo bj" with the cursor at the end.
        let text = "héllo bj";
        let (start, prefix) = prefix_at(text, text.chars().count());
        assert_eq!(prefix, "bj");
        let (new, _) = complete(text, text.chars().count(), start, "bjourne", Token::Nick);
        assert_eq!(new, "héllo bjourne ");
    }

    #[test]
    fn a_leading_slash_is_a_command_only_in_the_first_column() {
        let (start, prefix, kind) = classify("/wh", 3);
        assert_eq!((start, prefix.as_str(), kind), (0, "wh", Token::Command));
        // Mid-line a slash is ordinary text — a URL or a fraction, not a
        // command. Completing it against the command table would be nonsense.
        let text = "see http://x/y";
        let (_, _, kind) = classify(text, text.chars().count());
        assert_eq!(kind, Token::Nick);
    }

    #[test]
    fn channel_sigils_classify_but_a_mode_argument_does_not() {
        assert_eq!(classify("#lur", 4).2, Token::Channel);
        assert_eq!(classify("&local", 6).2, Token::Channel);
        // "/mode +o<Tab>" must not hunt for a channel named "+o" — the same
        // collision that bit the command parser.
        let text = "/mode +o";
        assert_eq!(classify(text, text.chars().count()).2, Token::Nick);
    }

    #[test]
    fn completing_a_command_restores_the_slash() {
        let (start, prefix, kind) = classify("/wh", 3);
        assert_eq!(prefix, "wh");
        let (text, cursor) = complete("/wh", 3, start, "whois", kind);
        assert_eq!(text, "/whois ");
        assert_eq!(cursor, 7);
    }

    #[test]
    fn completing_a_channel_keeps_its_sigil_and_never_addresses_it() {
        // A channel at the start of a line gets a plain space, not ": " — you
        // are naming it, not talking to it.
        let (start, _, kind) = classify("#lur", 4);
        let (text, _) = complete("#lur", 4, start, "#lurker-spooky", kind);
        assert_eq!(text, "#lurker-spooky ");
    }

    #[test]
    fn command_candidates_are_prefix_matched_and_sorted() {
        let known = ["whois", "who", "whowas", "join", "wi"];
        assert_eq!(command_candidates("wh", &known), ["who", "whois", "whowas"]);
        assert_eq!(command_candidates("j", &known), ["join"]);
        assert!(command_candidates("zzz", &known).is_empty());
        // An empty prefix ("/" alone) offers everything, which is a usable
        // "what can I type?" affordance.
        assert_eq!(command_candidates("", &known).len(), known.len());
    }

    #[test]
    fn recall_walks_to_oldest_and_back_out() {
        // history = [old, mid, new]
        assert_eq!(recall_step(None, 3, -1), Some(2), "first Up → newest");
        assert_eq!(recall_step(Some(2), 3, -1), Some(1));
        assert_eq!(recall_step(Some(1), 3, -1), Some(0));
        assert_eq!(recall_step(Some(0), 3, -1), Some(0), "Up at oldest stays");
        assert_eq!(recall_step(Some(0), 3, 1), Some(1));
        assert_eq!(recall_step(Some(2), 3, 1), None, "Down past newest → stash");
        assert_eq!(recall_step(None, 3, 1), None, "Down without recalling is a no-op");
        assert_eq!(recall_step(None, 0, -1), None, "empty history");
    }
}

/// Merge a colour pick into any mIRC colour code already sitting at the
/// cursor, so "left-click red, right-click yellow" produces ONE
/// `\x0304,08` rather than two codes fighting (`\x0304` then `\x0399,08`
/// resets the foreground the first click chose).
///
/// `before` is the text before the cursor. Returns how many chars of it to
/// replace and the replacement code. A background-only pick uses foreground
/// 99 — mIRC "default" — matching the web's own picker.
pub fn merge_color_code(before: &str, pick: u8, background: bool) -> (usize, String) {
    // Parse a trailing \x03FF[,BB] by walking back over [digits][,digits].
    let chars: Vec<char> = before.chars().collect();
    let mut i = chars.len();
    let mut digits_end = i;
    let mut comma = None;
    while i > 0 && chars[i - 1].is_ascii_digit() && digits_end - i < 2 {
        i -= 1;
    }
    if i > 0 && chars[i - 1] == ',' && i < digits_end {
        comma = Some(i - 1);
        i -= 1;
        digits_end = i;
        while i > 0 && chars[i - 1].is_ascii_digit() && digits_end - i < 2 {
            i -= 1;
        }
    }
    let (existing_fg, existing_bg, span) = if i > 0 && chars[i - 1] == '\u{03}' && i < chars.len()
    {
        let code: String = chars[i..].iter().collect();
        let (fg_s, bg_s) = match comma {
            Some(c) => {
                let rel = c - i;
                (code[..rel].to_string(), Some(code[rel + 1..].to_string()))
            }
            None => (code.clone(), None),
        };
        (
            fg_s.parse::<u8>().ok(),
            bg_s.and_then(|b| b.parse::<u8>().ok()),
            chars.len() - (i - 1),
        )
    } else {
        (None, None, 0)
    };

    let (fg, bg) = if background {
        (existing_fg, Some(pick))
    } else {
        (Some(pick), existing_bg)
    };
    let code = match (fg, bg) {
        (Some(f), Some(b)) => format!("\u{03}{f:02},{b:02}"),
        (Some(f), None) => format!("\u{03}{f:02}"),
        // Background with no foreground: 99 is mIRC "default fg".
        (None, Some(b)) => format!("\u{03}99,{b:02}"),
        (None, None) => format!("\u{03}{pick:02}"),
    };
    (span, code)
}

#[cfg(test)]
mod color_merge_tests {
    use super::*;

    #[test]
    fn a_foreground_pick_stands_alone() {
        assert_eq!(merge_color_code("hello ", 4, false), (0, "\u{03}04".into()));
    }

    #[test]
    fn a_background_pick_alone_uses_default_foreground() {
        // 99 is mIRC "default", the same convention the web picker emits.
        assert_eq!(merge_color_code("hello ", 8, true), (0, "\u{03}99,08".into()));
    }

    #[test]
    fn left_then_right_merges_into_one_code() {
        // Left-click red, right-click yellow: one \x0304,08, never two codes.
        let (span, code) = merge_color_code("hi \u{03}04", 8, true);
        assert_eq!((span, code.as_str()), (3, "\u{03}04,08"));
    }

    #[test]
    fn right_then_left_fills_the_foreground_in() {
        let (span, code) = merge_color_code("\u{03}99,08", 4, false);
        assert_eq!((span, code.as_str()), (6, "\u{03}04,08"));
    }

    #[test]
    fn repicking_replaces_that_slot_only() {
        let (span, code) = merge_color_code("\u{03}04,08", 12, true);
        assert_eq!((span, code.as_str()), (6, "\u{03}04,12"));
        let (span, code) = merge_color_code("\u{03}04,08", 2, false);
        assert_eq!((span, code.as_str()), (6, "\u{03}02,08"));
    }

    #[test]
    fn ordinary_trailing_digits_are_not_a_colour_code() {
        // "port 80" ends in digits but carries no \x03 — nothing to merge.
        assert_eq!(merge_color_code("port 80", 4, false), (0, "\u{03}04".into()));
    }
}
