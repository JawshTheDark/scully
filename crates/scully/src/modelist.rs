// Parsing channel list-mode replies (ban / except / invite lists).
//
// Lurker relays these numerics into the network's `:server:` log as `motd`
// text via `formatUnknownNumeric`, which drops the leading recipient-nick
// param and joins the rest. So RPL_BANLIST
//
//     :server 367 mynick #chan mask!user@host setter 1699999999
//
// arrives as the text `#chan mask!user@host setter 1699999999`, and
// RPL_ENDOFBANLIST as `#chan End of Channel Ban List`.
//
// Parsing them lets the channel dialog show the list in place rather than
// leaving the user to hunt through the server log — which is what made the
// list buttons look like they did nothing at all.

/// One parsed line of a list-mode reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListLine {
    /// A list entry: the mask, plus who set it and when, when the server says.
    Entry { mask: String, setter: Option<String>, when: Option<String> },
    /// The terminator ("End of Channel Ban List").
    End,
    /// Not part of this channel's list reply.
    Other,
}

/// Parse one server-log line as a list-mode reply for `channel`.
///
/// Channel comparison is ASCII case-insensitive, matching the rest of the
/// client's folding (§9.2).
pub fn parse_line(channel: &str, text: &str) -> ListLine {
    let text = text.trim();
    let channel = channel.trim();
    if channel.is_empty() {
        return ListLine::Other;
    }

    // The line must start with this channel's name followed by whitespace.
    let (head, rest) = match text.split_once(char::is_whitespace) {
        Some(parts) => parts,
        None => return ListLine::Other,
    };
    if !head.eq_ignore_ascii_case(channel) {
        return ListLine::Other;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return ListLine::Other;
    }

    // Terminators read "End of Channel Ban List" / "End of Channel Exception
    // List" / similar. Servers vary in wording, so match the prefix.
    if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("end of") {
        return ListLine::End;
    }

    let mut parts = rest.split_whitespace();
    let Some(mask) = parts.next() else { return ListLine::Other };
    // A mask always contains one of these; without them this is prose (an
    // unrelated numeric that happens to name the channel first), not an entry.
    if !mask.contains(['!', '@', '*', '$', '.', ':']) {
        return ListLine::Other;
    }
    ListLine::Entry {
        mask: mask.to_string(),
        setter: parts.next().map(str::to_string),
        when: parts.next().map(str::to_string),
    }
}

/// A parsed entry rendered for display: `mask — set by nick, <date>`.
pub fn describe(mask: &str, setter: Option<&str>, when: Option<&str>) -> String {
    let mut out = mask.to_string();
    match (setter, when.and_then(format_unix)) {
        (Some(who), Some(date)) => out.push_str(&format!("   — set by {who}, {date}")),
        (Some(who), None) => out.push_str(&format!("   — set by {who}")),
        (None, Some(date)) => out.push_str(&format!("   — {date}")),
        (None, None) => {}
    }
    out
}

/// Format a unix timestamp as a local date, if it looks like one.
fn format_unix(raw: &str) -> Option<String> {
    let secs: i64 = raw.parse().ok()?;
    // Sanity bound: 2000-01-01 .. 2100-01-01, so a non-timestamp trailing
    // token isn't rendered as a nonsense date.
    if !(946_684_800..4_102_444_800).contains(&secs) {
        return None;
    }
    let dt = gtk::glib::DateTime::from_unix_local(secs).ok()?;
    dt.format("%Y-%m-%d").ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_ban_entry_with_setter_and_timestamp() {
        let line = "#chan *!*@evil.example.net oper!o@host 1700000000";
        assert_eq!(
            parse_line("#chan", line),
            ListLine::Entry {
                mask: "*!*@evil.example.net".into(),
                setter: Some("oper!o@host".into()),
                when: Some("1700000000".into()),
            }
        );
    }

    #[test]
    fn parses_a_bare_mask_entry() {
        assert_eq!(
            parse_line("#chan", "#chan nick!*@*"),
            ListLine::Entry { mask: "nick!*@*".into(), setter: None, when: None }
        );
    }

    #[test]
    fn recognises_the_terminator() {
        assert_eq!(parse_line("#chan", "#chan End of Channel Ban List"), ListLine::End);
        assert_eq!(parse_line("#chan", "#chan End of Channel Exception List"), ListLine::End);
        // Case-insensitive on both channel and the phrase.
        assert_eq!(parse_line("#CHAN", "#chan end of channel invite list"), ListLine::End);
    }

    #[test]
    fn ignores_lines_for_other_channels_or_unrelated_numerics() {
        assert_eq!(parse_line("#chan", "#other *!*@x"), ListLine::Other);
        assert_eq!(parse_line("#chan", "Welcome to the network"), ListLine::Other);
        // Names the channel but is prose, not a mask.
        assert_eq!(parse_line("#chan", "#chan Cannot join channel"), ListLine::Other);
        assert_eq!(parse_line("#chan", ""), ListLine::Other);
    }

    #[test]
    fn channel_match_is_case_insensitive() {
        assert!(matches!(
            parse_line("#TestChan", "#testchan *!*@a.b"),
            ListLine::Entry { .. }
        ));
    }

    #[test]
    fn describe_renders_setter_and_date_when_present() {
        let d = describe("*!*@x", Some("oper"), Some("1700000000"));
        assert!(d.starts_with("*!*@x"));
        assert!(d.contains("set by oper"));
        assert!(d.contains("2023-11-"), "expected a formatted date, got {d}");

        // A non-timestamp trailing token is not rendered as a date.
        let d = describe("*!*@x", Some("oper"), Some("notatime"));
        assert_eq!(d, "*!*@x   — set by oper");
        assert_eq!(describe("*!*@x", None, None), "*!*@x");
    }
}
