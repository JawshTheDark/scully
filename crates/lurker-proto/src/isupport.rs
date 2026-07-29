// RPL_ISUPPORT (numeric 005) parsing.
//
// Lurker's server relays 005 lines into the network's `:server:` log as
// `motd` rows (§7.3's catch-all), each ending in "are supported by this
// server". The tokens describe what the IRC network actually implements, so a
// channel-mode UI built from them shows a user only the switches that exist —
// ergo's mode set and UnrealIRCd's barely overlap outside the RFC core.

/// Parsed capability tokens relevant to channel management.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Isupport {
    /// `CHANMODES=A,B,C,D`:
    /// A = list modes (ban-like, always take a mask),
    /// B = always take a parameter,
    /// C = take a parameter only when set,
    /// D = flags with no parameter.
    pub list_modes: String,
    pub param_modes: String,
    pub set_param_modes: String,
    pub flag_modes: String,
    /// Prefix (per-user) mode letters from `PREFIX=(qaohv)~&@%+` — these are
    /// nicklist matters, not channel switches, and a channel dialog must
    /// exclude them even though some servers also list them in CHANMODES.
    pub prefix_modes: String,
    /// `EXCEPTS[=e]` — ban exceptions, when advertised.
    pub except_mode: Option<char>,
    /// `INVEX[=I]` — invite exceptions, when advertised.
    pub invex_mode: Option<char>,
    /// `TOPICLEN=390`, when advertised.
    pub topic_len: Option<usize>,
    /// Whether real 005 tokens were seen (vs. the RFC fallback).
    from_tokens: bool,
}

impl Default for Isupport {
    /// The RFC 2811 baseline, for a network whose 005 was never seen (or has
    /// been evicted from the ring). Better a conservative core set than an
    /// empty dialog.
    fn default() -> Self {
        Self {
            list_modes: "beI".into(),
            param_modes: "k".into(),
            set_param_modes: "l".into(),
            flag_modes: "imnpst".into(),
            prefix_modes: "ov".into(),
            except_mode: Some('e'),
            invex_mode: Some('I'),
            topic_len: None,
            from_tokens: false,
        }
    }
}

impl Isupport {
    /// True when this came from real 005 tokens rather than the RFC fallback.
    pub fn advertised(&self) -> bool {
        self.from_tokens
    }

    /// Merge the tokens from one server-log line into this Isupport, if it is
    /// an ISUPPORT (005) relay line. Non-005 lines are ignored. This is the
    /// incremental form used to CACHE capabilities as the 005 burst arrives,
    /// so they survive the server-buffer ring evicting the original lines
    /// (which is why a channel-mode dialog opened minutes later would
    /// otherwise fall back to the RFC minimum).
    ///
    /// Returns true if the line was an ISUPPORT line (tokens were merged).
    pub fn merge_line(&mut self, line: &str) -> bool {
        // The relayed 005 text is `TOKEN TOKEN … are supported by this server`.
        // Match loosely: trailing whitespace/period tolerated.
        let trimmed = line.trim_end_matches(['.', ' ']);
        let Some(tokens) = trimmed.strip_suffix("are supported by this server") else {
            return false;
        };
        let mut saw = false;
        for token in tokens.split_whitespace() {
            saw = true;
            let (key, value) = match token.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (token, None),
            };
            match key {
                "CHANMODES" => {
                    if let Some(v) = value {
                        let mut classes = v.split(',');
                        self.list_modes = classes.next().unwrap_or("").to_string();
                        self.param_modes = classes.next().unwrap_or("").to_string();
                        self.set_param_modes = classes.next().unwrap_or("").to_string();
                        self.flag_modes = classes.next().unwrap_or("").to_string();
                    }
                }
                "PREFIX" => {
                    if let Some(v) = value {
                        if let (Some(o), Some(c)) = (v.find('('), v.find(')')) {
                            if o < c {
                                self.prefix_modes = v[o + 1..c].to_string();
                            }
                        }
                    }
                }
                "EXCEPTS" => {
                    self.except_mode = Some(value.and_then(|v| v.chars().next()).unwrap_or('e'));
                }
                "INVEX" => {
                    self.invex_mode = Some(value.and_then(|v| v.chars().next()).unwrap_or('I'));
                }
                "TOPICLEN" => {
                    self.topic_len = value.and_then(|v| v.parse().ok());
                }
                _ => {}
            }
        }
        if saw {
            self.from_tokens = true;
        }
        saw
    }

    /// All non-prefix mode letters the server supports, for "is this letter
    /// known" checks.
    pub fn supports(&self, letter: char) -> bool {
        self.list_modes.contains(letter)
            || self.param_modes.contains(letter)
            || self.set_param_modes.contains(letter)
            || self.flag_modes.contains(letter)
    }
}

/// Parse ISUPPORT tokens out of server-log lines.
///
/// Feed it every `motd` line from a network's `:server:` buffer; it uses the
/// ones that carry 005 tokens and ignores the rest. Later lines override
/// earlier ones (as on a real 005 renegotiation).
pub fn parse(lines: impl Iterator<Item = String>) -> Isupport {
    let mut out = Isupport::default();
    for line in lines {
        out.merge_line(&line);
    }
    out
}

/// Human labels for well-known channel modes. Unknown letters render as
/// "mode X" — shown (the server advertises them) but not explained.
pub fn mode_label(letter: char) -> Option<&'static str> {
    Some(match letter {
        'i' => "Invite only",
        'm' => "Moderated (only voiced may speak)",
        'n' => "No external messages",
        't' => "Only ops may set the topic",
        's' => "Secret (hidden from lists)",
        'p' => "Private",
        'k' => "Channel key (password)",
        'l' => "User limit",
        'b' => "Ban list",
        'e' => "Ban exceptions",
        'I' => "Invite exceptions",
        'r' => "Registered channel",
        'R' => "Only registered users may join",
        'M' => "Only registered users may speak",
        'c' => "Block colour codes",
        'C' => "Block CTCPs",
        'S' => "Strip colour codes",
        'T' => "Block notices",
        'K' => "Block /knock",
        'V' => "Block invites",
        'u' => "Auditorium (hide joins)",
        'z' => "TLS users only",
        'O' => "Opers only",
        'f' => "Forward to channel",
        'j' => "Join throttle",
        'L' => "Large ban list",
        'P' => "Permanent",
        'Q' => "Block kicks",
        'N' => "No nick changes",
        'g' => "Free invite (anyone may /invite)",
        'E' => "Roleplay commands",
        'F' => "Allow forwarding to this channel",
        'J' => "Blocked-user rejoin delay",
        'd' => "Block realname/away spam",
        'G' => "Filter bad words",
        'x' => "Adminonly",
        'Y' => "IRCops always allowed",
        'a' => "Protected/admin (see nicklist)",
        'A' => "Admins may set +A",
        'D' => "Delay join messages",
        'W' => "Warn on entry",
        _ => return None,
    })
}

/// Split a current mode string like `+Cntl 50` (or `Cnt`) into
/// `(set_letters, params)`.
pub fn parse_current(modes: &str) -> (Vec<char>, Vec<String>) {
    let mut parts = modes.split_whitespace();
    let letters = parts
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| *c != '+')
        .collect();
    (letters, parts.map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact 005 relay lines captured from the ergo test network.
    const ERGO_005: [&str; 2] = [
        "AWAYLEN=390 BOT=B CASEMAPPING=ascii CHANLIMIT=#:100 CHANMODES=Ibe,k,fl,CEMRUimnstu \
         CHANNELLEN=64 CHANTYPES=# CHATHISTORY=1000 ELIST=U EXCEPTS EXTBAN=,m FORWARD=f INVEX \
         are supported by this server",
        "KICKLEN=390 MAXLIST=beI:100 MAXTARGETS=4 MODES MONITOR=100 NETWORK=ErgoTest NICKLEN=32 \
         PREFIX=(qaohv)~&@%+ SAFELIST STATUSMSG=~&@%+ TOPICLEN=390 are supported by this server",
    ];

    fn ergo() -> Isupport {
        parse(ERGO_005.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_chanmode_classes_from_real_ergo_lines() {
        let i = ergo();
        assert_eq!(i.list_modes, "Ibe");
        assert_eq!(i.param_modes, "k");
        assert_eq!(i.set_param_modes, "fl");
        assert_eq!(i.flag_modes, "CEMRUimnstu");
        assert!(i.advertised());
    }

    #[test]
    fn prefix_letters_are_separated_from_channel_modes() {
        let i = ergo();
        assert_eq!(i.prefix_modes, "qaohv");
        // qaohv are per-user; the dialog filter relies on them being distinct
        // from the flag set.
        for letter in "qaohv".chars() {
            assert!(!i.flag_modes.contains(letter));
        }
    }

    #[test]
    fn excepts_invex_and_topiclen() {
        let i = ergo();
        assert_eq!(i.except_mode, Some('e'));
        assert_eq!(i.invex_mode, Some('I'));
        assert_eq!(i.topic_len, Some(390));
    }

    #[test]
    fn non_005_lines_are_ignored_and_fallback_is_rfc() {
        let i = parse(
            ["Welcome to the network".to_string(), "Your host is ergo.test".to_string()]
                .into_iter(),
        );
        assert!(!i.advertised());
        assert_eq!(i.flag_modes, "imnpst", "RFC baseline when no 005 seen");
        assert!(i.supports('t'));
        assert!(!i.supports('C'), "extensions are not assumed");
    }

    #[test]
    fn supports_covers_all_four_classes() {
        let i = ergo();
        for (letter, why) in
            [('I', "list"), ('k', "param"), ('l', "set-param"), ('C', "flag")]
        {
            assert!(i.supports(letter), "{letter} ({why})");
        }
        assert!(!i.supports('X'));
    }

    #[test]
    fn current_mode_strings_split_into_letters_and_params() {
        assert_eq!(parse_current("+Cnt"), ("Cnt".chars().collect(), vec![]));
        assert_eq!(
            parse_current("+ntkl secret 50"),
            ("ntkl".chars().collect(), vec!["secret".to_string(), "50".to_string()])
        );
        assert_eq!(parse_current(""), (vec![], vec![]));
    }

    #[test]
    fn known_modes_have_labels_and_unknown_do_not() {
        assert_eq!(mode_label('t'), Some("Only ops may set the topic"));
        assert_eq!(mode_label('C'), Some("Block CTCPs"));
        assert_eq!(mode_label('Ω'), None);
    }
}
