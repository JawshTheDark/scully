// Slash commands that translate to a raw IRC line.
//
// Scully's command surface has two halves. The ones that change *client* state
// (`/msg` opening a DM, `/clear`, `/away`, `/search`) are handled in the window,
// because they need a buffer and a UI. Everything else is a pure text
// translation — `/op nick` → `MODE #chan +o nick` — and lives here so it can be
// unit-tested without a display.
//
// §12: slash commands are parsed CLIENT-side; the server does not interpret `/`
// in `send` text. Anything not in this table still reaches the network via the
// `raw` escape hatch, so an unknown command is passed through rather than
// rejected — matching how a long-lived IRC client behaves.

/// Expand a command into a raw IRC line, given the current channel.
///
/// Returns `None` when this module does not own the command (the caller then
/// falls back to its own handling or the raw passthrough).
/// `prefix_modes` is the network's ISUPPORT `PREFIX` letters (`qaohv`), used to
/// disambiguate `+q`: it means channel OWNER where `q` is a prefix mode
/// (UnrealIRCd, InspIRCd) but QUIET where it is a list mode (Libera/solanum).
/// Guessing wrong silently mutes someone you meant to promote, or vice versa.
pub fn to_raw(cmd: &str, args: &str, channel: &str, prefix_modes: &str) -> Option<String> {
    let q_is_owner = prefix_modes.contains('q');
    let args = args.trim();
    let first = args.split_whitespace().next().unwrap_or("");
    let rest_after_first = args.strip_prefix(first).map(str::trim).unwrap_or("");

    // Mode changes that take a nick, applied to the current channel when no
    // channel is given. `/op`, `/deop`, `/voice`, …
    let mode_for = |sign: char, letter: char, who: &str| {
        (!who.is_empty()).then(|| format!("MODE {channel} {sign}{letter} {who}"))
    };

    Some(match cmd {
        // ── Channel user modes ──
        "op" => mode_for('+', 'o', first)?,
        "deop" => mode_for('-', 'o', first)?,
        "hop" | "halfop" => mode_for('+', 'h', first)?,
        "dehalfop" | "dehop" => mode_for('-', 'h', first)?,
        "voice" => mode_for('+', 'v', first)?,
        "devoice" => mode_for('-', 'v', first)?,
        // Only offered where `q` is actually a prefix (status) mode.
        "owner" if q_is_owner => mode_for('+', 'q', first)?,
        "deowner" if q_is_owner => mode_for('-', 'q', first)?,
        "admin" | "protect" => mode_for('+', 'a', first)?,
        "deadmin" | "deprotect" => mode_for('-', 'a', first)?,

        // ── Bans and access ──
        "ban" => mode_for('+', 'b', first)?,
        "unban" => mode_for('-', 'b', first)?,
        // Conversely, quiet is `+q` only where `q` is NOT a status prefix.
        "quiet" | "mute" if !q_is_owner => mode_for('+', 'q', first)?,
        "unquiet" | "unmute" if !q_is_owner => mode_for('-', 'q', first)?,
        "kick" => {
            if first.is_empty() {
                return None;
            }
            if rest_after_first.is_empty() {
                format!("KICK {channel} {first}")
            } else {
                format!("KICK {channel} {first} :{rest_after_first}")
            }
        }
        "kickban" | "kb" => {
            if first.is_empty() {
                return None;
            }
            // Only the ban here; the caller sends both lines in order.
            format!("MODE {channel} +b {first}!*@*")
        }
        "invite" => {
            if first.is_empty() {
                return None;
            }
            let target = if rest_after_first.is_empty() { channel } else { rest_after_first };
            format!("INVITE {first} {target}")
        }
        "remove" => {
            if first.is_empty() {
                return None;
            }
            format!("REMOVE {channel} {first}")
        }

        // ── Channel and topic ──
        "topic" => {
            if args.is_empty() {
                format!("TOPIC {channel}")
            } else {
                format!("TOPIC {channel} :{args}")
            }
        }
        "mode" => {
            // `/mode +t` applies to this channel; `/mode #other +t` is explicit.
            //
            // `+` is technically a channel prefix (RFC 2811 no-mode channels)
            // but those are effectively extinct, while `+t` as a mode string is
            // everyday usage — so a leading `+`/`-` reads as modes, not a
            // channel. `#`, `&` and `!` still mean an explicit channel.
            if first.starts_with(['#', '&', '!']) {
                format!("MODE {args}")
            } else if args.is_empty() {
                format!("MODE {channel}")
            } else {
                format!("MODE {channel} {args}")
            }
        }
        "names" => format!("NAMES {}", if args.is_empty() { channel } else { args }),
        "knock" => format!("KNOCK {}", if args.is_empty() { channel } else { args }),

        // ── Server and user queries ──
        "whowas" => format!("WHOWAS {args}"),
        "who" => format!("WHO {}", if args.is_empty() { channel } else { args }),
        "ison" => format!("ISON {args}"),
        "userhost" => format!("USERHOST {args}"),
        "motd" => format!("MOTD {args}").trim_end().to_string(),
        "lusers" => "LUSERS".to_string(),
        "links" => "LINKS".to_string(),
        "map" => "MAP".to_string(),
        "stats" => format!("STATS {args}"),
        "time" => format!("TIME {args}").trim_end().to_string(),
        "version" => format!("VERSION {args}").trim_end().to_string(),
        "info" => "INFO".to_string(),
        "admin_server" => "ADMIN".to_string(),
        "oper" => format!("OPER {args}"),

        // ── Identity ──
        "nick" => {
            if first.is_empty() {
                return None;
            }
            format!("NICK {first}")
        }

        // ── Services shortcuts. These carry credentials, so §12 says send them
        //    over `raw` without local echo — which is what the caller does. ──
        "ns" | "nickserv" => format!("PRIVMSG NickServ :{args}"),
        "cs" | "chanserv" => format!("PRIVMSG ChanServ :{args}"),
        "ms" | "memoserv" => format!("PRIVMSG MemoServ :{args}"),
        "os" | "operserv" => format!("PRIVMSG OperServ :{args}"),
        "bs" | "botserv" => format!("PRIVMSG BotServ :{args}"),
        "hs" | "hostserv" => format!("PRIVMSG HostServ :{args}"),
        "identify" => format!("PRIVMSG NickServ :IDENTIFY {args}"),

        // ── Raw passthrough ──
        "quote" | "raw" => args.to_string(),
        "quit" => {
            if args.is_empty() {
                "QUIT".to_string()
            } else {
                format!("QUIT :{args}")
            }
        }

        _ => return None,
    })
}

/// Text-only commands that expand into a message rather than a raw line —
/// returns the text to send as a normal PRIVMSG.
pub fn to_message(cmd: &str, args: &str) -> Option<String> {
    Some(match cmd {
        "shrug" => {
            let shrug = "¯\\_(ツ)_/¯";
            if args.trim().is_empty() {
                shrug.to_string()
            } else {
                format!("{} {shrug}", args.trim())
            }
        }
        "tableflip" => format!("{} (╯°□°)╯︵ ┻━┻", args.trim()).trim().to_string(),
        "unflip" => format!("{} ┬─┬ノ( º _ ºノ)", args.trim()).trim().to_string(),
        _ => return None,
    })
}

/// Commands that expand into a CTCP ACTION (`/me`-style).
pub fn to_action(cmd: &str, args: &str) -> Option<String> {
    Some(match cmd {
        "slap" => {
            let who = args.split_whitespace().next().unwrap_or("");
            if who.is_empty() {
                return None;
            }
            format!("slaps {who} around a bit with a large trout")
        }
        _ => return None,
    })
}

/// Every command name this module or the window handles, for `/help` and
/// future completion.
pub const KNOWN: &[&str] = &[
    "me", "msg", "query", "notice", "join", "part", "leave", "close", "clear", "away", "back",
    "whois", "wi", "search", "topic", "mode", "names", "knock", "op", "deop", "hop", "halfop",
    "dehalfop", "voice", "devoice", "owner", "deowner", "admin", "protect", "ban", "unban",
    "quiet", "unquiet", "kick", "kickban", "kb", "invite", "remove", "nick", "who", "whowas",
    "ison", "userhost", "motd", "lusers", "links", "map", "stats", "time", "version", "info",
    "oper", "ns", "cs", "ms", "os", "bs", "hs", "identify", "quote", "raw", "quit", "shrug",
    "tableflip", "unflip", "slap", "help",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A network where `q` is a status prefix (Unreal-style): /owner works.
    fn raw(cmd: &str, args: &str) -> Option<String> {
        to_raw(cmd, args, "#chan", "qaohv")
    }

    /// A network where `q` is a list mode (Libera/solanum): /quiet works.
    fn raw_libera(cmd: &str, args: &str) -> Option<String> {
        to_raw(cmd, args, "#chan", "ohv")
    }

    #[test]
    fn mode_shortcuts_target_the_current_channel() {
        assert_eq!(raw("op", "bob").unwrap(), "MODE #chan +o bob");
        assert_eq!(raw("deop", "bob").unwrap(), "MODE #chan -o bob");
        assert_eq!(raw("voice", "bob").unwrap(), "MODE #chan +v bob");
        assert_eq!(raw("devoice", "bob").unwrap(), "MODE #chan -v bob");
        assert_eq!(raw("halfop", "bob").unwrap(), "MODE #chan +h bob");
        assert_eq!(raw("owner", "bob").unwrap(), "MODE #chan +q bob");
        assert_eq!(raw("admin", "bob").unwrap(), "MODE #chan +a bob");
        // A mode shortcut with no nick is not a valid command.
        assert!(raw("op", "").is_none());
    }

    #[test]
    fn plus_q_resolves_by_what_the_network_advertises() {
        // Unreal-style: `q` is a status prefix, so /owner promotes and /quiet
        // is NOT offered (it would mute nobody and promote someone instead).
        assert_eq!(raw("owner", "bob").unwrap(), "MODE #chan +q bob");
        assert!(raw("quiet", "bob").is_none(), "quiet must not mean +q here");

        // Libera-style: `q` is a list mode, so /quiet mutes and /owner is not
        // offered.
        assert_eq!(raw_libera("quiet", "bob").unwrap(), "MODE #chan +q bob");
        assert!(raw_libera("owner", "bob").is_none(), "owner must not mean +q here");
    }

    #[test]
    fn kick_carries_an_optional_reason() {
        assert_eq!(raw("kick", "bob").unwrap(), "KICK #chan bob");
        assert_eq!(raw("kick", "bob being rude").unwrap(), "KICK #chan bob :being rude");
    }

    #[test]
    fn topic_reads_without_args_and_sets_with_them() {
        assert_eq!(raw("topic", "").unwrap(), "TOPIC #chan");
        assert_eq!(raw("topic", "hello world").unwrap(), "TOPIC #chan :hello world");
    }

    #[test]
    fn mode_defaults_to_this_channel_but_accepts_an_explicit_one() {
        assert_eq!(raw("mode", "+t").unwrap(), "MODE #chan +t");
        assert_eq!(raw("mode", "#other +t").unwrap(), "MODE #other +t");
        assert_eq!(raw("mode", "").unwrap(), "MODE #chan");
    }

    #[test]
    fn invite_defaults_the_channel_and_accepts_an_explicit_one() {
        assert_eq!(raw("invite", "bob").unwrap(), "INVITE bob #chan");
        assert_eq!(raw("invite", "bob #other").unwrap(), "INVITE bob #other");
    }

    #[test]
    fn services_shortcuts_become_privmsgs() {
        assert_eq!(raw("ns", "identify hunter2").unwrap(), "PRIVMSG NickServ :identify hunter2");
        assert_eq!(raw("cs", "op #chan").unwrap(), "PRIVMSG ChanServ :op #chan");
        assert_eq!(raw("identify", "hunter2").unwrap(), "PRIVMSG NickServ :IDENTIFY hunter2");
    }

    #[test]
    fn quote_passes_through_verbatim_and_quit_quotes_its_reason() {
        assert_eq!(raw("quote", "PING :x").unwrap(), "PING :x");
        assert_eq!(raw("quit", "").unwrap(), "QUIT");
        assert_eq!(raw("quit", "bye all").unwrap(), "QUIT :bye all");
    }

    #[test]
    fn who_and_names_default_to_the_current_channel() {
        assert_eq!(raw("who", "").unwrap(), "WHO #chan");
        assert_eq!(raw("names", "").unwrap(), "NAMES #chan");
        assert_eq!(raw("who", "#other").unwrap(), "WHO #other");
    }

    #[test]
    fn unknown_commands_are_not_claimed() {
        assert!(raw("definitelynotacommand", "x").is_none());
    }

    #[test]
    fn text_expansions() {
        assert_eq!(to_message("shrug", "").unwrap(), "¯\\_(ツ)_/¯");
        assert_eq!(to_message("shrug", "oh well").unwrap(), "oh well ¯\\_(ツ)_/¯");
        assert!(to_message("tableflip", "").unwrap().contains("┻━┻"));
        assert!(to_message("nope", "").is_none());
    }

    #[test]
    fn slap_is_an_action_and_needs_a_target() {
        assert_eq!(
            to_action("slap", "bob").unwrap(),
            "slaps bob around a bit with a large trout"
        );
        assert!(to_action("slap", "").is_none());
    }
}
