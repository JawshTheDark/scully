//! Call signalling over IRC — the pure protocol half, always compiled.
//!
//! Presence for calls rides the network Scully already has: when you start a
//! call in a channel, we announce it with a CTCP so *other* Scully clients can
//! light up a "join" affordance. CTCP is the right carrier — it is the IRC
//! convention for structured, out-of-band client-to-client messages, and a
//! plain IRC client shows it as a harmless `LURKER-CALL` action rather than
//! spraying the channel with visible chatter.
//!
//! This module knows only the wire format: how to build the announcement and
//! how to recognise one coming back. It deliberately has no LiveKit or GTK
//! dependency, so it lives outside the `voice` feature gate and is unit-tested
//! in the ordinary test run — the same split as the rest of the protocol layer.

/// The CTCP verb we send/recognise. Uppercase per CTCP convention.
pub const CTCP_TYPE: &str = "LURKER-CALL";

/// A recognised call signal from a peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallSignal {
    /// A call is now open in this buffer; `room` is the SFU room name the token
    /// endpoint will hand out (informational — the client re-mints its own
    /// token rather than trusting a room name off the wire).
    Start { room: String },
    /// A call was ended by its announcer.
    End { room: String },
}

/// Build the CTCP payload for an announcement: returns `(ctcp_type, args)` ready
/// for `ClientVerb::Ctcp`. Only the outbound (call-starting) path uses this, so
/// it is dead code in a build without the `voice` feature — but it stays tested.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub fn announce(signal: &CallSignal) -> (String, String) {
    let args = match signal {
        CallSignal::Start { room } => format!("START {room}"),
        CallSignal::End { room } => format!("END {room}"),
    };
    (CTCP_TYPE.to_string(), args)
}

/// Parse an incoming CTCP into a [`CallSignal`], if it is one of ours.
///
/// Lenient by design: case-insensitive verb, tolerant of extra whitespace, and
/// ignores anything after the room token so a future field can't break an old
/// client. Returns `None` for any CTCP that isn't a well-formed LURKER-CALL —
/// the caller treats that as "not a call signal" and moves on.
pub fn parse(ctcp_type: &str, args: &str) -> Option<CallSignal> {
    if !ctcp_type.eq_ignore_ascii_case(CTCP_TYPE) {
        return None;
    }
    let mut it = args.split_whitespace();
    let verb = it.next()?;
    let room = it.next()?.to_string();
    if room.is_empty() {
        return None;
    }
    match verb.to_ascii_uppercase().as_str() {
        "START" => Some(CallSignal::Start { room }),
        "END" => Some(CallSignal::End { room }),
        _ => None,
    }
}

/// Convenience: parse straight from a full CTCP text like `"LURKER-CALL START
/// net-1-c-#dev"` (verb and args in one string), which is how some relays
/// deliver CTCP payloads.
pub fn parse_line(text: &str) -> Option<CallSignal> {
    let text = text.trim().trim_matches('\u{1}'); // strip CTCP \x01 delimiters if present
    let (ty, rest) = text.split_once(char::is_whitespace)?;
    parse(ty, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_round_trips_start() {
        let sig = CallSignal::Start { room: "net-7-c-#dev".into() };
        let (ty, args) = announce(&sig);
        assert_eq!(ty, "LURKER-CALL");
        assert_eq!(parse(&ty, &args), Some(sig));
    }

    #[test]
    fn announce_round_trips_end() {
        let sig = CallSignal::End { room: "net-7-d-alice-bob".into() };
        let (ty, args) = announce(&sig);
        assert_eq!(parse(&ty, &args), Some(sig));
    }

    #[test]
    fn verb_and_type_are_case_insensitive() {
        assert_eq!(
            parse("lurker-call", "start net-1-c-#x"),
            Some(CallSignal::Start { room: "net-1-c-#x".into() })
        );
    }

    #[test]
    fn tolerates_extra_whitespace_and_trailing_fields() {
        assert_eq!(
            parse(CTCP_TYPE, "  START   net-1-c-#x   future-field  "),
            Some(CallSignal::Start { room: "net-1-c-#x".into() })
        );
    }

    #[test]
    fn rejects_foreign_ctcp_and_malformed() {
        assert_eq!(parse("VERSION", "START x"), None);
        assert_eq!(parse(CTCP_TYPE, "START"), None); // no room
        assert_eq!(parse(CTCP_TYPE, "BOGUS net-1"), None); // unknown verb
        assert_eq!(parse(CTCP_TYPE, ""), None);
    }

    #[test]
    fn parse_line_handles_delimiters_and_joined_text() {
        assert_eq!(
            parse_line("\u{1}LURKER-CALL START net-2-c-#ops\u{1}"),
            Some(CallSignal::Start { room: "net-2-c-#ops".into() })
        );
        assert_eq!(parse_line("hello world"), None);
    }
}
