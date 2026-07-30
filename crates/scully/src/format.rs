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
/// Gap columns either side of the nick.
pub const GUTTER: usize = 2;

/// Total columns before the message text begins — the hanging indent.
pub const TEXT_COLUMN: usize = TIME_WIDTH + GUTTER + NICK_WIDTH + GUTTER;

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
}

/// Fallback nick palette, used until `look.nick.colors` loads (and when it is
/// empty). Nicks keep the same colour across restarts and across windows
/// because the colour is a pure function of the folded nick and palette size,
/// never of arrival order.
pub const DEFAULT_NICK_PALETTE: [&str; 12] = [
    "#82aaff", "#c792ea", "#f78c6c", "#89ddff", "#c3e88d", "#ffcb6b",
    "#ff9cac", "#7fdbca", "#b2ccd6", "#e59aff", "#f07178", "#a1c4fd",
];

pub fn nick_color_index(nick: &str, palette_len: usize) -> usize {
    // djb2 over the *folded* nick, so `Alice` and `alice` — the same person —
    // never get two different colours.
    let h = nick
        .to_ascii_lowercase()
        .bytes()
        .fold(5381u64, |h, b| h.wrapping_mul(33).wrapping_add(b as u64));
    (h % palette_len.max(1) as u64) as usize
}

/// Translate the registry's moment.js-style time format (`HH:mm:ss`) to the
/// strftime tokens `glib::DateTime::format` understands. Unknown characters
/// pass through, and literal `%` is escaped first so a stored value cannot
/// smuggle strftime tokens in.
pub fn time_format_to_strftime(fmt: &str) -> String {
    let mut out = fmt.replace('%', "%%");
    for (from, to) in [("HH", "%H"), ("mm", "%M"), ("ss", "%S"), ("h", "%I"), ("A", "%p"), ("a", "%P")]
    {
        out = out.replace(from, to);
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

/// Local timestamp from the event's ISO 8601 stamp, in the given strftime
/// format (see [`time_format_to_strftime`]).
fn format_time(raw: Option<&str>, strftime: &str) -> String {
    let Some(raw) = raw else { return " ".repeat(TIME_WIDTH) };
    match glib::DateTime::from_iso8601(raw, None) {
        Ok(dt) => dt
            .to_local()
            .ok()
            .and_then(|d| d.format(strftime).ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| " ".repeat(TIME_WIDTH)),
        Err(_) => " ".repeat(TIME_WIDTH),
    }
}

/// Render a folded run of presence events as one summary line.
///
/// Uses the same muted styling as an individual presence event, so folding
/// changes the density of the buffer without changing its visual vocabulary.
pub fn summary_line(summary: &lurker_proto::consolidate::Summary, strftime: &str) -> Line {
    Line {
        time: format_time(summary.time.as_deref(), strftime),
        nick: pad_nick("--"),
        nick_tag: Some("nick-plain".to_string()),
        text: summary.render(),
        kind: LineKind::NickChange,
    }
}

/// Render an event, or `None` if it draws nothing.
///
/// `strftime` is the timestamp format (from `look.buffer.time_format`);
/// `palette_len` sizes the deterministic nick-colour hash to the user's
/// palette (`look.nick.colors`).
pub fn line_for(event: &MessageEvent, strftime: &str, palette_len: usize) -> Option<Line> {
    let time = format_time(event.time.as_deref(), strftime);
    let nick = event.nick.clone().unwrap_or_default();
    let text = event.text.clone().unwrap_or_default();

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
            (pad_nick("-->"), format!("{nick} joined"), LineKind::Join, false)
        }
        EventType::Part => (
            pad_nick("<--"),
            if text.is_empty() { format!("{nick} left") } else { format!("{nick} left ({text})") },
            LineKind::Leave,
            false,
        ),
        EventType::Quit => (
            pad_nick("<--"),
            if text.is_empty() { format!("{nick} quit") } else { format!("{nick} quit ({text})") },
            LineKind::Leave,
            false,
        ),
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
            (pad_nick("--"), format!("{nick} is now known as {new}"), LineKind::NickChange, false)
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
        time,
        nick: nick_col,
        nick_tag: colored
            .then(|| {
                if event.is_self {
                    "nick-self".to_string()
                } else {
                    format!("nick-{}", nick_color_index(&nick, palette_len))
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
        assert_eq!(nick_color_index("Alice", n), nick_color_index("alice", n));
        assert_eq!(nick_color_index("ALICE", n), nick_color_index("alice", n));
        assert!(nick_color_index("alice", n) < n);
        // A degenerate palette must not divide by zero.
        assert_eq!(nick_color_index("alice", 0), 0);
    }

    #[test]
    fn translates_moment_style_time_formats() {
        assert_eq!(time_format_to_strftime("HH:mm:ss"), "%H:%M:%S");
        assert_eq!(time_format_to_strftime("h:mm a"), "%I:%M %P");
        // Literal % in a stored value must not smuggle tokens through.
        assert_eq!(time_format_to_strftime("%n"), "%%n");
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
        assert_eq!(line_for(&e, "%H:%M:%S", 12).unwrap().kind, LineKind::Highlight);
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
            assert!(line_for(&ev(t.clone()), "%H:%M:%S", 12).is_none(), "{t:?} should not render");
        }
    }

    #[test]
    fn presence_events_render_with_direction_markers() {
        let mut join = ev(EventType::Join);
        join.time = None;
        let line = line_for(&join, "%H:%M:%S", DEFAULT_NICK_PALETTE.len()).unwrap();
        assert!(line.nick.trim() == "-->");
        assert_eq!(line.text, "alice joined");

        let mut quit = ev(EventType::Quit);
        quit.text = Some("Ping timeout".into());
        assert_eq!(line_for(&quit, "%H:%M:%S", 12).unwrap().text, "alice quit (Ping timeout)");
    }
}
