// Turning events into rendered lines.
//
// The layout is the weechat three-column one Lurker's web client uses:
//
//     20:16:04       LuckyMan  You can see it here: https://example.com
//     20:23:30           <--   p0indexter quit (Ping timeout: 245 seconds)
//
// Everything is monospace, so the nick column is right-aligned by padding
// rather than by measuring, and wrapped continuation lines hang under the text
// column via a negative paragraph indent.

use gtk::glib;

use lurker_proto::{EventType, MessageEvent};

/// Width of the nick column, in characters.
pub const NICK_WIDTH: usize = 13;
/// Width of the timestamp column (`HH:MM:SS`).
pub const TIME_WIDTH: usize = 8;

/// Which style a line is drawn in. Maps to a GtkTextTag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Message,
    SelfMessage,
    Highlight,
    Action,
    Notice,
    Join,
    Leave,
    Kick,
    NickChange,
    Mode,
    Topic,
    Server,
    Error,
}

impl LineKind {
    /// GtkTextTag name for the message text.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Message => "msg",
            Self::SelfMessage => "msg-self",
            Self::Highlight => "msg-highlight",
            Self::Action => "action",
            Self::Notice => "notice",
            Self::Join => "join",
            Self::Leave => "leave",
            Self::Kick => "kick",
            Self::NickChange => "nickchange",
            Self::Mode => "mode",
            Self::Topic => "topic",
            Self::Server => "server",
            Self::Error => "error",
        }
    }
}

/// One rendered row.
pub struct Line {
    pub time: String,
    /// Already padded to [`NICK_WIDTH`] and right-aligned.
    pub nick: String,
    /// Tag for the nick column, when it should carry its own colour.
    pub nick_tag: Option<String>,
    pub text: String,
    pub kind: LineKind,
    /// The speaker, when this row is something a *person said* and should
    /// therefore group under a nick heading. `None` for joins, modes, server
    /// notices and the like, which stand alone.
    ///
    /// Kept separate from `nick` because that field is the row's left-column
    /// marker (`-->`, `--`, `*`), which is not the same thing as authorship.
    pub author: Option<String>,
    /// The event's moment as unix seconds, for the author-collapse window
    /// (`look.message.collapse_authors_window`). `None` when the event carried
    /// no parseable time — such rows never collapse.
    pub unix: Option<i64>,
}

/// Fallback nick palette, used until `look.nick.colors` loads (and when it is
/// empty). Nicks keep the same colour across restarts and across windows
/// because the colour is a pure function of the folded nick and palette size,
/// never of arrival order.
pub const DEFAULT_NICK_PALETTE: [&str; 12] = [
    "#82aaff", "#c792ea", "#f78c6c", "#89ddff", "#c3e88d", "#ffcb6b",
    "#ff9cac", "#7fdbca", "#b2ccd6", "#e59aff", "#f07178", "#a1c4fd",
];

/// Trim trailing separator characters before hashing, weechat-style: `alice_`
/// and `alice__` are the same person as `alice`, so they share a colour.
/// Leading stop chars are kept (a nick like `_alice` is just `_alice`); the
/// trim starts at the first stop char that FOLLOWS a real character.
fn trim_for_color<'a>(nick: &'a str, stop_chars: &str) -> &'a str {
    let mut seen_other = false;
    for (i, ch) in nick.char_indices() {
        let is_stop = stop_chars.contains(ch);
        if is_stop && seen_other {
            return &nick[..i];
        }
        if !is_stop {
            seen_other = true;
        }
    }
    nick
}

/// Map a nick to a palette slot — **identically to the web client**
/// (`vue_client/src/utils/nickColor.ts`), so the same person is the same
/// colour whichever client you're looking at.
///
/// That parity is the whole design constraint here, and it is why this is NOT
/// textbook djb2: the web implements weechat's variant
/// (`h ^= (h<<5) + (h>>2) + codepoint`, 32-bit), over the stop-char-trimmed,
/// lowercased nick, by Unicode code point rather than byte. Any deviation —
/// 64-bit arithmetic, plain `h*33+c`, hashing bytes — produces different slots
/// for some nicks and quietly breaks cross-client agreement.
pub fn nick_color_index(nick: &str, palette_len: usize, stop_chars: &str) -> usize {
    let normalized = trim_for_color(nick, stop_chars).to_lowercase();
    let mut h: u32 = 5381;
    for ch in normalized.chars() {
        let term = (h << 5)
            .wrapping_add(h >> 2)
            .wrapping_add(ch as u32);
        h ^= term;
    }
    (h % palette_len.max(1) as u32) as usize
}

/// Translate the registry's moment.js-style time format (`HH:mm:ss`) to the
/// strftime tokens `glib::DateTime::format` understands. Unknown characters
/// pass through, and a literal `%` is escaped so a stored value cannot smuggle
/// strftime tokens in.
///
/// Scanned in ONE left-to-right pass, longest token first. The previous version
/// ran a chain of `str::replace` calls, which had two ways to be wrong: it had
/// no `hh`, so the single-`h` rule matched *both* characters and emitted the
/// hour twice (`hh:mm:ss` rendered `0909:11:06`), and every later replacement
/// re-scanned text an earlier one had already produced. A single pass can do
/// neither, because output is never re-examined.
pub fn time_format_to_strftime(fmt: &str) -> String {
    // Longest-first within each family: `hh` must be tried before `h`.
    const TOKENS: &[(&str, &str)] = &[
        ("HH", "%H"), // 00-23, padded
        ("H", "%H"),
        ("hh", "%I"), // 01-12, padded
        ("h", "%I"),
        ("mm", "%M"),
        ("m", "%M"),
        ("ss", "%S"),
        ("s", "%S"),
        ("A", "%p"), // AM/PM
        ("a", "%P"), // am/pm
    ];

    let mut out = String::with_capacity(fmt.len() + 8);
    let mut rest = fmt;
    'scan: while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('%') {
            out.push_str("%%");
            rest = r;
            continue;
        }
        for (from, to) in TOKENS {
            if let Some(r) = rest.strip_prefix(from) {
                out.push_str(to);
                rest = r;
                continue 'scan;
            }
        }
        // Not a token: copy one character through verbatim. Stepping by a whole
        // char keeps multi-byte separators intact.
        let ch = rest.chars().next().expect("non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn pad_nick(s: &str) -> String {
    let width = s.chars().count();
    if width >= NICK_WIDTH {
        // Truncate from the left, keeping the tail, so similar long nicks stay
        // distinguishable (`…ongNickName` rather than `VeryLongNick…`).
        let skip = width - NICK_WIDTH + 1;
        format!("…{}", s.chars().skip(skip).collect::<String>())
    } else {
        format!("{s:>NICK_WIDTH$}")
    }
}

/// Rendered width of `strftime`, in characters.
///
/// Every timestamp in a session uses one format and strftime zero-pads its
/// numeric fields, so a single reference render gives the column width. This
/// matters because the timestamp leads the line: a row with no time must still
/// occupy exactly the same column, and `TIME_WIDTH` alone is wrong for any
/// format that isn't `HH:mm:ss` (`hh:mm:ss a` renders eleven characters).
pub fn time_width(strftime: &str) -> usize {
    if strftime.is_empty() {
        return 0;
    }
    glib::DateTime::from_utc(2026, 1, 1, 0, 0, 0.0)
        .ok()
        .and_then(|d| d.format(strftime).ok())
        .map(|s| s.chars().count())
        .unwrap_or(TIME_WIDTH)
}

/// Local timestamp from the event's ISO 8601 stamp, in the given strftime
/// format (see [`time_format_to_strftime`]).
fn format_time(raw: Option<&str>, strftime: &str) -> String {
    let blank = || " ".repeat(time_width(strftime));
    let Some(raw) = raw else { return blank() };
    match glib::DateTime::from_iso8601(raw, None) {
        Ok(dt) => dt
            .to_local()
            .ok()
            .and_then(|d| d.format(strftime).ok())
            .map(|s| s.to_string())
            .unwrap_or_else(blank),
        Err(_) => blank(),
    }
}

/// Render a folded run of presence events as one summary line.
///
/// Uses the same muted styling as an individual presence event, so folding
/// changes the density of the buffer without changing its visual vocabulary.
pub fn summary_line(summary: &lurker_proto::consolidate::Summary, strftime: &str) -> Line {
    Line {
        author: None,
        time: format_time(summary.time.as_deref(), strftime),
        nick: pad_nick("--"),
        nick_tag: Some("nick-plain".to_string()),
        text: summary.render(),
        kind: LineKind::NickChange,
        unix: None,
    }
}

/// Everything the renderer needs from settings, gathered once per redraw so
/// per-line rendering never re-reads the settings store.
#[derive(Clone, Debug)]
pub struct RenderOpts {
    /// `look.buffer.time_format` (or the compact variant), as strftime.
    pub strftime: String,
    /// Length of `look.nick.colors`.
    pub palette_len: usize,
    /// `look.nick.color_stop_chars`.
    pub stop_chars: String,
    /// `chat.show_event_host`: user@host on join/part/quit/nick lines.
    pub show_event_host: bool,
    /// `chat.show_join_account`: services account on join lines.
    pub show_join_account: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            strftime: "%H:%M:%S".into(),
            palette_len: DEFAULT_NICK_PALETTE.len(),
            stop_chars: "_|".into(),
            show_event_host: false,
            show_join_account: false,
        }
    }
}

/// Render an event, or `None` if it draws nothing.
pub fn line_for(event: &MessageEvent, opts: &RenderOpts) -> Option<Line> {
    let strftime = opts.strftime.as_str();
    let palette_len = opts.palette_len;
    let time = format_time(event.time.as_deref(), strftime);
    let nick = event.nick.clone().unwrap_or_default();
    let text = event.text.clone().unwrap_or_default();

    // `chat.show_event_host`: the affected user's user@host beside their nick
    // on presence lines — ops reading for ban masks. The web renders
    // `alice (~alice@host) joined`; match it.
    let with_host = |nick: &str| -> String {
        match (&event.userhost, opts.show_event_host) {
            (Some(uh), true) if !uh.is_empty() => format!("{nick} ({uh})"),
            _ => nick.to_string(),
        }
    };

    let (nick_col, body, kind, colored) = match event.event_type {
        EventType::Message => {
            let kind = if event.matched {
                // §5.3: `matched` drives styling. It is deliberately NOT the
                // same question as whether to raise an alert — a muted-channel
                // highlight arrives matched:true, notify:false and must still
                // look like a highlight here.
                LineKind::Highlight
            } else if event.is_self {
                LineKind::SelfMessage
            } else {
                LineKind::Message
            };
            (pad_nick(&nick), text, kind, true)
        }
        EventType::Action => (pad_nick("*"), format!("{nick} {text}"), LineKind::Action, false),
        EventType::Notice => {
            (pad_nick(&format!("-{nick}-")), text, LineKind::Notice, false)
        }
        EventType::Join => {
            // `chat.show_join_account`: `alice [acct] joined`, matching the
            // web client — ops confirming who is identified.
            let mut who = with_host(&nick);
            if opts.show_join_account {
                if let Some(acct) = event.account.as_deref().filter(|a| !a.is_empty()) {
                    who = format!("{who} [{acct}]");
                }
            }
            (pad_nick("-->"), format!("{who} joined"), LineKind::Join, false)
        }
        EventType::Part => {
            let who = with_host(&nick);
            (
                pad_nick("<--"),
                if text.is_empty() { format!("{who} left") } else { format!("{who} left ({text})") },
                LineKind::Leave,
                false,
            )
        }
        EventType::Quit => {
            let who = with_host(&nick);
            (
                pad_nick("<--"),
                if text.is_empty() { format!("{who} quit") } else { format!("{who} quit ({text})") },
                LineKind::Leave,
                false,
            )
        }
        EventType::Kick => {
            let who = event.kicked.clone().unwrap_or_default();
            (
                pad_nick("<--"),
                if text.is_empty() {
                    format!("{who} was kicked by {nick}")
                } else {
                    format!("{who} was kicked by {nick} ({text})")
                },
                LineKind::Kick,
                false,
            )
        }
        EventType::Nick => {
            let new = event.new_nick.clone().unwrap_or_default();
            (
                pad_nick("--"),
                format!("{} is now known as {new}", with_host(&nick)),
                LineKind::NickChange,
                false,
            )
        }
        EventType::Chghost => {
            let ident = event.new_ident.clone().unwrap_or_default();
            let host = event.new_host.clone().unwrap_or_default();
            (pad_nick("--"), format!("{nick} changed host to {ident}@{host}"), LineKind::Mode, false)
        }
        EventType::Mode => (pad_nick("--"), format!("{nick} set {text}"), LineKind::Mode, false),
        EventType::Topic => (
            pad_nick("--"),
            format!("{nick} changed the topic to: {text}"),
            LineKind::Topic,
            false,
        ),
        EventType::Invite => {
            let ch = event.channel.clone().unwrap_or_default();
            let from = event.from.clone().unwrap_or_else(|| nick.clone());
            (pad_nick("--"), format!("{from} invited you to {ch}"), LineKind::Topic, false)
        }
        // §7.3: `motd` is deliberately the catch-all for all server-voice text
        // that has no better home — don't build a taxonomy on top of it.
        EventType::Motd => (pad_nick(""), text, LineKind::Server, false),
        EventType::Error => (pad_nick("!!"), text, LineKind::Error, false),
        EventType::System => {
            let kind = match event.level.as_deref() {
                Some("error") => LineKind::Error,
                Some("warn") => LineKind::Mode,
                _ => LineKind::Server,
            };
            (pad_nick("--"), text, kind, false)
        }
        EventType::Ctcp | EventType::E2e => (pad_nick("--"), text, LineKind::Server, false),
        // Pure state signals render nothing.
        _ => return None,
    };

    Some(Line {
        unix: event.time.as_deref().and_then(lurker_proto::timeparse::rfc3339_to_unix),
        author: match event.event_type {
            EventType::Message | EventType::Action => event.nick.clone(),
            _ => None,
        },
        time,
        nick: nick_col,
        nick_tag: colored
            .then(|| {
                if event.is_self {
                    "nick-self".to_string()
                } else {
                    format!("nick-{}", nick_color_index(&nick, palette_len, &opts.stop_chars))
                }
            })
            .or(Some("nick-plain".to_string())),
        text: body,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: EventType) -> MessageEvent {
        MessageEvent { event_type: t, nick: Some("alice".into()), ..Default::default() }
    }

    #[test]
    fn nick_colour_is_case_insensitive_and_stable() {
        // The same human must not get two colours because a server echoed
        // their nick with different casing.
        let n = DEFAULT_NICK_PALETTE.len();
        assert_eq!(nick_color_index("Alice", n, "_|"), nick_color_index("alice", n, "_|"));
        assert_eq!(nick_color_index("ALICE", n, "_|"), nick_color_index("alice", n, "_|"));
        assert!(nick_color_index("alice", n, "_|") < n);
        // A degenerate palette must not divide by zero.
        assert_eq!(nick_color_index("alice", 0, "_|"), 0);
    }

    #[test]
    fn nick_colour_matches_the_web_client_exactly() {
        // Reference values computed by running the web client's own
        // trimForColor + djb2 (vue_client/src/utils/nickColor.ts) in Node.
        // These are the whole point: the same person must be the same colour
        // whichever client you're looking at, so any change here that breaks
        // one of these breaks cross-client agreement, however reasonable it
        // looks locally.
        for (nick, index) in [
            ("amiantos", 7),
            ("Milambar", 10),
            ("kaitphone", 8),
            ("alice", 2),
            ("jawsh", 8),
            ("Guest22618", 2),
        ] {
            assert_eq!(nick_color_index(nick, 12, "_|"), index, "{nick}");
        }
    }

    #[test]
    fn stop_chars_trim_trailing_separators_but_keep_leading_ones() {
        // weechat semantics, via the web client: `alice__` is alice come back
        // from a netsplit, but `_alice` is just someone called `_alice`.
        let n = 12;
        assert_eq!(nick_color_index("alice__", n, "_|"), nick_color_index("alice", n, "_|"));
        assert_eq!(nick_color_index("alice|away", n, "_|"), nick_color_index("alice", n, "_|"));
        assert_eq!(nick_color_index("_alice", n, "_|"), 3, "web reference value");
        assert_ne!(
            nick_color_index("_alice", n, "_|"),
            nick_color_index("alice", n, "_|"),
            "a leading underscore is part of the name, not a separator"
        );
        // No stop chars configured: nothing is trimmed.
        assert_ne!(
            nick_color_index("alice__", n, ""),
            nick_color_index("alice", n, "")
        );
    }

    #[test]
    fn translates_moment_style_time_formats() {
        assert_eq!(time_format_to_strftime("HH:mm:ss"), "%H:%M:%S");
        assert_eq!(time_format_to_strftime("h:mm a"), "%I:%M %P");
        // Literal % in a stored value must not smuggle tokens through.
        assert_eq!(time_format_to_strftime("%n"), "%%n");
    }

    #[test]
    fn a_doubled_token_is_one_field_not_two() {
        // The regression: with no "hh" rule, the single-"h" rule matched both
        // characters and emitted the hour twice — 9:11:06pm rendered as
        // "0909:11:06:pm". Every doubled form must collapse to one field.
        assert_eq!(time_format_to_strftime("hh:mm:ss:a"), "%I:%M:%S:%P");
        assert_eq!(time_format_to_strftime("hh"), "%I");
        assert_eq!(time_format_to_strftime("HH"), "%H");
        assert_eq!(time_format_to_strftime("ss"), "%S");
        assert_eq!(time_format_to_strftime("mm"), "%M");
    }

    #[test]
    fn single_letter_forms_are_understood_too() {
        // moment.js allows the unpadded spellings; they were silently passed
        // through as literal letters before.
        assert_eq!(time_format_to_strftime("H:m:s"), "%H:%M:%S");
        assert_eq!(time_format_to_strftime("h:mm A"), "%I:%M %p");
    }

    #[test]
    fn output_is_never_rescanned_for_more_tokens() {
        // A single pass is what makes this safe: "%H" produced by an earlier
        // rule contains an "H", and a second replace pass would have expanded
        // it again. Separators and unknown text pass through untouched.
        assert_eq!(time_format_to_strftime("[HH]"), "[%H]");
        assert_eq!(time_format_to_strftime("HH·mm"), "%H·%M", "multi-byte separator survives");
        assert_eq!(time_format_to_strftime(""), "");
    }

    #[test]
    fn the_time_column_is_as_wide_as_the_format_renders() {
        // The stamp leads the line now, so a row with no time has to pad to
        // exactly this width or the text beside it steps out of column.
        // TIME_WIDTH alone is only right for HH:mm:ss.
        assert_eq!(time_width("%H:%M:%S"), 8);
        assert_eq!(time_width("%I:%M:%S:%P"), 11, "12-hour with a meridiem is wider");
        assert_eq!(time_width("%H:%M"), 5);
        assert_eq!(time_width(""), 0, "timestamps off: no column at all");
    }

    #[test]
    fn a_row_with_no_time_still_fills_the_column() {
        assert_eq!(format_time(None, "%I:%M:%S:%P").chars().count(), 11);
        assert_eq!(format_time(Some("not a date"), "%H:%M:%S"), "        ");
    }

    #[test]
    fn nick_column_is_fixed_width() {
        assert_eq!(pad_nick("bob").chars().count(), NICK_WIDTH);
        assert_eq!(pad_nick("a-very-long-nickname-indeed").chars().count(), NICK_WIDTH);
    }

    #[test]
    fn long_nicks_keep_their_tail() {
        // Truncating the head keeps similar long nicks distinguishable.
        let padded = pad_nick("SuperLongNickAAA");
        assert!(padded.ends_with("AAA"), "got {padded:?}");
        assert!(padded.starts_with('…'));
    }

    #[test]
    fn a_matched_message_styles_as_a_highlight_regardless_of_notify() {
        // §5.3: styling reads `matched`; alerting reads `notify`. A muted
        // highlight is matched:true, notify:false and must still look like one.
        let mut e = ev(EventType::Message);
        e.text = Some("ping".into());
        e.matched = true;
        e.notify = false;
        assert_eq!(line_for(&e, &RenderOpts::default()).unwrap().kind, LineKind::Highlight);
    }

    #[test]
    fn pure_state_signals_render_nothing() {
        for t in [
            EventType::Names,
            EventType::MemberUpdate,
            EventType::Typing,
            EventType::Lag,
            EventType::ChannelJoined,
            EventType::State,
        ] {
            assert!(line_for(&ev(t.clone()), &RenderOpts::default()).is_none(), "{t:?} should not render");
        }
    }

    #[test]
    fn presence_events_render_with_direction_markers() {
        let mut join = ev(EventType::Join);
        join.time = None;
        let line = line_for(&join, &RenderOpts::default()).unwrap();
        assert!(line.nick.trim() == "-->");
        assert_eq!(line.text, "alice joined");

        let mut quit = ev(EventType::Quit);
        quit.text = Some("Ping timeout".into());
        assert_eq!(line_for(&quit, &RenderOpts::default()).unwrap().text, "alice quit (Ping timeout)");
    }

    #[test]
    fn event_host_and_join_account_follow_their_settings() {
        // chat.show_event_host / chat.show_join_account, matching the web's
        // rendering: `alice (~a@host) [acct] joined`. Off by default; a join
        // that carries the data must not leak it unless asked.
        let mut join = ev(EventType::Join);
        join.userhost = Some("~a@host.example.net".into());
        join.account = Some("aliceacct".into());

        assert_eq!(line_for(&join, &RenderOpts::default()).unwrap().text, "alice joined");

        let host_only = RenderOpts { show_event_host: true, ..RenderOpts::default() };
        assert_eq!(
            line_for(&join, &host_only).unwrap().text,
            "alice (~a@host.example.net) joined"
        );

        let both = RenderOpts {
            show_event_host: true,
            show_join_account: true,
            ..RenderOpts::default()
        };
        assert_eq!(
            line_for(&join, &both).unwrap().text,
            "alice (~a@host.example.net) [aliceacct] joined"
        );

        // Quit lines take the host but never the account — accounts are a
        // join-line detail (the extended-join cap), matching the web.
        let mut quit = ev(EventType::Quit);
        quit.userhost = Some("~a@host.example.net".into());
        quit.account = Some("aliceacct".into());
        assert_eq!(
            line_for(&quit, &both).unwrap().text,
            "alice (~a@host.example.net) quit"
        );
    }
}
