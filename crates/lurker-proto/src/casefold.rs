// Buffer identity and case folding.
//
// Protocol §5.2 / §9.2: IRC targets are case-insensitive and servers echo
// inconsistently-cased names, so identity is `(networkId, case-folded target)`.
// Folding is **ASCII `to_lowercase` only**. RFC 1459 casemapping (which treats
// `{}|^` as the lowercase of `[]\~`) is deliberately NOT implemented anywhere
// in Lurker, and the doc is explicit that a client must match that rather than
// "fix" it unilaterally — doing so would split identity with the server and
// every other client.
//
// The two sentinel targets are exact-match and never folded.

use std::fmt;

/// Per-network server log.
///
/// The protocol doc describes this target as literally `":server:"`, but a live
/// v1.1.5 server actually sends `":server:<networkId>"` (e.g. `:server:1`), and
/// `wsHub.ts` tests it with `startsWith(':server:')`. Both forms are therefore
/// treated as the same buffer: [`fold`] normalises any `:server:*` target to
/// this constant, so the key stays `(network_id, ":server:")` — unique per
/// network either way — while the buffer's `display_name` keeps the exact form
/// the server used, which is what goes back on the wire.
///
/// Getting this wrong splits one server log into two buffers: the backlog
/// creates `:server:1`, then untargeted `motd`/`error` events (§7.3), which
/// carry no target at all, land in a second, separate `:server:`.
pub const SERVER_TARGET: &str = ":server:";

/// App-scoped Lurker log. Always carries `network_id: None` and has its own
/// message id sequence (§4.4) — never feed its ids into the resume cursor.
pub const SYSTEM_TARGET: &str = ":system:";

/// True for a per-network server log, in either the bare or id-suffixed form.
pub fn is_server_target(target: &str) -> bool {
    target.starts_with(SERVER_TARGET)
}

/// True for the sentinel targets, which are never case-folded as names.
pub fn is_sentinel(target: &str) -> bool {
    target == SYSTEM_TARGET || is_server_target(target)
}

/// Fold a target to its identity form.
///
/// ASCII lowercase for ordinary names (§9.2). `:system:` is exact. Any
/// `:server:*` normalises to [`SERVER_TARGET`] so the id-suffixed and bare
/// forms resolve to one buffer per network.
pub fn fold(target: &str) -> String {
    if target == SYSTEM_TARGET {
        target.to_string()
    } else if is_server_target(target) {
        SERVER_TARGET.to_string()
    } else {
        target.to_ascii_lowercase()
    }
}

/// True if `target` names a channel rather than a nick.
///
/// Matters because `open-buffer` on an unjoined `#channel` JOINs it (§4.3), so
/// the distinction gates a write with side effects visible on every device.
pub fn is_channel(target: &str) -> bool {
    matches!(target.chars().next(), Some('#' | '&' | '!' | '+'))
}

/// Identity of one buffer: a network (or `None` for the app-scoped system
/// buffer) plus a folded target.
///
/// Construct via [`BufferKey::new`] so folding can never be skipped. Display
/// casing is kept separately on the buffer itself, not here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BufferKey {
    pub network_id: Option<i64>,
    pub target: String,
}

impl BufferKey {
    pub fn new(network_id: Option<i64>, target: &str) -> Self {
        Self { network_id, target: fold(target) }
    }

    /// The app-scoped system buffer (`network_id: None`, `:system:`).
    pub fn system() -> Self {
        Self { network_id: None, target: SYSTEM_TARGET.to_string() }
    }

    /// A network's server log (`:server:`).
    pub fn server(network_id: i64) -> Self {
        Self { network_id: Some(network_id), target: SERVER_TARGET.to_string() }
    }

    pub fn is_system(&self) -> bool {
        self.network_id.is_none() && self.target == SYSTEM_TARGET
    }

    pub fn is_server_log(&self) -> bool {
        self.target == SERVER_TARGET
    }

    pub fn is_channel(&self) -> bool {
        is_channel(&self.target)
    }

    /// True when this buffer is a DCC chat, conventionally named `=nick`.
    /// These are peer-to-peer chats, distinct from a server-relayed DM.
    pub fn is_dcc(&self) -> bool {
        self.network_id.is_some() && self.target.starts_with('=')
    }

    /// True when this buffer is a direct message: a real network buffer that is
    /// neither a channel, a sentinel, nor a DCC chat. DCC chats are a separate
    /// category (`=nick`) even though they are also one-to-one.
    pub fn is_dm(&self) -> bool {
        self.network_id.is_some()
            && !self.is_channel()
            && !self.is_dcc()
            && !is_sentinel(&self.target)
    }
}

impl fmt::Display for BufferKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.network_id {
            Some(id) => write!(f, "{id}/{}", self.target),
            None => write!(f, "-/{}", self.target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_only() {
        assert_eq!(fold("#Lurker"), "#lurker");
        assert_eq!(fold("AmIantos"), "amiantos");
    }

    #[test]
    fn does_not_apply_rfc1459_casemapping() {
        // `{}|^` must NOT fold onto `[]\~`. Lurker deliberately omits RFC 1459
        // casemapping; matching that is a protocol requirement (§9.2), so this
        // test pins the *absence* of the "fix".
        assert_eq!(fold("nick[]\\~"), "nick[]\\~");
        assert_ne!(fold("#chan[]"), fold("#chan{}"));
    }

    #[test]
    fn sentinels_are_exact_match() {
        assert_eq!(fold(SERVER_TARGET), SERVER_TARGET);
        assert_eq!(fold(SYSTEM_TARGET), SYSTEM_TARGET);
    }

    #[test]
    fn id_suffixed_server_target_folds_onto_the_bare_one() {
        // A live v1.1.5 server sends `:server:1`, while §7.3 events with no
        // target at all route to the network's server log. Both must land in
        // ONE buffer, or the log silently splits in two.
        assert_eq!(fold(":server:1"), SERVER_TARGET);
        assert_eq!(fold(":server:42"), SERVER_TARGET);
        assert_eq!(BufferKey::new(Some(1), ":server:1"), BufferKey::server(1));
        // Still distinct across networks.
        assert_ne!(BufferKey::new(Some(1), ":server:1"), BufferKey::new(Some(2), ":server:2"));
        assert!(BufferKey::new(Some(1), ":server:1").is_server_log());
        assert!(!BufferKey::new(Some(1), ":server:1").is_dm());
    }

    #[test]
    fn key_folds_on_construction() {
        assert_eq!(BufferKey::new(Some(1), "#Chat"), BufferKey::new(Some(1), "#chat"));
        assert_ne!(BufferKey::new(Some(1), "#chat"), BufferKey::new(Some(2), "#chat"));
    }

    #[test]
    fn classifies_buffer_kinds() {
        assert!(BufferKey::new(Some(1), "#chat").is_channel());
        assert!(BufferKey::new(Some(1), "&local").is_channel());
        assert!(BufferKey::new(Some(1), "amiantos").is_dm());
        assert!(!BufferKey::server(1).is_dm());
        assert!(!BufferKey::system().is_dm());
        assert!(BufferKey::system().is_system());
    }

    #[test]
    fn dcc_chats_are_a_distinct_category() {
        // `=nick` is a DCC chat, not a plain DM and not a channel.
        let dcc = BufferKey::new(Some(1), "=amiantos");
        assert!(dcc.is_dcc());
        assert!(!dcc.is_dm(), "DCC is not a plain DM");
        assert!(!dcc.is_channel());
        // A normal DM is not DCC.
        assert!(!BufferKey::new(Some(1), "amiantos").is_dcc());
    }
}
