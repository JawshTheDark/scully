// `MessageEvent` — the row shape shared by live `irc` frames and the `events[]`
// arrays inside `backlog` / `history` (§5.3, §7.2).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The inner `type` field of an event.
///
/// Carries an [`EventType::Other`] escape hatch because §2 makes unknown event
/// types explicitly non-fatal and additive: the server may introduce new ones at
/// any time without a `protocolVersion` bump. Keeping the raw string (rather
/// than collapsing to a unit "unknown") costs nothing and makes an unrecognized
/// type visible in logs instead of silently indistinguishable from every other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EventType {
    // ── Persisted (carry `id`, advance the resume cursor) ──
    Message,
    Action,
    Notice,
    Join,
    Part,
    Quit,
    Kick,
    Nick,
    Mode,
    Topic,
    Motd,
    Error,
    Chghost,
    /// Persisted in its own table with its own id sequence (§4.4).
    System,
    /// Ephemeral to you, persisted in the op-visibility variant (§7.2).
    Invite,

    // ── Ephemeral (no `id`, never advance the cursor) ──
    OwnNick,
    Usermode,
    ChannelTopic,
    ChannelModes,
    ChannelJoined,
    ChannelParted,
    JoinError,
    Names,
    MemberUpdate,
    State,
    AwayState,
    PeerPresence,
    Typing,
    Lag,
    Ctcp,
    E2e,
    WhoisResult,
    ChanlistStart,
    ChanlistProgress,
    ChanlistEnd,

    Other(String),
}

impl EventType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Message => "message",
            Self::Action => "action",
            Self::Notice => "notice",
            Self::Join => "join",
            Self::Part => "part",
            Self::Quit => "quit",
            Self::Kick => "kick",
            Self::Nick => "nick",
            Self::Mode => "mode",
            Self::Topic => "topic",
            Self::Motd => "motd",
            Self::Error => "error",
            Self::Chghost => "chghost",
            Self::System => "system",
            Self::Invite => "invite",
            Self::OwnNick => "own-nick",
            Self::Usermode => "usermode",
            Self::ChannelTopic => "channel-topic",
            Self::ChannelModes => "channel-modes",
            Self::ChannelJoined => "channel-joined",
            Self::ChannelParted => "channel-parted",
            Self::JoinError => "join-error",
            Self::Names => "names",
            Self::MemberUpdate => "member-update",
            Self::State => "state",
            Self::AwayState => "away-state",
            Self::PeerPresence => "peer-presence",
            Self::Typing => "typing",
            Self::Lag => "lag",
            Self::Ctcp => "ctcp",
            Self::E2e => "e2e",
            Self::WhoisResult => "whois_result",
            Self::ChanlistStart => "chanlist-start",
            Self::ChanlistProgress => "chanlist-progress",
            Self::ChanlistEnd => "chanlist-end",
            Self::Other(s) => s,
        }
    }

    /// True for the types that produce a message the user reads, as opposed to
    /// presence churn or pure state signals.
    pub fn is_chat(&self) -> bool {
        matches!(self, Self::Message | Self::Action | Self::Notice)
    }

    /// Types that fold into a consolidated summary line (`CONSOLIDATABLE_TYPES`
    /// in `shared/consolidate.ts` — the canonical set, §8).
    pub fn is_consolidatable(&self) -> bool {
        matches!(self, Self::Join | Self::Part | Self::Quit | Self::Nick | Self::Chghost)
    }

    /// `NOISE_TYPES` in `shared/eventFilter.ts`: the consolidatable set plus
    /// `mode`. This is what the `none` event tier hides entirely.
    ///
    /// Deliberately excludes `kick`, `topic`, `invite`, `error` and `motd` —
    /// those are things that happened, not churn, and hiding them would make
    /// the buffer lie about its history rather than merely be quieter.
    pub fn is_noise(&self) -> bool {
        self.is_consolidatable() || matches!(self, Self::Mode)
    }
}

/// Exists only so [`MessageEvent`] can derive `Default` for construction in
/// tests and builders. Deserialization never produces it: `type` is a required
/// field on every event, so there is no wire path to a "default" type.
impl Default for EventType {
    fn default() -> Self {
        Self::Other(String::new())
    }
}

impl From<&str> for EventType {
    fn from(s: &str) -> Self {
        match s {
            "message" => Self::Message,
            "action" => Self::Action,
            "notice" => Self::Notice,
            "join" => Self::Join,
            "part" => Self::Part,
            "quit" => Self::Quit,
            "kick" => Self::Kick,
            "nick" => Self::Nick,
            "mode" => Self::Mode,
            "topic" => Self::Topic,
            "motd" => Self::Motd,
            "error" => Self::Error,
            "chghost" => Self::Chghost,
            "system" => Self::System,
            "invite" => Self::Invite,
            "own-nick" => Self::OwnNick,
            "usermode" => Self::Usermode,
            "channel-topic" => Self::ChannelTopic,
            "channel-modes" => Self::ChannelModes,
            "channel-joined" => Self::ChannelJoined,
            "channel-parted" => Self::ChannelParted,
            "join-error" => Self::JoinError,
            "names" => Self::Names,
            "member-update" => Self::MemberUpdate,
            "state" => Self::State,
            "away-state" => Self::AwayState,
            "peer-presence" => Self::PeerPresence,
            "typing" => Self::Typing,
            "lag" => Self::Lag,
            "ctcp" => Self::Ctcp,
            "e2e" => Self::E2e,
            "whois_result" => Self::WhoisResult,
            "chanlist-start" => Self::ChanlistStart,
            "chanlist-progress" => Self::ChanlistProgress,
            "chanlist-end" => Self::ChanlistEnd,
            other => Self::Other(other.to_string()),
        }
    }
}

impl Serialize for EventType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from(String::deserialize(d)?.as_str()))
    }
}

/// A channel member as it appears in `names`, `member-update` and the network
/// snapshot's nicklists.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub nick: String,
    /// Prefix-mode **letters**, highest first (`q a o h v`) — *not* sigils
    /// (`~ & @ % +`). §5.1 is explicit that mapping to sigils is the client's
    /// job; rendering these raw would show `o` where the user expects `@`.
    #[serde(default)]
    pub modes: Vec<String>,
    #[serde(default)]
    pub away: bool,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
}

impl Member {
    /// The member's highest prefix mode letter, ranked `q a o h v`.
    ///
    /// Ranked by SCANNING, never by taking `modes[0]`: the array is in
    /// grant order, so someone op'd first and given owner later reads
    /// `["o","q"]` — and `first()` showed them as `@` until they happened to
    /// lose the op. (Matches the web's `memberPrefix.ts`.)
    pub fn highest_mode(&self) -> Option<&str> {
        for letter in ["q", "a", "o", "h", "v"] {
            if self.modes.iter().any(|m| m == letter) {
                return Some(letter);
            }
        }
        None
    }

    /// Map the highest prefix mode to its display sigil.
    pub fn sigil(&self) -> Option<char> {
        match self.highest_mode() {
            Some("q") => Some('~'),
            Some("a") => Some('&'),
            Some("o") => Some('@'),
            Some("h") => Some('%'),
            Some("v") => Some('+'),
            _ => None,
        }
    }
}

/// One event row.
///
/// Every field beyond `event_type` is optional because this single shape covers
/// ~35 event types plus their type-specific extras; absence is normal, not an
/// error. Unknown fields are ignored by default (no `deny_unknown_fields`) —
/// required by §2's additive-evolution rule.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    /// Absent on ephemeral events. Present rows come from one global monotonic
    /// sequence — except system-buffer rows, which have their own (§4.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,

    /// `None` for system-buffer rows. Also the flag that keeps the resume
    /// cursor correct: only `Some` ids may advance it.
    #[serde(default)]
    pub network_id: Option<i64>,

    /// Absent on `motd` / `error` / `ctcp` / `e2e`, which route to the
    /// network's `:server:` buffer instead (§7.3).
    #[serde(default)]
    pub target: Option<String>,

    #[serde(rename = "type")]
    pub event_type: EventType,

    /// IRCv3 server-time where the network offers it, receive time otherwise.
    /// **Not monotonic with respect to `id`** — a bouncer upstream can replay
    /// old messages live — so order and dedupe by `id`, never by this (§5.3).
    #[serde(default)]
    pub time: Option<String>,

    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub text: Option<String>,

    /// The raw IRC command (`privmsg`, `action`, `notice`, …).
    ///
    /// **Survives only inside `backlog`/`history` `events[]`.** On a live frame
    /// the envelope discriminator overwrites it (§5.3's "live-frame `kind`
    /// clobber"), so this is `None` there. Dispatch on `event_type`, never on
    /// this.
    #[serde(default)]
    pub kind: Option<String>,

    /// You sent it, from any of your clients.
    #[serde(default, rename = "self")]
    pub is_self: bool,

    #[serde(default)]
    pub userhost: Option<String>,
    /// Alternating display flag (`messages.alt`, stored 0/1, sent as a JSON
    /// **boolean**) used to stripe consecutive rows.
    ///
    /// Typed wrong once, as a string — and because a single mistyped field
    /// fails the whole event, that dropped every `irc`, `backlog` and `history`
    /// frame carrying messages while shells (which have no events) still
    /// parsed. The result looked exactly like a server that sends buffer names
    /// and no content. Hence [`crate::event::tests::real_captured_frames_parse`].
    #[serde(default)]
    pub alt: bool,
    /// Set on the `:server:` copy of a notice to a closed/absent DM (§9.1).
    #[serde(default)]
    pub mirrored: bool,
    #[serde(default)]
    pub dm: bool,

    // ── Highlight / notify decoration ──
    #[serde(default)]
    pub matched: bool,
    #[serde(default)]
    pub matched_rule_id: Option<i64>,
    #[serde(default)]
    pub from_ignored: bool,
    #[serde(default)]
    pub notify_always: bool,
    /// **The single flag to gate a live alert on** (§5.3).
    ///
    /// The server has already unioned the content signals and applied the
    /// user's mute/ignore verdict. A muted-channel highlight arrives as
    /// `matched: true, notify: false` — style it as a highlight, but raise no
    /// notification. Do not re-derive this client-side: push fires when no
    /// client is attached, so the veto cannot live in a client.
    #[serde(default)]
    pub notify: bool,

    /// IRCv3 message id. Absent — not null — on untagged networks.
    #[serde(default)]
    pub msgid: Option<String>,
    /// `true` when saved; **absent rather than `false`** otherwise (§5.3), so
    /// the unsaved majority costs nothing on the wire.
    #[serde(default)]
    pub bookmarked: Option<bool>,

    // ── Type-specific extras ──
    #[serde(default)]
    pub new_nick: Option<String>,
    #[serde(default)]
    pub kicked: Option<String>,
    #[serde(default)]
    pub modes: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "crate::de::lenient")]
    pub members: Option<Vec<Member>>,
    #[serde(default, deserialize_with = "crate::de::lenient")]
    pub member: Option<Member>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub unknown_command: Option<String>,
    /// Severity for `system` and `e2e` rows: `info` | `warn` | `error`.
    #[serde(default)]
    pub level: Option<String>,
    /// Network connection state on `state`; peer state on `peer-presence`.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub state_at: Option<String>,
    #[serde(default)]
    pub away_message: Option<String>,
    #[serde(default)]
    pub came_online: Option<bool>,
    #[serde(default)]
    pub lag_ms: Option<i64>,
    #[serde(default)]
    pub new_ident: Option<String>,
    #[serde(default)]
    pub new_host: Option<String>,
    /// The structured whois payload on a `whois_result` event (the
    /// irc-framework whois shape: nick, ident, hostname, real_name, server,
    /// channels, idle, account, …). Rendered by the client, not persisted.
    #[serde(default)]
    pub whois: Option<serde_json::Value>,
}

impl MessageEvent {
    /// True when this row persists and therefore carries an `id`.
    pub fn is_persisted(&self) -> bool {
        self.id.is_some()
    }

    /// Whether this event may advance the resume cursor (§4.4).
    ///
    /// Two independent gates: ephemeral rows have no id, and **system-buffer
    /// rows live in a separate id sequence** — feeding one of those into the
    /// cursor corrupts resume for every other buffer, which is why this is a
    /// method rather than an `id.is_some()` check at each call site.
    pub fn advances_cursor(&self) -> bool {
        self.id.is_some() && self.network_id.is_some()
    }

    /// The buffer this event belongs to.
    ///
    /// Server-voice events (`motd`, `error`, `ctcp`, `e2e`) arrive with no
    /// target and route to the network's `:server:` log; `system` events route
    /// to the app-scoped `:system:` buffer (§7.3).
    pub fn buffer_key(&self) -> BufferKey {
        match (&self.target, self.network_id) {
            (Some(t), net) => BufferKey::new(net, t),
            (None, Some(net)) => BufferKey::server(net),
            (None, None) => BufferKey::system(),
        }
    }
}

use crate::casefold::BufferKey;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_event_type_round_trips_and_is_not_fatal() {
        let e: MessageEvent = serde_json::from_str(r#"{"type":"quantum-flux","id":7}"#).unwrap();
        assert_eq!(e.event_type, EventType::Other("quantum-flux".into()));
        assert_eq!(e.event_type.as_str(), "quantum-flux");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // §2: new fields may appear at any time; they must not break parsing.
        let e: MessageEvent =
            serde_json::from_str(r#"{"type":"message","someFutureField":{"a":1}}"#).unwrap();
        assert_eq!(e.event_type, EventType::Message);
    }

    #[test]
    fn only_networked_persisted_rows_advance_the_cursor() {
        let live = MessageEvent { id: Some(42), network_id: Some(1), ..Default::default() };
        assert!(live.advances_cursor());

        // System buffer: separate id sequence — must never touch the cursor.
        let sys = MessageEvent { id: Some(999_999), network_id: None, ..Default::default() };
        assert!(!sys.advances_cursor());

        // Ephemeral: no id at all.
        let eph = MessageEvent { id: None, network_id: Some(1), ..Default::default() };
        assert!(!eph.advances_cursor());
    }

    #[test]
    fn routes_untargeted_server_voice_to_the_server_log() {
        let motd = MessageEvent {
            event_type: EventType::Motd,
            network_id: Some(3),
            target: None,
            ..Default::default()
        };
        assert_eq!(motd.buffer_key(), BufferKey::server(3));

        let sys = MessageEvent {
            event_type: EventType::System,
            network_id: None,
            target: None,
            ..Default::default()
        };
        assert_eq!(sys.buffer_key(), BufferKey::system());
    }

    #[test]
    fn buffer_key_folds_target_case() {
        let e = MessageEvent {
            network_id: Some(1),
            target: Some("#Lurker".into()),
            ..Default::default()
        };
        assert_eq!(e.buffer_key(), BufferKey::new(Some(1), "#lurker"));
    }

    /// Frames captured verbatim from a live v1.1.5 server.
    ///
    /// Guessing a field's type from the protocol doc's prose is how `alt`
    /// became a `String`; the doc lists it among "userhost, alt, mirrored, dm"
    /// without types. Because serde fails the *whole* event on one bad field,
    /// that silently dropped every frame carrying messages. These are real
    /// bytes off the wire, so the types are observed rather than inferred.
    #[test]
    fn real_captured_frames_parse() {
        // A live `irc` message echo, including the `alt` boolean.
        let live = r##"{"type":"message","target":"#TestChan","nick":"reprouser",
            "text":"probe message 1","kind":"irc","self":true,
            "userhost":"reprouser!~u@smkg34u5ggm8e.irc","time":"2026-07-29T00:32:44.205Z",
            "msgid":"2napr9h2f5mxixcjdw7qiee7u2","userId":1,"networkId":1,"id":26,
            "alt":true,"matched":false,"matchedRuleId":null,"fromIgnored":false,
            "dm":false,"notifyAlways":false,"notify":false}"##;
        let e: MessageEvent = serde_json::from_str(live).expect("live irc event must parse");
        assert!(e.alt);
        assert!(e.is_self);
        assert_eq!(e.id, Some(26));
        assert_eq!(e.text.as_deref(), Some("probe message 1"));
        // `userId` is an extra field this client does not model — it must be
        // ignored, not rejected (§2).
        assert_eq!(e.event_type, EventType::Message);

        // A stored row from a `history` reply: `kind` survives here, and
        // `alt` is false rather than absent.
        let stored = r##"{"id":23,"networkId":1,"target":"#TestChan",
            "time":"2026-07-29T00:31:16.088Z","type":"message","nick":"reprouser",
            "text":"probe message 1","kind":"privmsg","self":true,
            "userhost":"reprouser!~u@smkg34u5ggm8e.irc","alt":false,"matched":false,
            "matchedRuleId":null,"fromIgnored":false,"mirrored":false,
            "msgid":"z6ufx7e2fkhyqrjv9thed4nfgs","dm":false,"notifyAlways":false,
            "notify":false}"##;
        let e: MessageEvent = serde_json::from_str(stored).expect("stored row must parse");
        assert!(!e.alt);
        assert_eq!(e.kind.as_deref(), Some("privmsg"));

        // A server-log row: empty-string `kind`, null nick/text, and the
        // id-suffixed target.
        let server_row = r##"{"id":19,"networkId":1,"target":":server:1",
            "time":"2026-07-29T00:30:24.667Z","type":"usermode","nick":null,"text":null,
            "kind":"","self":false,"userhost":null,"alt":false,"matched":false,
            "matchedRuleId":null,"fromIgnored":false,"mirrored":false,"dm":false,
            "notifyAlways":false,"notify":false}"##;
        let e: MessageEvent = serde_json::from_str(server_row).expect("server row must parse");
        assert_eq!(e.event_type, EventType::Usermode);
        assert_eq!(e.buffer_key(), BufferKey::server(1));
    }

    #[test]
    fn member_maps_mode_letters_to_sigils() {
        let m = Member { nick: "a".into(), modes: vec!["o".into()], ..Default::default() };
        assert_eq!(m.sigil(), Some('@'));
        assert_eq!(Member { nick: "b".into(), ..Default::default() }.sigil(), None);
    }

    #[test]
    fn noise_set_is_consolidatable_plus_mode() {
        assert!(EventType::Join.is_noise());
        assert!(EventType::Mode.is_noise());
        assert!(!EventType::Mode.is_consolidatable());
        // Content, not churn — never hidden by the tier.
        assert!(!EventType::Kick.is_noise());
        assert!(!EventType::Topic.is_noise());
        assert!(!EventType::Invite.is_noise());
    }
}

#[cfg(test)]
mod member_tests {
    use super::*;

    #[test]
    fn the_highest_mode_wins_regardless_of_grant_order() {
        // Reported live: op'd first, owner'd second → ["o","q"] → shown as @
        // until -o removed the op and the ~ finally appeared.
        let m = Member { modes: vec!["o".into(), "q".into()], ..Default::default() };
        assert_eq!(m.highest_mode(), Some("q"));
        assert_eq!(m.sigil(), Some('~'));

        let m = Member { modes: vec!["v".into(), "h".into()], ..Default::default() };
        assert_eq!(m.sigil(), Some('%'));

        let m = Member { modes: vec![], ..Default::default() };
        assert_eq!(m.sigil(), None);
    }
}
