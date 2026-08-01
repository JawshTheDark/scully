// Client → server verbs (§6).
//
// Discriminated by `type` (the server → client direction uses `kind`; §1).
// Verbs marked ⏸ in the doc are rejected while an account is paused — every
// write is. The read verbs, notably `History`, stay available, which is what
// lets a paused account still be shown its history.

use serde::Serialize;

use crate::casefold::BufferKey;
use crate::frame::HistoryMode;

/// What a `history` request's `limit` counts (§8, `shared/eventFilter.ts`).
///
/// **Ask for the unit you actually render in.** `limit` counts stored rows by
/// default, so a client that folds presence runs can request 100 rows out of a
/// netsplit and render three visible lines — then fetch again, and the user
/// watches the buffer assemble itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PageUnit {
    /// Every stored row. Correct when each event renders on its own line.
    Event,
    /// Rows outside the consolidatable set. Correct when runs are folded.
    Renderable,
    /// Rows outside the noise set. Correct at the `none` event tier.
    Chat,
}

/// The client's event-noise tier, resolved client-side (§8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EventMode {
    #[default]
    All,
    Smart,
    None,
}

impl EventMode {
    /// The page unit to request for this tier.
    ///
    /// There is deliberately no unit for `Smart`: which events it hides depends
    /// on who spoke recently in *this* client, which the server cannot know —
    /// so it asks for the unit it would otherwise use and accepts an
    /// occasionally short page.
    pub fn page_unit(self, consolidate: bool) -> PageUnit {
        match self {
            Self::None => PageUnit::Chat,
            _ if consolidate => PageUnit::Renderable,
            _ => PageUnit::Event,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientVerb {
    // ── Sending ⏸ ──
    /// PRIVMSG. Acked by `send-result` **only** if `client_id` is present.
    #[serde(rename_all = "camelCase")]
    Send {
        network_id: i64,
        target: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// CTCP ACTION (`/me`).
    #[serde(rename_all = "camelCase")]
    Action {
        network_id: i64,
        target: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Notice {
        network_id: i64,
        target: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Raw IRC line — the escape hatch for `/mode`, `/kick`, `/whois` and any
    /// command without a typed verb. Slash commands are parsed **client-side**;
    /// the server does not interpret `/` in `send` text (§12).
    #[serde(rename_all = "camelCase")]
    Raw { network_id: i64, line: String },
    /// CTCP request (`/ping`, `/version` at a user). `issuing_target` is the
    /// buffer where the status line should surface (§6).
    #[serde(rename_all = "camelCase")]
    Ctcp {
        network_id: i64,
        target: String,
        ctcp_type: String,
        args: String,
        issuing_target: String,
    },

    // ── Channels & buffers ⏸ ──
    /// Request only. The buffer materializes on `channel-joined`, never on this
    /// (§9.1) — record it as a pending intent with a timeout.
    #[serde(rename_all = "camelCase")]
    Join {
        network_id: i64,
        channel: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Part {
        network_id: i64,
        channel: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// **A write.** Reopens a closed buffer, mints a DM row for a bare nick, and
    /// JOINs an unjoined channel — all persisted and announced to the user's
    /// other devices. Reserve it for explicit user intent; hydrate with
    /// [`ClientVerb::History`] `Latest` instead (§4.3).
    #[serde(rename_all = "camelCase")]
    OpenBuffer {
        network_id: Option<i64>,
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        count_by: Option<PageUnit>,
    },
    #[serde(rename_all = "camelCase")]
    CloseBuffer {
        network_id: Option<i64>,
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    // ── View state (persisted, fanned out to the user's other devices) ──
    /// MAX-clamped server-side and idempotent, so re-sending a stale id is
    /// safe. For the system buffer send an explicit `network_id: None` rather
    /// than omitting it.
    #[serde(rename_all = "camelCase")]
    MarkRead { network_id: Option<i64>, target: String, message_id: i64 },
    MarkAllRead,
    #[serde(rename_all = "camelCase")]
    ClearBuffer { network_id: Option<i64>, target: String },
    #[serde(rename_all = "camelCase")]
    UnclearBuffer { network_id: Option<i64>, target: String },
    #[serde(rename_all = "camelCase")]
    PinBuffer { network_id: Option<i64>, target: String },
    #[serde(rename_all = "camelCase")]
    UnpinBuffer { network_id: Option<i64>, target: String },
    #[serde(rename_all = "camelCase")]
    ReorderPins { network_id: i64, targets: Vec<String> },
    #[serde(rename_all = "camelCase")]
    SetNicklistCollapsed { network_id: i64, target: String, collapsed: bool },
    #[serde(rename_all = "camelCase")]
    SetChannelNotifyAlways { network_id: i64, target: String, notify_always: bool },
    #[serde(rename_all = "camelCase")]
    DraftSet { network_id: Option<i64>, target: String, body: String },
    #[serde(rename_all = "camelCase")]
    DraftClear { network_id: Option<i64>, target: String },
    #[serde(rename_all = "camelCase")]
    InputHistoryAdd { network_id: Option<i64>, target: String, text: String },
    /// Silently a no-op for a message you don't own and for system-buffer rows
    /// (no owning network) — no `bookmark-updated` follows, so don't render the
    /// toggle optimistically.
    #[serde(rename_all = "camelCase")]
    SetBookmark { message_id: i64 },
    #[serde(rename_all = "camelCase")]
    UnsetBookmark { message_id: i64 },
    #[serde(rename_all = "camelCase")]
    SetNickNote { network_id: i64, nick: String, note: String },
    /// `rule` follows `ignoreRuleInput.ts`: `{mask, channels?, pattern?,
    /// patternKind, levels?, isExcept?, expiresAt?}`. Channel/network muting is
    /// expressed here too — a rule with no mask scoped to a channel (§6).
    #[serde(rename_all = "camelCase")]
    AddIgnore { network_id: Option<i64>, rule: serde_json::Value },
    #[serde(rename_all = "camelCase")]
    RemoveIgnore {
        network_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mask: Option<String>,
    },

    // ── Presence & status ──
    /// **Per-socket, and resets to `false` on every new socket** — re-assert on
    /// every reconnect (§9.5). This gates push (no push while any client is
    /// visible) and auto-away. An open socket is deliberately *not* presence.
    Presence { visible: bool },
    #[serde(rename_all = "camelCase")]
    Typing { network_id: i64, target: String, state: String },
    /// User-scoped: hits every network.
    Away {
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Back,
    #[serde(rename_all = "camelCase")]
    ProbePresence { network_id: i64, nick: String },

    // ── Sync & fetch (reads — available while paused) ──
    /// Re-runs the connect burst as a gap-fill from the server's tracked cursor
    /// for this socket, without dropping the socket (§4.4).
    Snapshot,
    #[serde(rename_all = "camelCase")]
    History {
        network_id: Option<i64>,
        target: String,
        mode: HistoryMode,
        /// Clamped server-side to 1–500.
        limit: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        count_by: Option<PageUnit>,
        /// Exclusive id for `Before`.
        #[serde(skip_serializing_if = "Option::is_none")]
        before: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        anchor_id: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    Search {
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        network_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        nick: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        before: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<i64>,
    },
    /// Add or edit a friend. `contact_id` absent creates one.
    #[serde(rename_all = "camelCase")]
    SetContact {
        #[serde(skip_serializing_if = "Option::is_none")]
        contact_id: Option<i64>,
        display_name: String,
        notify_online: bool,
        targets: Vec<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    DeleteContact { contact_id: i64 },
    #[serde(rename_all = "camelCase")]
    ListChannels { network_id: i64 },
    #[serde(rename_all = "camelCase")]
    ChanlistSearch {
        network_id: i64,
        query: String,
        sort_by: String,
        sort_dir: String,
        offset: u32,
        limit: u32,
    },
}

impl ClientVerb {
    /// Hydrate a buffer: the *read* that fills the first screenful.
    ///
    /// This is the counterpart to the `open-buffer` warning in §4.3 — it
    /// changes nothing (no reopen, no row minted, no JOIN, nothing announced to
    /// other devices), always answers even for a target with no history, and is
    /// not gated for paused accounts. Send it for any buffer, as often as you
    /// like.
    pub fn hydrate(key: &BufferKey, limit: u32, count_by: PageUnit, token: i64) -> Self {
        Self::History {
            network_id: key.network_id,
            target: key.target.clone(),
            mode: HistoryMode::Latest,
            limit,
            token: Some(token),
            count_by: Some(count_by),
            before: None,
            after_id: None,
            anchor_id: None,
        }
    }

    /// Page older history (scroll up). `before` is exclusive.
    pub fn page_older(
        key: &BufferKey,
        before: i64,
        limit: u32,
        count_by: PageUnit,
        token: i64,
    ) -> Self {
        Self::History {
            network_id: key.network_id,
            target: key.target.clone(),
            mode: HistoryMode::Before,
            limit,
            token: Some(token),
            count_by: Some(count_by),
            before: Some(before),
            after_id: None,
            anchor_id: None,
        }
    }

    /// Jump-to-message window. **Detaches the buffer from live** (§8): stop
    /// splicing live events into the visible slice until a `Latest` reattaches.
    pub fn jump_around(
        key: &BufferKey,
        anchor_id: i64,
        limit: u32,
        count_by: PageUnit,
        token: i64,
    ) -> Self {
        Self::History {
            network_id: key.network_id,
            target: key.target.clone(),
            mode: HistoryMode::Around,
            limit,
            token: Some(token),
            count_by: Some(count_by),
            before: None,
            after_id: None,
            anchor_id: Some(anchor_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(v: &ClientVerb) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

    #[test]
    fn send_omits_absent_client_id_rather_than_nulling_it() {
        // §6: the ack is sent iff `clientId` is present, so a null would be a
        // different request than an absent field.
        let v = json(&ClientVerb::Send {
            network_id: 1,
            target: "#c".into(),
            text: "hi".into(),
            client_id: None,
        });
        assert_eq!(v["type"], "send");
        assert!(v.get("clientId").is_none());
    }

    #[test]
    fn hydrate_is_a_latest_history_read_not_an_open_buffer() {
        let v = json(&ClientVerb::hydrate(
            &BufferKey::new(Some(2), "#Chat"),
            100,
            PageUnit::Renderable,
            7,
        ));
        assert_eq!(v["type"], "history");
        assert_eq!(v["mode"], "latest");
        assert_eq!(v["target"], "#chat");
        assert_eq!(v["countBy"], "renderable");
        assert_eq!(v["token"], 7);
    }

    #[test]
    fn mark_read_sends_explicit_null_network_for_the_system_buffer() {
        // §6: "send an explicit null, don't omit" — omitting would leave the
        // server unable to distinguish the system buffer from a missing field.
        let v = json(&ClientVerb::MarkRead {
            network_id: None,
            target: ":system:".into(),
            message_id: 12,
        });
        assert!(v.as_object().unwrap().contains_key("networkId"));
        assert!(v["networkId"].is_null());
    }

    #[test]
    fn verb_names_are_kebab_case() {
        assert_eq!(json(&ClientVerb::MarkAllRead)["type"], "mark-all-read");
        assert_eq!(
            json(&ClientVerb::OpenBuffer {
                network_id: Some(1),
                target: "#c".into(),
                count_by: None
            })["type"],
            "open-buffer"
        );
        assert_eq!(json(&ClientVerb::Presence { visible: true })["type"], "presence");
    }

    #[test]
    fn creating_a_contact_omits_the_id_rather_than_nulling_it() {
        // `set_contact`'s schema is `additionalProperties: false` with
        // `contactId: {type: integer}`, so a null would be a type error where
        // an absent key is the documented "create" signal.
        let v = json(&ClientVerb::SetContact {
            contact_id: None,
            display_name: "amiantos".into(),
            notify_online: true,
            targets: vec![serde_json::json!({
                "networkId": 1, "nick": "amiantos", "isPrimary": true
            })],
        });
        assert_eq!(v["type"], "set-contact");
        assert!(!v.as_object().unwrap().contains_key("contactId"));
        assert_eq!(v["displayName"], "amiantos");
        assert_eq!(v["notifyOnline"], true);
        assert_eq!(v["targets"][0]["isPrimary"], true);

        let edit = json(&ClientVerb::SetContact {
            contact_id: Some(7),
            display_name: "amiantos".into(),
            notify_online: false,
            targets: vec![],
        });
        assert_eq!(edit["contactId"], 7);
        assert_eq!(json(&ClientVerb::DeleteContact { contact_id: 7 })["type"], "delete-contact");
    }

    #[test]
    fn page_unit_follows_the_render_tier() {
        assert_eq!(EventMode::None.page_unit(true), PageUnit::Chat);
        assert_eq!(EventMode::None.page_unit(false), PageUnit::Chat);
        assert_eq!(EventMode::All.page_unit(true), PageUnit::Renderable);
        assert_eq!(EventMode::All.page_unit(false), PageUnit::Event);
        // Smart has no unit of its own; it borrows the tier it would use.
        assert_eq!(EventMode::Smart.page_unit(true), PageUnit::Renderable);
    }
}
