// Server → client frames (§7.1).
//
// Frames are discriminated by a top-level `kind`. The client → server direction
// uses `type` instead; that asymmetry is load-bearing (§1) and is why these are
// two separate enums rather than one.

use serde::{Deserialize, Serialize};

use crate::casefold::BufferKey;
use crate::de::{lenient, lenient_events};
use crate::event::MessageEvent;

/// A recent speaker, for nick autocomplete (`listSpeakers`,
/// `server/db/messages.ts:955`). Members currently present come from NAMES;
/// this adds people who spoke recently and have since left.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Speaker {
    #[serde(default)]
    pub nick: String,
    #[serde(default)]
    pub last_time: Option<f64>,
}

/// How to merge a `backlog` frame's `events` into what you already hold.
///
/// §8 rule 1: this is the server stating how it built the slice, and it is the
/// only field needed to decide *how* to merge. The legacy `reset` boolean is
/// deliberately not modelled — rule 2 shows it is undecodable on its own
/// (`false` means append on a resume gap but replace on a fresh connect and on
/// the system buffer: three meanings across two values).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BacklogMode {
    /// Authoritative slice, not contiguous with your tail. Overlapping older
    /// rows *may* be preserved — see §8 rule 5.
    Replace,
    /// A contiguous gap-fill: splice onto the existing tail.
    Append,
    /// "This buffer exists, nothing shipped." Must never wipe a buffer you
    /// already populated (§8 rule 3) — which is precisely why shells are their
    /// own mode rather than `replace` with an empty array.
    Shell,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BacklogFrame {
    pub network_id: Option<i64>,
    pub target: String,
    #[serde(default, deserialize_with = "lenient_events")]
    pub events: Vec<MessageEvent>,
    /// Absent on servers predating the field; see [`BacklogFrame::merge_mode`].
    #[serde(default)]
    pub mode: Option<BacklogMode>,
    /// Legacy. Read only by the pre-`mode` fallback, never on its own (§8 r2).
    #[serde(default)]
    pub reset: Option<bool>,
    /// Whether the scroll-up pager is still armed. §8 rule 4: record this on
    /// **every** frame including `append` and `shell`, or a gap-fill strands
    /// the buffer at whatever it happens to hold.
    #[serde(default)]
    pub has_more_older: bool,
    #[serde(default)]
    pub joined: bool,
    #[serde(default)]
    pub last_read_id: Option<i64>,
    #[serde(default)]
    pub unread: i64,
    #[serde(default)]
    pub highlights: i64,
    #[serde(default)]
    pub highlights_capped: bool,
    #[serde(default)]
    pub cleared_before_id: Option<i64>,
    #[serde(default)]
    pub cleared_at: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub speakers: Vec<Speaker>,
    #[serde(default)]
    pub input_history: Option<Vec<String>>,
}

impl BacklogFrame {
    pub fn key(&self) -> BufferKey {
        BufferKey::new(self.network_id, &self.target)
    }

    /// The merge mode, with the documented fallback for a server that predates
    /// the `mode` field (§8 rule 2).
    ///
    /// Order matters: the empty-and-more check must come **first**. An absent
    /// `reset` would otherwise fall through to `Replace` and un-hydrate a
    /// buffer that a shell was only meant to announce.
    pub fn merge_mode(&self) -> BacklogMode {
        if let Some(mode) = self.mode {
            return mode;
        }
        if self.events.is_empty() && self.has_more_older {
            return BacklogMode::Shell;
        }
        if self.network_id.is_some() && self.reset == Some(false) {
            BacklogMode::Append
        } else {
            BacklogMode::Replace
        }
    }
}

/// Which direction a `history` reply extends, mirroring the request mode (§8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryMode {
    /// Older page (scroll up) — prepend, dedupe by id.
    Before,
    /// Newer page while detached — append, dedupe by id.
    After,
    /// Jump-to-message window. Replaces the slice and **detaches** the buffer
    /// from live (§8): live events must not be spliced in until `latest`.
    Around,
    /// Newest slice. Hydrates a shell, reattaches to live, carries
    /// `inputHistory`.
    Latest,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryFrame {
    pub network_id: Option<i64>,
    pub target: String,
    pub mode: HistoryMode,
    /// Echo of the request token. §6: keep it monotonic and drop replies whose
    /// token you have already superseded.
    #[serde(default)]
    pub token: Option<i64>,
    #[serde(default, deserialize_with = "lenient_events")]
    pub events: Vec<MessageEvent>,
    #[serde(default, deserialize_with = "lenient")]
    pub speakers: Vec<Speaker>,
    #[serde(default)]
    pub has_more_older: bool,
    #[serde(default)]
    pub has_more_newer: bool,
    #[serde(default)]
    pub anchor_id: Option<i64>,
    #[serde(default)]
    pub anchor_missing: bool,
    #[serde(default)]
    pub input_history: Option<Vec<String>>,
}

impl HistoryFrame {
    pub fn key(&self) -> BufferKey {
        BufferKey::new(self.network_id, &self.target)
    }
}

/// Server-authoritative unread/highlight counts (§5.4).
///
/// Render verbatim; never count locally (§9.4).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadState {
    pub network_id: Option<i64>,
    pub target: String,
    #[serde(default)]
    pub last_read_id: Option<i64>,
    #[serde(default)]
    pub unread: i64,
    #[serde(default)]
    pub highlights: i64,
    #[serde(default)]
    pub highlights_capped: bool,
}

impl ReadState {
    pub fn key(&self) -> BufferKey {
        BufferKey::new(self.network_id, &self.target)
    }
}

// ── Network snapshot (§5.1) ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkState {
    Connecting,
    Connected,
    Reconnecting,
    #[default]
    Disconnected,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotChannel {
    pub name: String,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub modes: Option<String>,
    #[serde(default)]
    pub members: Vec<crate::event::Member>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwayState {
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub auto_set: bool,
    #[serde(default)]
    pub back_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerPresence {
    pub nick: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub state_at: Option<String>,
    #[serde(default)]
    pub away_message: Option<String>,
}

/// One network's live state inside the `snapshot` frame.
///
/// Note what is *not* here: display name and host. Those come from
/// `GET /api/networks` (§5.1), which the client fetches before opening the
/// socket — it doubles as a token-validity check.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSnapshot {
    pub network_id: i64,
    #[serde(default)]
    pub state: NetworkState,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub user_modes: Option<String>,
    #[serde(default)]
    pub lag_ms: Option<i64>,
    #[serde(default, deserialize_with = "lenient")]
    pub away: Option<AwayState>,
    /// **Not authoritative for buffer existence** (§4.3). Empty for any network
    /// without a live connection even though those networks still own persisted
    /// buffers, and read at the `'connected'` instant on a live one — before
    /// auto-rejoin JOINs land. Judging membership from it decides a user left
    /// channels they are still in.
    #[serde(default, deserialize_with = "lenient")]
    pub channels: Vec<SnapshotChannel>,
    #[serde(default, deserialize_with = "lenient")]
    pub peer_presence: std::collections::HashMap<String, PeerPresence>,
    #[serde(default)]
    pub pinned: Vec<String>,
    #[serde(default)]
    pub collapsed_nicklists: std::collections::HashMap<String, bool>,
    #[serde(default)]
    pub ignored_masks: Vec<serde_json::Value>,
    #[serde(default)]
    pub nick_notes: Vec<serde_json::Value>,
    #[serde(default)]
    pub relay_bots: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotFrame {
    #[serde(default)]
    pub protocol_version: Option<u32>,
    /// Largest file this account may send *right now*. Never hardcode a cap —
    /// it is the smallest of the 200 MB hard limit, the instance's transport
    /// limit, and the uploader's policy cap, so instances legitimately differ
    /// by a lot (§10 Uploads).
    #[serde(default)]
    pub max_upload_bytes: Option<u64>,
    #[serde(default)]
    pub networks: Vec<NetworkSnapshot>,
    #[serde(default)]
    pub global_ignores: Vec<serde_json::Value>,
    /// Present only on a fresh connect: the current global max message id.
    ///
    /// **Seed the resume cursor from it.** The shell backlogs that follow carry
    /// no rows, so without this the cursor would still be 0 after a full
    /// snapshot burst and the next reconnect would refetch everything.
    #[serde(default)]
    pub cursor: Option<i64>,
}

/// Every server → client frame.
///
/// [`ServerFrame::Unknown`] is mandatory, not defensive: §2 guarantees new
/// frame kinds may appear at any time without a version bump, and requires the
/// client to ignore what it does not recognize.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ServerFrame {
    Snapshot(SnapshotFrame),
    #[serde(rename = "draft-snapshot")]
    #[serde(rename_all = "camelCase")]
    DraftSnapshot {
        #[serde(default)]
        drafts: serde_json::Value,
    },
    #[serde(rename = "contacts-snapshot")]
    #[serde(rename_all = "camelCase")]
    ContactsSnapshot {
        #[serde(default)]
        contacts: Vec<serde_json::Value>,
    },
    /// Terminal marker of the connect burst. After it, **absence is proof**: a
    /// buffer key with no `backlog` frame is not open (§4.3). Without waiting
    /// for it, "no row yet" and "no such buffer" are indistinguishable and a
    /// navigate-by-key screen spins forever.
    BacklogComplete,
    Backlog(BacklogFrame),
    /// A live event. The envelope has already clobbered the inner `kind`.
    Irc(MessageEvent),
    History(HistoryFrame),
    ReadState(ReadState),
    #[serde(rename_all = "camelCase")]
    SendResult {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    /// Ack to the socket that asked **and** a fan-out to the user's other
    /// devices — the same frame for both. Focus only on a target you have an
    /// outstanding `open-buffer` for, or you drag the user to a buffer they
    /// opened on a different device (§4.3).
    #[serde(rename_all = "camelCase")]
    BufferOpened {
        network_id: Option<i64>,
        target: String,
    },
    #[serde(rename_all = "camelCase")]
    BufferClosed {
        network_id: Option<i64>,
        target: String,
    },
    #[serde(rename_all = "camelCase")]
    BufferReopened {
        network_id: Option<i64>,
        target: String,
    },
    #[serde(rename_all = "camelCase")]
    BufferCleared {
        network_id: Option<i64>,
        target: String,
        #[serde(default)]
        cleared_before_id: Option<i64>,
        #[serde(default)]
        cleared_at: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    PinsChanged {
        network_id: Option<i64>,
        #[serde(default)]
        pinned: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    NicklistCollapsedChanged {
        network_id: Option<i64>,
        target: String,
        #[serde(default)]
        collapsed: bool,
    },
    #[serde(rename_all = "camelCase")]
    ChannelNotifyChanged {
        network_id: Option<i64>,
        target: String,
        #[serde(default)]
        notify_always: bool,
    },
    #[serde(rename_all = "camelCase")]
    DraftUpdated {
        network_id: Option<i64>,
        target: String,
        #[serde(default)]
        body: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    InputHistoryAdded {
        network_id: Option<i64>,
        target: String,
        #[serde(default)]
        text: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    BookmarkUpdated {
        #[serde(default)]
        message_id: Option<i64>,
        #[serde(default)]
        bookmarked: bool,
    },
    NickNoteUpdated(serde_json::Value),
    RelayBotUpdated(serde_json::Value),
    ContactUpdated(serde_json::Value),
    ContactDeleted(serde_json::Value),
    IgnoreListUpdated(serde_json::Value),
    #[serde(rename_all = "camelCase")]
    Settings {
        #[serde(default)]
        changes: serde_json::Value,
        /// Present only when the upload cap was touched.
        #[serde(default)]
        max_upload_bytes: Option<u64>,
    },
    HighlightRulesChanged,
    #[serde(rename_all = "camelCase")]
    AccountState {
        #[serde(default)]
        paused: bool,
    },
    ChanlistState(serde_json::Value),
    ChanlistResult(serde_json::Value),
    #[serde(rename_all = "camelCase")]
    SearchResult {
        #[serde(default)]
        token: Option<i64>,
        /// Matching rows, newest first. Salvaged row-by-row like every other
        /// events array, so one odd row cannot cost the whole result page.
        #[serde(default, deserialize_with = "lenient_events")]
        results: Vec<MessageEvent>,
        #[serde(default)]
        has_more: bool,
        /// Echo of the request cursor, for paging older matches.
        #[serde(default)]
        before: Option<i64>,
    },
    #[serde(rename = "e2eExport")]
    E2eExport(serde_json::Value),
    #[serde(rename = "e2eImport")]
    E2eImport(serde_json::Value),
    DccTransfer(serde_json::Value),
    #[serde(rename_all = "camelCase")]
    UploadProgress {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        destination: Option<String>,
        #[serde(default)]
        percent: Option<f64>,
    },
    Export(serde_json::Value),
    /// Non-fatal by contract — also the reply to an unknown verb. The socket
    /// stays open (§2).
    #[serde(rename_all = "camelCase")]
    Error {
        #[serde(default)]
        text: Option<String>,
    },

    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ServerFrame {
        serde_json::from_str(s).expect("frame should parse")
    }

    #[test]
    fn unknown_frame_kind_is_not_fatal() {
        // §2: the client must ignore kinds it does not recognize rather than
        // erroring out, because new kinds ship without a version bump.
        assert!(matches!(
            parse(r##"{"kind":"time-travel","payload":{"x":1}}"##),
            ServerFrame::Unknown
        ));
    }

    #[test]
    fn live_irc_frame_loses_its_inner_kind() {
        // §5.3 "live-frame kind clobber": the envelope discriminator consumes
        // the `kind` slot, so the raw IRC command is gone here. This test pins
        // that we cannot accidentally start dispatching on it.
        let f = parse(r##"{"kind":"irc","type":"message","id":5,"networkId":1,"target":"#c"}"##);
        let ServerFrame::Irc(e) = f else { panic!("expected irc") };
        assert_eq!(e.kind, None);
        assert_eq!(e.event_type, crate::event::EventType::Message);
    }

    #[test]
    fn backlog_events_retain_their_raw_kind() {
        // Inside events[] nothing is tag-parsed, so `kind` survives.
        let f = parse(
            r##"{"kind":"backlog","networkId":1,"target":"#c","mode":"replace",
                "events":[{"type":"message","kind":"privmsg","id":1}]}"##,
        );
        let ServerFrame::Backlog(b) = f else { panic!("expected backlog") };
        assert_eq!(b.events[0].kind.as_deref(), Some("privmsg"));
    }

    #[test]
    fn merge_mode_prefers_explicit_mode() {
        let f = parse(r##"{"kind":"backlog","networkId":1,"target":"#c","mode":"append"}"##);
        let ServerFrame::Backlog(b) = f else { panic!() };
        assert_eq!(b.merge_mode(), BacklogMode::Append);
    }

    #[test]
    fn legacy_fallback_reads_shell_before_reset() {
        // §8 rule 2: on a pre-`mode` server an absent `reset` would fall
        // through to Replace and un-hydrate the buffer, so the shell test must
        // come first.
        let f = parse(r##"{"kind":"backlog","networkId":1,"target":"#c","hasMoreOlder":true}"##);
        let ServerFrame::Backlog(b) = f else { panic!() };
        assert_eq!(b.merge_mode(), BacklogMode::Shell);
    }

    #[test]
    fn legacy_fallback_appends_only_for_networked_reset_false() {
        let f = parse(
            r##"{"kind":"backlog","networkId":1,"target":"#c","reset":false,
                "events":[{"type":"message","id":1}]}"##,
        );
        let ServerFrame::Backlog(b) = f else { panic!() };
        assert_eq!(b.merge_mode(), BacklogMode::Append);

        // System buffer: reset:false means *replace* there, not append.
        let f = parse(
            r##"{"kind":"backlog","networkId":null,"target":":system:","reset":false,
                "events":[{"type":"system","id":1}]}"##,
        );
        let ServerFrame::Backlog(b) = f else { panic!() };
        assert_eq!(b.merge_mode(), BacklogMode::Replace);
    }

    #[test]
    fn parses_snapshot_cursor_and_upload_cap() {
        let f = parse(
            r##"{"kind":"snapshot","protocolVersion":1,"maxUploadBytes":1048576,
                "cursor":9182,"networks":[],"globalIgnores":[]}"##,
        );
        let ServerFrame::Snapshot(s) = f else { panic!() };
        assert_eq!(s.cursor, Some(9182));
        assert_eq!(s.max_upload_bytes, Some(1_048_576));
        assert_eq!(s.protocol_version, Some(1));
    }

    #[test]
    fn parses_kebab_case_frame_kinds() {
        assert!(matches!(parse(r##"{"kind":"backlog-complete"}"##), ServerFrame::BacklogComplete));
        assert!(matches!(
            parse(r##"{"kind":"read-state","networkId":1,"target":"#c","unread":3}"##),
            ServerFrame::ReadState(_)
        ));
        assert!(matches!(
            parse(r##"{"kind":"send-result","clientId":"a","ok":true}"##),
            ServerFrame::SendResult { .. }
        ));
        assert!(matches!(
            parse(r##"{"kind":"account-state","paused":true}"##),
            ServerFrame::AccountState { paused: true }
        ));
    }

    #[test]
    fn struct_variants_read_camel_case_field_names() {
        // Regression: the container's `rename_all = "kebab-case"` renames
        // VARIANTS, not their fields, so each struct variant needs its own
        // `rename_all = "camelCase"`. Without it `networkId` silently failed to
        // match `network_id` — and because a missing `Option` field
        // deserializes to `None` rather than erroring, the frame parsed
        // "successfully" with the network dropped. Every one of these carries a
        // networkId that must survive.
        let cases = [
            r##"{"kind":"buffer-opened","networkId":7,"target":"#c"}"##,
            r##"{"kind":"buffer-closed","networkId":7,"target":"#c"}"##,
            r##"{"kind":"buffer-reopened","networkId":7,"target":"#c"}"##,
            r##"{"kind":"buffer-cleared","networkId":7,"target":"#c","clearedBeforeId":3}"##,
            r##"{"kind":"pins-changed","networkId":7,"pinned":[]}"##,
            r##"{"kind":"nicklist-collapsed-changed","networkId":7,"target":"#c","collapsed":true}"##,
            r##"{"kind":"channel-notify-changed","networkId":7,"target":"#c","notifyAlways":true}"##,
            r##"{"kind":"draft-updated","networkId":7,"target":"#c","body":"x"}"##,
            r##"{"kind":"input-history-added","networkId":7,"target":"#c","text":"x"}"##,
        ];
        for case in cases {
            let got = match parse(case) {
                ServerFrame::BufferOpened { network_id, .. }
                | ServerFrame::BufferClosed { network_id, .. }
                | ServerFrame::BufferReopened { network_id, .. }
                | ServerFrame::BufferCleared { network_id, .. }
                | ServerFrame::PinsChanged { network_id, .. }
                | ServerFrame::NicklistCollapsedChanged { network_id, .. }
                | ServerFrame::ChannelNotifyChanged { network_id, .. }
                | ServerFrame::DraftUpdated { network_id, .. }
                | ServerFrame::InputHistoryAdded { network_id, .. } => network_id,
                other => panic!("wrong variant for {case}: {other:?}"),
            };
            assert_eq!(got, Some(7), "networkId lost while parsing {case}");
        }

        // Same hazard on the non-networked variants.
        let ServerFrame::SendResult { client_id, ok, .. } =
            parse(r##"{"kind":"send-result","clientId":"abc","ok":true}"##)
        else {
            panic!()
        };
        assert_eq!(client_id.as_deref(), Some("abc"));
        assert!(ok);

        let ServerFrame::BookmarkUpdated { message_id, .. } =
            parse(r##"{"kind":"bookmark-updated","messageId":55,"bookmarked":true}"##)
        else {
            panic!()
        };
        assert_eq!(message_id, Some(55));

        let ServerFrame::Settings { max_upload_bytes, .. } =
            parse(r##"{"kind":"settings","changes":{},"maxUploadBytes":123}"##)
        else {
            panic!()
        };
        assert_eq!(max_upload_bytes, Some(123));
    }

    #[test]
    fn history_with_object_speakers_parses_and_keeps_its_events() {
        // Captured live 2026-07-28: `speakers` is an array of OBJECTS
        // (`{nick, lastTime}` — server/db/messages.ts:963), not strings. Typed
        // as `Vec<String>` this failed the entire frame, so every real-server
        // hydrate dropped all its messages — while local testing passed,
        // because an empty `speakers: []` parses as either type. The shape here
        // is the one off the wire, not hand-written.
        let f = parse(
            r##"{"kind":"history","networkId":2,"target":"#ai","mode":"latest","token":1,
                "speakers":[{"nick":"dbr","lastTime":1785285000000},{"nick":"gekkie","lastTime":1785284000000}],
                "events":[{"id":100,"networkId":2,"target":"#ai","type":"message",
                           "nick":"dbr","text":"hello","kind":"privmsg","alt":true}],
                "hasMoreOlder":true,"hasMoreNewer":false,"hasMore":true,"before":null,
                "inputHistory":["/j #all-out-war"]}"##,
        );
        let ServerFrame::History(h) = f else { panic!("expected history") };
        assert_eq!(h.events.len(), 1, "the events must survive");
        assert_eq!(h.speakers.len(), 2);
        assert_eq!(h.speakers[0].nick, "dbr");
        assert!(h.has_more_older);
    }

    #[test]
    fn a_bad_event_row_costs_that_row_not_the_frame() {
        // The alt/speakers class of bug, contained: rows are salvaged
        // one-by-one, so a single undecodable row can no longer take 149
        // good messages with it.
        let f = parse(
            r##"{"kind":"history","networkId":1,"target":"#c","mode":"latest",
                "events":[{"id":1,"networkId":1,"target":"#c","type":"message","text":"good"},
                          {"id":2,"networkId":1,"target":"#c","type":"message","text":123456},
                          {"id":3,"networkId":1,"target":"#c","type":"message","text":"also good"}]}"##,
        );
        let ServerFrame::History(h) = f else { panic!() };
        assert_eq!(h.events.len(), 2, "bad row dropped, good rows kept");
        assert_eq!(h.events[0].id, Some(1));
        assert_eq!(h.events[1].id, Some(3));
    }

    #[test]
    fn a_bad_speakers_field_costs_speakers_not_the_frame() {
        let f = parse(
            r##"{"kind":"history","networkId":1,"target":"#c","mode":"latest",
                "speakers":"totally-wrong-shape",
                "events":[{"id":1,"networkId":1,"target":"#c","type":"message","text":"kept"}]}"##,
        );
        let ServerFrame::History(h) = f else { panic!() };
        assert!(h.speakers.is_empty(), "unparseable decoration falls back to default");
        assert_eq!(h.events.len(), 1, "the payload survives");
    }

    #[test]
    fn parses_camel_case_e2e_frames() {
        // These two are camelCase where every other multiword kind is kebab —
        // a historical wart that must be matched exactly.
        assert!(matches!(parse(r##"{"kind":"e2eExport"}"##), ServerFrame::E2eExport(_)));
        assert!(matches!(parse(r##"{"kind":"e2eImport"}"##), ServerFrame::E2eImport(_)));
    }
}
