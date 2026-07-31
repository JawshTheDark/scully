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
pub fn candidates(
    prefix: &str,
    recent_speakers_newest_first: impl Iterator<Item = String>,
    members_sorted: impl Iterator<Item = String>,
    own_nick: Option<&str>,
) -> Vec<String> {
    let own = own_nick.map(|n| n.to_ascii_lowercase());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for nick in recent_speakers_newest_first.chain(members_sorted) {
        let folded = nick.to_ascii_lowercase();
        if folded.starts_with(prefix) && Some(&folded) != own.as_ref() && seen.insert(folded) {
            out.push(nick);
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
            ["Anna".to_string(), "alfred".to_string()].into_iter(),
            ["abbot".to_string(), "alfred".to_string(), "anna".to_string(), "amiantos".to_string()]
                .into_iter(),
            Some("Abbot"),
        );
        // Anna spoke most recently → first; alfred next; members follow,
        // deduped case-insensitively; own nick (abbot) excluded.
        assert_eq!(got, ["Anna", "alfred", "amiantos"]);
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
