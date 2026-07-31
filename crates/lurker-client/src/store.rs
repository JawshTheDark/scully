// The reducer: frames in, model out.
//
// This is where §8 (backlog merging) and §9 ("the tribal-knowledge section —
// every one of these was a real bug once") are implemented. It is deliberately
// synchronous, transport-free and UI-free so those rules can be tested directly
// against constructed frames.
//
// The store is single-writer and shared by every window (multi-window is a
// first-class case): windows observe [`StoreEvent`]s and read the model, they
// never mutate it themselves.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use lurker_proto::{
    BacklogFrame, BacklogMode, BufferKey, EventType, HistoryFrame, HistoryMode, Member,
    MessageEvent, NetworkSnapshot, NetworkState, ReadState, ServerFrame,
};

/// In-memory rows kept per buffer before older ones are evicted to the pager.
///
/// Policy, not protocol — §8 rule 8 notes the web client uses the same number.
pub const RING_CAPACITY: usize = 500;

/// Cap on how much of a single page is merged into an existing slice.
///
/// §8: a `countBy:'renderable'` page can be much larger than the ring, and
/// merging it wholesale evicts the very rows the reader is looking at, leaving
/// their scroll position pointing at content that no longer exists.
pub const MAX_INCREMENTAL_MERGE: usize = 250;

/// What changed, so windows can redraw the minimum.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreEvent {
    /// The buffer list changed shape (a buffer appeared or disappeared).
    BufferListChanged,
    /// Rows in this buffer changed.
    BufferChanged(BufferKey),
    /// Unread/highlight counts changed; recompute any badge.
    ReadStateChanged(BufferKey),
    /// This buffer's nicklist changed.
    MembersChanged(BufferKey),
    /// Someone started or stopped typing in this buffer.
    TypingChanged(BufferKey),
    /// Search results changed (the app holds them; windows just redraw).
    SearchResults,
    /// A structured whois result arrived (ephemeral, no buffer of its own).
    /// The window shows it wherever the user's setting says.
    WhoisResult(Box<MessageEvent>),
    /// Network connection state or nick changed.
    NetworkChanged(i64),
    /// A fresh event the server says should raise an alert. Already gated on
    /// `notify` and on freshness — see [`Store::apply_event`].
    Notify(BufferKey, Box<MessageEvent>),
    /// The connect burst finished. Until this arrives, a missing buffer is
    /// merely "not shipped yet" (§4.3).
    BacklogComplete,
    /// Server said the account is paused: treat as read-only, not an error.
    PausedChanged(bool),
    /// Settings values changed (local edit or another device via the
    /// `settings` frame). Windows re-render, since tier/consolidation are
    /// display choices.
    SettingsChanged,
    /// A page of the channel list arrived (`/list`). The app holds the rows;
    /// the browser window redraws.
    ChanlistResult,
    /// A `LIST` refresh started or finished for a network.
    ChanlistState,
    /// A voice call's participant count changed in a channel (Lurker #680), so
    /// any "call active (N)" badge on that buffer should be recomputed. Carries
    /// the affected buffer.
    CallPresenceChanged(BufferKey),
    /// A non-fatal server error frame.
    Error(String),
}

/// One conversation.
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    pub key: BufferKey,
    /// First-seen casing, kept for display while identity stays folded (§5.2).
    pub display_name: String,
    pub events: VecDeque<MessageEvent>,
    /// High-water mark of applied ids, used for the §9.6 freshness test.
    ///
    /// Tracked separately from the ring rather than derived from it, so that
    /// evicting old rows can never reopen the dedupe window and let a replayed
    /// event apply twice.
    pub newest_id: Option<i64>,
    /// Whether the scroll-up pager is still armed (§8 rule 4).
    pub has_more_older: bool,
    /// False for a shell that has never been filled.
    pub hydrated: bool,
    /// Currently joined (channels only).
    pub joined: bool,
    /// Set by a `history` `around` jump. While detached, live events must not
    /// be spliced into the visible slice (§8); `latest` reattaches.
    pub detached: bool,
    pub last_read_id: Option<i64>,
    pub unread: i64,
    pub highlights: i64,
    pub highlights_capped: bool,
    pub topic: Option<String>,
    /// Current channel mode string (`+Cnt`, possibly with params), from the
    /// snapshot and `channel-modes` events.
    pub modes: Option<String>,
    pub cleared_before_id: Option<i64>,
    /// Folded nick → member.
    pub members: BTreeMap<String, Member>,
    pub draft: Option<String>,
    pub input_history: Vec<String>,
    pub pinned: bool,
    /// Nicks currently typing: folded nick → (display nick, last activity).
    ///
    /// Entries are inserted on `typing` state `active`/`paused` and removed on
    /// `done` — but a `done` is not guaranteed (a client can vanish), so
    /// readers must also expire by age. [`Buffer::typing_nicks`] applies the
    /// cutoff.
    pub typing: std::collections::HashMap<String, (String, std::time::Instant)>,
}

/// How long a typing signal stays live without a refresh. The web client's
/// +typing TAGMSGs repeat every ~3 s while composing.
pub const TYPING_TTL: std::time::Duration = std::time::Duration::from_secs(6);

impl Buffer {
    fn new(key: BufferKey, display_name: String) -> Self {
        Self { key, display_name, ..Default::default() }
    }

    /// Display nicks typing right now, expired by [`TYPING_TTL`].
    pub fn typing_nicks(&self) -> Vec<String> {
        let now = std::time::Instant::now();
        let mut nicks: Vec<String> = self
            .typing
            .values()
            .filter(|(_, at)| now.duration_since(*at) < TYPING_TTL)
            .map(|(nick, _)| nick.clone())
            .collect();
        nicks.sort();
        nicks
    }

    /// Oldest id currently held, if any.
    fn oldest_id(&self) -> Option<i64> {
        self.events.iter().find_map(|e| e.id)
    }

    fn recompute_newest(&mut self) {
        self.newest_id = self.events.iter().filter_map(|e| e.id).max();
    }

    /// Drop rows off the old edge to respect the ring cap.
    ///
    /// §8: once this evicts anything there is more older history than we hold
    /// regardless of what `has_more_older` said, so the pager is re-armed here
    /// rather than trusting the flag from the frame.
    fn trim(&mut self) {
        while self.events.len() > RING_CAPACITY {
            self.events.pop_front();
            self.has_more_older = true;
        }
    }

    /// Insert keeping the ring ordered by id (§5.3: `time` is not monotonic
    /// with respect to `id`, so ordering is always by id).
    fn insert_ordered(&mut self, event: MessageEvent) {
        match event.id {
            // Ephemeral rows have no id and simply append at the live edge.
            None => self.events.push_back(event),
            Some(id) => {
                if self.events.back().and_then(|e| e.id).is_none_or(|last| last < id) {
                    self.events.push_back(event);
                } else {
                    let pos = self
                        .events
                        .iter()
                        .position(|e| e.id.is_some_and(|existing| existing > id))
                        .unwrap_or(self.events.len());
                    self.events.insert(pos, event);
                }
            }
        }
    }

    fn contains_id(&self, id: i64) -> bool {
        self.events.iter().any(|e| e.id == Some(id))
    }
}

/// A network, combining the WS snapshot with the name/host that only
/// `GET /api/networks` carries (§5.1).
#[derive(Clone, Debug, Default)]
pub struct Network {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub state: NetworkState,
    pub nick: Option<String>,
    pub lag_ms: Option<i64>,
    pub away: bool,
    pub pinned: Vec<String>,
    /// Capabilities from this network's ISUPPORT (005), cached as the burst
    /// arrives so a channel-mode dialog opened later still sees the full set
    /// even after the server-buffer ring evicted the original 005 lines.
    pub isupport: lurker_proto::isupport::Isupport,
}

#[derive(Debug, Default)]
pub struct Store {
    pub networks: BTreeMap<i64, Network>,
    pub buffers: BTreeMap<BufferKey, Buffer>,
    /// Highest **persisted, networked** message id ever seen — the `?since`
    /// resume cursor (§4.4).
    cursor: i64,
    pub paused: bool,
    /// True once `backlog-complete` has arrived, after which a missing buffer
    /// is proof that it is not open rather than merely not shipped yet (§4.3).
    pub backlog_complete: bool,
    pub max_upload_bytes: Option<u64>,
    /// Targets we have an outstanding `open-buffer` for.
    ///
    /// §4.3: `buffer-opened` is both our ack and a fan-out to the user's other
    /// devices, and the two are the same frame — so focusing on one we did not
    /// ask for drags the user to a buffer they opened on another device.
    pending_opens: HashSet<BufferKey>,
    /// Channels we have sent a `join` for but not yet seen `channel-joined`.
    pending_joins: HashSet<BufferKey>,

    /// Nicklists and topics from the most recent `snapshot`, held until the
    /// buffers they describe exist.
    ///
    /// The connect burst sends `snapshot` **first**, before any `backlog`
    /// frame has materialised a buffer (§4.3), so applying channel state
    /// directly would write it to buffers that do not exist yet and silently
    /// drop every nicklist. Applying it on materialisation instead is what
    /// makes the burst order irrelevant.
    snapshot_channels: HashMap<BufferKey, (Option<String>, Option<String>, Vec<Member>)>,

    /// Live voice-call participant counts per buffer (Lurker #680). Fed by the
    /// `call-presence` frame and by REST hydration on connect. Absent key or 0
    /// means no active call. Kept independent of `Buffer` so a call can be shown
    /// on a channel whose buffer isn't materialised yet.
    call_counts: HashMap<BufferKey, u32>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// The resume cursor to present as `?since` on the next connect.
    pub fn cursor(&self) -> i64 {
        self.cursor
    }

    /// Sum of highlights across every buffer — the app badge (§9.4).
    ///
    /// Recomputed from server-authoritative counts on every `read-state`
    /// rather than incremented locally, because a push notification can only
    /// *revise* the OS badge and the client must correct it once the user
    /// actually reads.
    pub fn total_highlights(&self) -> i64 {
        self.buffers.values().map(|b| b.highlights).sum()
    }

    pub fn buffer(&self, key: &BufferKey) -> Option<&Buffer> {
        self.buffers.get(key)
    }

    /// Number of participants in the active voice call for this buffer, or 0 if
    /// there is no call (Lurker #680).
    pub fn call_count(&self, key: &BufferKey) -> u32 {
        self.call_counts.get(key).copied().unwrap_or(0)
    }

    /// Replace a network's known call counts with a fresh server snapshot from
    /// `GET /api/voice/presence`. Called on each (re)connect edge, because the
    /// `call-presence` frame carries only live deltas — without this, a client
    /// that attaches mid-call never sees the badge. Clears this network's stale
    /// entries first, so a call that ended while we were away disappears.
    /// Returns the buffers whose count changed, for targeted redraws.
    pub fn hydrate_call_presence(
        &mut self,
        network_id: i64,
        calls: &[(String, u32)],
    ) -> Vec<BufferKey> {
        let mut changed = Vec::new();
        let fresh: HashMap<BufferKey, u32> = calls
            .iter()
            .filter(|(_, c)| *c > 0)
            .map(|(t, c)| (BufferKey::new(Some(network_id), t), *c))
            .collect();
        // Drop stale entries for this network that aren't in the snapshot.
        let stale: Vec<BufferKey> = self
            .call_counts
            .keys()
            .filter(|k| k.network_id == Some(network_id) && !fresh.contains_key(k))
            .cloned()
            .collect();
        for k in stale {
            self.call_counts.remove(&k);
            changed.push(k);
        }
        for (k, c) in fresh {
            if self.call_counts.insert(k.clone(), c) != Some(c) {
                changed.push(k);
            }
        }
        changed
    }

    /// Inject synthetic, non-persisted display lines into a buffer (e.g. a
    /// whois readout shown in the active buffer). They carry no id, so they
    /// never advance the cursor, never dedupe against real rows, and vanish on
    /// reconnect — exactly right for transient client-side output.
    pub fn inject_lines(
        &mut self,
        key: &BufferKey,
        nick: &str,
        lines: impl IntoIterator<Item = String>,
    ) {
        let Some(buf) = self.buffers.get_mut(key) else { return };
        for text in lines {
            buf.events.push_back(MessageEvent {
                event_type: EventType::Notice,
                network_id: key.network_id,
                target: Some(key.target.clone()),
                nick: Some(nick.to_string()),
                text: Some(text),
                ..Default::default()
            });
        }
        buf.trim();
    }

    /// The target string to put **on the wire** for this buffer.
    ///
    /// Folding is for local identity only (§5.2). The server stores
    /// `messages.target` in a BINARY-collated column that keeps whatever casing
    /// the network handed it (`#idleRPG` stays `#idleRPG`), and only resolves a
    /// request's casing when a `buffers` row exists to resolve it against —
    /// "no row falls through to what was asked" (`wsHub.ts:3098`). Send a
    /// lowercased target for a buffer with history but no row and the exact
    /// match finds nothing, which reads as an empty buffer with
    /// `hasMoreOlder:false`: hydrated, permanently blank, unpageable.
    ///
    /// So: identity folds, the wire carries the canonical casing we were told.
    pub fn wire_target(&self, key: &BufferKey) -> String {
        self.buffers
            .get(key)
            .map(|b| b.display_name.clone())
            .unwrap_or_else(|| key.target.clone())
    }

    /// Seed network names/hosts from `GET /api/networks`, which the snapshot
    /// does not carry.
    pub fn set_network_roster(&mut self, rows: impl IntoIterator<Item = (i64, String, String)>) {
        for (id, name, host) in rows {
            let net = self.networks.entry(id).or_insert_with(|| Network { id, ..Default::default() });
            net.name = name;
            net.host = host;
        }
    }

    /// Record that we asked to open `key`, so the `buffer-opened` reply can be
    /// told apart from another device's.
    pub fn note_pending_open(&mut self, key: BufferKey) {
        self.pending_opens.insert(key);
    }

    pub fn note_pending_join(&mut self, key: BufferKey) {
        self.pending_joins.insert(key);
    }

    /// Clear correlation state that a dropped socket invalidates.
    ///
    /// §4.3: a reply that never arrived would otherwise claim someone else's
    /// open later.
    pub fn on_socket_lost(&mut self) {
        self.pending_opens.clear();
        self.pending_joins.clear();
        self.backlog_complete = false;
    }

    // ── Frame dispatch ────────────────────────────────────────────────────

    pub fn apply_frame(&mut self, frame: ServerFrame) -> Vec<StoreEvent> {
        let mut out = Vec::new();
        match frame {
            ServerFrame::Snapshot(s) => {
                // Seed the cursor from the snapshot: the shell backlogs that
                // follow carry no rows, so without this a full burst would
                // leave the cursor at 0 and the next reconnect would refetch
                // everything (§4.3).
                if let Some(cursor) = s.cursor {
                    self.cursor = self.cursor.max(cursor);
                }
                self.max_upload_bytes = s.max_upload_bytes.or(self.max_upload_bytes);
                for ns in s.networks {
                    let id = ns.network_id;
                    self.apply_network_snapshot(ns);
                    out.push(StoreEvent::NetworkChanged(id));
                }
            }
            ServerFrame::BacklogComplete => {
                self.backlog_complete = true;
                out.push(StoreEvent::BacklogComplete);
            }
            ServerFrame::Backlog(b) => self.merge_backlog(b, &mut out),
            ServerFrame::History(h) => self.merge_history(h, &mut out),
            ServerFrame::Irc(e) => self.apply_event(*Box::new(e), &mut out),
            ServerFrame::ReadState(rs) => self.apply_read_state(rs, &mut out),
            ServerFrame::BufferOpened { network_id, target } => {
                // No handler is needed to *materialize* — the shell that
                // travels with it already did (§4.3). This only resolves
                // whether the open was ours, for focus purposes.
                let key = BufferKey::new(network_id, &target);
                self.touch_buffer(&key, &target, &mut out);
                self.pending_opens.remove(&key);
            }
            ServerFrame::BufferClosed { network_id, target } => {
                // Closed = absent: drop messages, drafts and membership (§9.1).
                let key = BufferKey::new(network_id, &target);
                if self.buffers.remove(&key).is_some() {
                    out.push(StoreEvent::BufferListChanged);
                }
            }
            ServerFrame::BufferReopened { .. } => {
                // Needs no handler: the event that caused the reopen arrives as
                // a normal `irc` frame and materializes the buffer (§9.1).
            }
            ServerFrame::BufferCleared { network_id, target, cleared_before_id, .. } => {
                let key = BufferKey::new(network_id, &target);
                if let Some(buf) = self.buffers.get_mut(&key) {
                    buf.cleared_before_id = cleared_before_id;
                    if let Some(before) = cleared_before_id {
                        buf.events.retain(|e| e.id.is_none_or(|id| id >= before));
                    }
                    out.push(StoreEvent::BufferChanged(key));
                }
            }
            ServerFrame::PinsChanged { network_id, pinned } => {
                if let Some(id) = network_id {
                    if let Some(net) = self.networks.get_mut(&id) {
                        net.pinned = pinned.clone();
                    }
                    let folded: HashSet<String> =
                        pinned.iter().map(|t| lurker_proto::fold(t)).collect();
                    for (key, buf) in self.buffers.iter_mut() {
                        if key.network_id == Some(id) {
                            buf.pinned = folded.contains(&key.target);
                        }
                    }
                    out.push(StoreEvent::BufferListChanged);
                }
            }
            ServerFrame::DraftUpdated { network_id, target, body } => {
                let key = BufferKey::new(network_id, &target);
                if let Some(buf) = self.buffers.get_mut(&key) {
                    buf.draft = body;
                }
            }
            ServerFrame::AccountState { paused } => {
                self.paused = paused;
                out.push(StoreEvent::PausedChanged(paused));
            }
            ServerFrame::Settings { max_upload_bytes, .. } => {
                if let Some(cap) = max_upload_bytes {
                    self.max_upload_bytes = Some(cap);
                }
            }
            ServerFrame::ChanlistResult { .. } => out.push(StoreEvent::ChanlistResult),
            ServerFrame::ChanlistState { .. } => out.push(StoreEvent::ChanlistState),
            ServerFrame::CallPresence { network_id, target, count } => {
                let key = BufferKey::new(Some(network_id), &target);
                let changed = if count > 0 {
                    self.call_counts.insert(key.clone(), count) != Some(count)
                } else {
                    self.call_counts.remove(&key).is_some()
                };
                if changed {
                    out.push(StoreEvent::CallPresenceChanged(key));
                }
            }
            ServerFrame::Error { text } => {
                out.push(StoreEvent::Error(text.unwrap_or_else(|| "unknown error".into())));
            }
            // §2: everything unrecognized is ignored, never fatal.
            _ => {}
        }
        out
    }

    /// Feed a motd event to its network's ISUPPORT cache. Called from every
    /// path that carries events (live, backlog, history), since the 005 burst
    /// usually arrives in the `:server:` backlog at connect — the bouncer
    /// registered upstream before this client attached.
    fn cache_isupport(&mut self, e: &MessageEvent) {
        if e.event_type != EventType::Motd {
            return;
        }
        if let (Some(id), Some(text)) = (e.network_id, e.text.as_ref()) {
            if let Some(net) = self.networks.get_mut(&id) {
                net.isupport.merge_line(text);
            }
        }
    }

    fn apply_network_snapshot(&mut self, ns: NetworkSnapshot) {
        let id = ns.network_id;
        let net = self.networks.entry(id).or_insert_with(|| Network { id, ..Default::default() });
        net.state = ns.state;
        net.nick = ns.nick.clone();
        net.lag_ms = ns.lag_ms;
        net.away = ns.away.as_ref().is_some_and(|a| a.active);
        net.pinned = ns.pinned.clone();

        let pinned: HashSet<String> = ns.pinned.iter().map(|t| lurker_proto::fold(t)).collect();

        // The snapshot's `channels` is authoritative for *nicklists and topics
        // of live channels*, but explicitly NOT for buffer existence (§4.3):
        // it is empty for any network without a live connection, and on a live
        // one it is read before auto-rejoin JOINs land. So it only updates
        // buffers, never creates or removes them.
        for ch in ns.channels {
            let key = BufferKey::new(Some(id), &ch.name);
            let members: Vec<Member> = ch.members;
            if let Some(buf) = self.buffers.get_mut(&key) {
                buf.topic = ch.topic.clone();
                buf.modes = ch.modes.clone();
                buf.joined = true;
                buf.members =
                    members.iter().map(|m| (lurker_proto::fold(&m.nick), m.clone())).collect();
            }
            // Held regardless: the buffer may not exist yet, and a later
            // `names` event will overwrite this anyway if one arrives.
            self.snapshot_channels.insert(key, (ch.topic, ch.modes, members));
        }
        for (key, buf) in self.buffers.iter_mut() {
            if key.network_id == Some(id) {
                buf.pinned = pinned.contains(&key.target);
            }
        }
    }

    // ── Buffer materialization (§9.1) ─────────────────────────────────────

    /// Get or create a buffer, preserving first-seen display casing.
    fn touch_buffer(&mut self, key: &BufferKey, display: &str, out: &mut Vec<StoreEvent>) {
        if self.buffers.contains_key(key) {
            return;
        }
        let mut buf = Buffer::new(key.clone(), display.to_string());
        // Adopt any channel state the snapshot described before this buffer
        // existed — otherwise the burst's snapshot-then-backlog order loses
        // every nicklist and topic.
        if let Some((topic, modes, members)) = self.snapshot_channels.get(key) {
            buf.topic = topic.clone();
            buf.modes = modes.clone();
            buf.joined = true;
            buf.members =
                members.iter().map(|m| (lurker_proto::fold(&m.nick), m.clone())).collect();
        }
        // Adopt the pin state from the network's snapshot `pinned` list — the
        // same materialize-after-snapshot ordering that loses nicklists loses
        // pins, so a pinned buffer restored on load would otherwise show under
        // its network instead of the pinned section.
        if let Some(net) = key.network_id.and_then(|id| self.networks.get(&id)) {
            let folded: Vec<String> = net.pinned.iter().map(|t| lurker_proto::fold(t)).collect();
            buf.pinned = folded.contains(&key.target);
        }
        self.buffers.insert(key.clone(), buf);
        out.push(StoreEvent::BufferListChanged);
        out.push(StoreEvent::MembersChanged(key.clone()));
    }

    /// Whether an event is allowed to bring a buffer into existence.
    ///
    /// Persisted rows may (that covers the DM rules for `message`/`action`, and
    /// the NOTICE case the server has already decided for us). Among ephemeral
    /// events only `channel-joined` may — it *is* the channel materialization
    /// signal. Everything else ephemeral is ambient and must resolve-or-drop:
    /// `typing` creating DM buffers was bug #292, and `read-state` resurrecting
    /// closed buffers in the sidebar was #319.
    fn may_materialize(event: &MessageEvent) -> bool {
        event.is_persisted() || event.event_type == EventType::ChannelJoined
    }

    // ── Live events ───────────────────────────────────────────────────────

    fn apply_event(&mut self, event: MessageEvent, out: &mut Vec<StoreEvent>) {
        // Whois results are ephemeral and targetless — hand them to windows
        // directly rather than materialising a buffer for them.
        if event.event_type == EventType::WhoisResult {
            out.push(StoreEvent::WhoisResult(Box::new(event)));
            return;
        }
        let key = event.buffer_key();
        let display = event.target.clone().unwrap_or_else(|| key.target.clone());

        if !self.buffers.contains_key(&key) {
            if !Self::may_materialize(&event) {
                return; // resolve-or-drop
            }
            self.touch_buffer(&key, &display, out);
        }

        // §9.6: a persisted event applies only if its id is greater than the
        // newest we hold. Replays happen by design (resume overlap,
        // backlog/live races), and running side effects on a replay is what
        // produced the topic-revert and double-join-line bugs.
        let fresh = match event.id {
            Some(id) => {
                let buf = &self.buffers[&key];
                let is_new = buf.newest_id.is_none_or(|newest| id > newest);
                if !is_new && buf.contains_id(id) {
                    return;
                }
                is_new
            }
            None => true,
        };

        // The cursor advances for any persisted networked row, including our
        // own echoes — but never for system-buffer rows, which have their own
        // id sequence (§4.4).
        if event.advances_cursor() {
            if let Some(id) = event.id {
                self.cursor = self.cursor.max(id);
            }
        }

        if fresh {
            if event.event_type.is_chat() {
                if let (Some(buf), Some(nick)) = (self.buffers.get_mut(&key), event.nick.as_ref())
                {
                    if buf.typing.remove(&lurker_proto::fold(nick)).is_some() {
                        out.push(StoreEvent::TypingChanged(key.clone()));
                    }
                }
            }
            self.apply_side_effects(&key, &event, out);
        }

        let notify = fresh && event.notify;

        {
            let buf = self.buffers.get_mut(&key).expect("materialized above");
            // A buffer detached by a jump must not have live rows spliced into
            // the visible slice (§8). It still tracks read state and counts.
            if !buf.detached && Self::is_renderable(&event.event_type) {
                buf.insert_ordered(event.clone());
                buf.trim();
            }
            if let Some(id) = event.id {
                buf.newest_id = Some(buf.newest_id.map_or(id, |n| n.max(id)));
            }
        }

        out.push(StoreEvent::BufferChanged(key.clone()));
        if notify {
            out.push(StoreEvent::Notify(key, Box::new(event)));
        }
    }

    /// Whether an event type produces a row at all, as opposed to being a pure
    /// state signal that renders nothing (§7.2).
    fn is_renderable(ty: &EventType) -> bool {
        !matches!(
            ty,
            EventType::ChannelTopic
                | EventType::ChannelModes
                | EventType::ChannelJoined
                | EventType::ChannelParted
                | EventType::Names
                | EventType::MemberUpdate
                | EventType::State
                | EventType::AwayState
                | EventType::PeerPresence
                | EventType::Typing
                | EventType::Lag
                | EventType::Usermode
                | EventType::OwnNick
                | EventType::ChanlistStart
                | EventType::ChanlistProgress
                | EventType::ChanlistEnd
        )
    }

    /// Membership, topic and connection-state mutations.
    ///
    /// §9.6: these ride the same events that render, and run **only when the
    /// event was fresh** — which is why they live behind the freshness gate
    /// rather than in the render path.
    fn apply_side_effects(&mut self, key: &BufferKey, e: &MessageEvent, out: &mut Vec<StoreEvent>) {
        let mut members_changed = false;
        match e.event_type {
            EventType::ChannelJoined => {
                if let Some(buf) = self.buffers.get_mut(key) {
                    buf.joined = true;
                }
                self.pending_joins.remove(key);
                out.push(StoreEvent::BufferListChanged);
            }
            EventType::ChannelParted => {
                // Resolve, never materialize: mark parted, clear members, keep
                // the buffer and its history (§9.1).
                if let Some(buf) = self.buffers.get_mut(key) {
                    buf.joined = false;
                    buf.members.clear();
                    members_changed = true;
                }
            }
            EventType::JoinError => {
                // Creates nothing, and cancels the intent. A 470 forward means
                // the channel we asked for never existed; the one we were
                // forwarded to announces itself with its own `channel-joined`.
                self.pending_joins.remove(key);
            }
            EventType::Names => {
                if let (Some(buf), Some(members)) = (self.buffers.get_mut(key), e.members.as_ref())
                {
                    buf.members =
                        members.iter().map(|m| (lurker_proto::fold(&m.nick), m.clone())).collect();
                    members_changed = true;
                }
            }
            EventType::MemberUpdate => {
                if let (Some(buf), Some(m)) = (self.buffers.get_mut(key), e.member.as_ref()) {
                    buf.members.insert(lurker_proto::fold(&m.nick), m.clone());
                    members_changed = true;
                }
            }
            EventType::Join => {
                if let (Some(buf), Some(nick)) = (self.buffers.get_mut(key), e.nick.as_ref()) {
                    buf.members.insert(
                        lurker_proto::fold(nick),
                        Member { nick: nick.clone(), ..Default::default() },
                    );
                    members_changed = true;
                }
            }
            EventType::Part | EventType::Quit => {
                if let (Some(buf), Some(nick)) = (self.buffers.get_mut(key), e.nick.as_ref()) {
                    buf.members.remove(&lurker_proto::fold(nick));
                    members_changed = true;
                }
            }
            EventType::Kick => {
                if let (Some(buf), Some(kicked)) = (self.buffers.get_mut(key), e.kicked.as_ref()) {
                    buf.members.remove(&lurker_proto::fold(kicked));
                    members_changed = true;
                }
            }
            EventType::Nick => {
                if let (Some(buf), Some(old), Some(new)) =
                    (self.buffers.get_mut(key), e.nick.as_ref(), e.new_nick.as_ref())
                {
                    if let Some(mut m) = buf.members.remove(&lurker_proto::fold(old)) {
                        m.nick = new.clone();
                        buf.members.insert(lurker_proto::fold(new), m);
                        members_changed = true;
                    }
                }
            }
            EventType::Typing => {
                // Resolve-or-drop: typing must never materialize a buffer
                // (bug #292 — the check in apply_event guarantees the buffer
                // exists here only if it already existed).
                if let (Some(buf), Some(nick)) = (self.buffers.get_mut(key), e.nick.as_ref()) {
                    match e.state.as_deref() {
                        Some("done") => {
                            buf.typing.remove(&lurker_proto::fold(nick));
                        }
                        _ => {
                            buf.typing.insert(
                                lurker_proto::fold(nick),
                                (nick.clone(), std::time::Instant::now()),
                            );
                        }
                    }
                    out.push(StoreEvent::TypingChanged(key.clone()));
                }
            }
            EventType::ChannelModes => {
                // The full mode string — the authoritative refresh a raw
                // `MODE #chan` query produces.
                if let Some(buf) = self.buffers.get_mut(key) {
                    buf.modes = e.text.clone();
                    out.push(StoreEvent::BufferChanged(key.clone()));
                }
            }
            // `chghost` renders only — its nicklist patch arrives separately as
            // `member-update` (§9.6).
            EventType::Topic | EventType::ChannelTopic => {
                if let Some(buf) = self.buffers.get_mut(key) {
                    buf.topic = e.text.clone();
                }
            }
            EventType::State => {
                if let Some(id) = e.network_id {
                    if let Some(net) = self.networks.get_mut(&id) {
                        if let Some(s) = e.state.as_deref() {
                            net.state = match s {
                                "connecting" => NetworkState::Connecting,
                                "connected" => NetworkState::Connected,
                                "reconnecting" => NetworkState::Reconnecting,
                                _ => NetworkState::Disconnected,
                            };
                        }
                        if e.nick.is_some() {
                            net.nick = e.nick.clone();
                        }
                    }
                    out.push(StoreEvent::NetworkChanged(id));
                }
            }
            EventType::OwnNick => {
                if let (Some(id), Some(nick)) = (e.network_id, e.nick.as_ref()) {
                    if let Some(net) = self.networks.get_mut(&id) {
                        net.nick = Some(nick.clone());
                    }
                    out.push(StoreEvent::NetworkChanged(id));
                }
            }
            EventType::Lag => {
                if let Some(id) = e.network_id {
                    if let Some(net) = self.networks.get_mut(&id) {
                        net.lag_ms = e.lag_ms;
                    }
                }
            }
            EventType::Motd => self.cache_isupport(e),
            _ => {}
        }
        if members_changed {
            out.push(StoreEvent::MembersChanged(key.clone()));
        }
    }

    fn apply_read_state(&mut self, rs: ReadState, out: &mut Vec<StoreEvent>) {
        let key = rs.key();
        // Ambient: `mark-all-read` fans out read-state for *closed* buffers
        // too, and materializing from it resurrected them in the sidebar
        // (bug #319). Resolve-or-drop.
        let Some(buf) = self.buffers.get_mut(&key) else { return };
        buf.last_read_id = rs.last_read_id;
        buf.unread = rs.unread;
        buf.highlights = rs.highlights;
        buf.highlights_capped = rs.highlights_capped;
        out.push(StoreEvent::ReadStateChanged(key));
    }

    // ── Backlog & history merging (§8) ────────────────────────────────────

    fn merge_backlog(&mut self, b: BacklogFrame, out: &mut Vec<StoreEvent>) {
        let key = b.key();
        let mode = b.merge_mode();
        self.touch_buffer(&key, &b.target, out);

        // Advance the cursor from any networked persisted row, and cache any
        // ISUPPORT lines the :server: backlog carries.
        for e in &b.events {
            if e.advances_cursor() {
                if let Some(id) = e.id {
                    self.cursor = self.cursor.max(id);
                }
            }
            self.cache_isupport(e);
        }

        let buf = self.buffers.get_mut(&key).expect("materialized above");
        buf.joined = b.joined;
        buf.last_read_id = b.last_read_id;
        buf.unread = b.unread;
        buf.highlights = b.highlights;
        buf.highlights_capped = b.highlights_capped;
        buf.cleared_before_id = b.cleared_before_id;
        if let Some(hist) = b.input_history {
            buf.input_history = hist;
        }
        // §8 rule 4: record `hasMoreOlder` on *every* frame — including
        // `append` and `shell` — or a gap-fill strands the pager.
        buf.has_more_older = b.has_more_older;

        match mode {
            // §8 rule 3: never un-hydrate. A shell announces existence only.
            BacklogMode::Shell => {}
            BacklogMode::Append => {
                Self::splice(buf, b.events);
                buf.hydrated = true;
            }
            BacklogMode::Replace => {
                Self::replace(buf, b.events);
                buf.hydrated = true;
            }
        }
        buf.recompute_newest();
        out.push(StoreEvent::BufferChanged(key.clone()));
        out.push(StoreEvent::ReadStateChanged(key));
    }

    fn merge_history(&mut self, h: HistoryFrame, out: &mut Vec<StoreEvent>) {
        let key = h.key();
        self.touch_buffer(&key, &h.target, out);

        for e in &h.events {
            if e.advances_cursor() {
                if let Some(id) = e.id {
                    self.cursor = self.cursor.max(id);
                }
            }
        }

        let buf = self.buffers.get_mut(&key).expect("materialized above");
        if let Some(hist) = h.input_history {
            buf.input_history = hist;
        }
        match h.mode {
            HistoryMode::Before => {
                buf.has_more_older = h.has_more_older;
                Self::splice(buf, h.events);
            }
            HistoryMode::After => Self::splice(buf, h.events),
            HistoryMode::Around => {
                // A jump detaches the buffer from live (§8).
                buf.detached = true;
                buf.has_more_older = h.has_more_older;
                Self::replace(buf, h.events);
            }
            HistoryMode::Latest => {
                buf.detached = false;
                buf.has_more_older = h.has_more_older;
                Self::replace(buf, h.events);
                buf.hydrated = true;
            }
        }
        buf.recompute_newest();
        out.push(StoreEvent::BufferChanged(key));
    }

    /// Contiguous gap-fill: dedupe by id and keep the ring ordered.
    fn splice(buf: &mut Buffer, events: Vec<MessageEvent>) {
        // §8: don't merge a page larger than the ring into an existing slice —
        // it evicts the rows the reader is looking at. Take the end adjacent to
        // what we hold and leave the remainder fetchable.
        let events = Self::clamp_to_adjacent_end(buf, events);
        for e in events {
            if let Some(id) = e.id {
                if buf.contains_id(id) {
                    continue;
                }
            }
            buf.insert_ordered(e);
        }
        buf.trim();
    }

    /// Authoritative slice.
    ///
    /// §8 rule 5: if it **overlaps** what we hold we may dedupe-merge and keep
    /// older rows the user paged in. This matters because `:system:` and
    /// offline `:server:` buffers get a full `replace` on *every* snapshot,
    /// including the in-band resync fired on visibility return — dropping
    /// everything there would throw away the user's scrollback each time they
    /// tab back. If the slice is **disjoint**, rows went missing in between and
    /// we must replace wholesale rather than create a hole.
    fn replace(buf: &mut Buffer, events: Vec<MessageEvent>) {
        let incoming_oldest = events.iter().find_map(|e| e.id);
        let overlaps = match (incoming_oldest, buf.newest_id) {
            (Some(oldest), Some(newest)) => oldest <= newest,
            _ => false,
        };

        if overlaps {
            Self::splice(buf, events);
            return;
        }

        // Wholesale replace — but §8 rule 6: keep held live events newer than
        // the slice tail, because a message can land mid-hydrate.
        let incoming_newest = events.iter().filter_map(|e| e.id).max();
        let survivors: Vec<MessageEvent> = match incoming_newest {
            Some(newest) => buf
                .events
                .iter()
                .filter(|e| e.id.is_some_and(|id| id > newest))
                .cloned()
                .collect(),
            None => Vec::new(),
        };

        buf.events = events.into_iter().collect();
        for e in survivors {
            if let Some(id) = e.id {
                if buf.contains_id(id) {
                    continue;
                }
            }
            buf.insert_ordered(e);
        }
        buf.trim();
    }

    /// Bound an incremental merge to [`MAX_INCREMENTAL_MERGE`] rows, keeping
    /// the end adjacent to what we already hold so the merge stays contiguous.
    fn clamp_to_adjacent_end(buf: &Buffer, mut events: Vec<MessageEvent>) -> Vec<MessageEvent> {
        if buf.events.is_empty() || events.len() <= MAX_INCREMENTAL_MERGE {
            return events;
        }
        let incoming_newest = events.iter().filter_map(|e| e.id).max();
        let older_page = match (incoming_newest, buf.oldest_id()) {
            (Some(newest), Some(held_oldest)) => newest <= held_oldest,
            _ => false,
        };
        if older_page {
            // Newest rows of an older page sit adjacent to our tail.
            events.drain(..events.len() - MAX_INCREMENTAL_MERGE);
        } else {
            // Oldest rows of a newer page sit adjacent to our head.
            events.truncate(MAX_INCREMENTAL_MERGE);
        }
        events
    }
}

#[cfg(test)]
mod tests;
