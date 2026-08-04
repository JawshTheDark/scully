// Shared application state.
//
// One tokio runtime, one socket, one store — and N windows observing it.
// Multi-window is why the store lives here rather than inside a window: windows
// hold no authoritative state of their own, they register an observer and read
// the store. A second window is therefore nearly free, and can never disagree
// with the first.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;

use lurker_client::{
    socket, ClientEvent, ConnState, Rest, SettingOption, Socket, SocketConfig, Store, StoreEvent,
};
use lurker_proto::{BufferKey, ClientVerb, EventMode, PageUnit};

use crate::credentials::Credentials;

/// Rows to request when filling a buffer's first screenful.
const HYDRATE_LIMIT: u32 = 150;
/// Rows to request when the user scrolls up.
const PAGE_LIMIT: u32 = 150;

pub type AppRef = Rc<App>;

/// Preferences stored on this machine only (`XDG_CONFIG_HOME/scully/device.json`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    pub inline_images: bool,
    pub inline_videos: bool,
    pub inline_audio: bool,
    /// Show whois replies in the active buffer instead of only the server log.
    pub whois_in_active_buffer: bool,
    /// Fetch link previews — title/description/thumbnail cards for links in
    /// chat. Off by default: it fetches URLs other people posted, which is a
    /// privacy consideration a user must opt into.
    pub link_previews: bool,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            inline_images: true,
            inline_videos: true,
            inline_audio: true,
            whois_in_active_buffer: false,
            link_previews: false,
        }
    }
}

impl DeviceSettings {
    fn path() -> std::path::PathBuf {
        crate::paths::config_dir().join("device.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub struct App {
    pub gtk_app: gtk::Application,
    pub rt: tokio::runtime::Runtime,
    pub store: RefCell<Store>,
    pub rest: RefCell<Option<Rest>>,
    socket: RefCell<Option<Socket>>,
    pub conn: RefCell<ConnState>,
    pub account: RefCell<Option<(url::Url, String)>>,

    /// Windows currently observing the store.
    observers: RefCell<Vec<(u64, Rc<dyn Fn(&[StoreEvent])>)>>,
    next_observer: Cell<u64>,
    /// Windows reporting themselves as focused, for the presence rule.
    focused_windows: RefCell<HashMap<u64, bool>>,

    /// Monotonic correlation token for `history` / `search` (§6).
    next_token: Cell<i64>,
    /// The newest token issued per buffer, so superseded replies can be dropped.
    live_tokens: RefCell<HashMap<BufferKey, i64>>,

    /// The user's event-noise tier, which decides the page unit we request.
    pub event_mode: Cell<EventMode>,
    pub consolidate: Cell<bool>,

    /// Windows the app keeps alive. Entries are removed when a window closes,
    /// which breaks the App→Window→App reference cycle.
    pub chat_windows: RefCell<Vec<Rc<crate::window::ChatWindow>>>,
    pub login_window: RefCell<Option<Rc<crate::login::LoginWindow>>>,

    /// The self-describing settings registry, fetched at session start.
    pub settings_registry: RefCell<Vec<SettingOption>>,
    /// This account's stored setting values, keyed by dotted registry key.
    pub settings_values: RefCell<std::collections::HashMap<String, serde_json::Value>>,
    /// The open settings window, if any (single instance).
    pub settings_window: RefCell<Option<Rc<crate::settings::SettingsWindow>>>,

    /// Device-local preferences the server registry doesn't model (inline
    /// media is a per-device bandwidth choice, like the mobile font size).
    pub device: RefCell<DeviceSettings>,
    /// Image texture cache for inline media.
    pub images: crate::media::ImageCache,
    /// Link-preview cache: url → fetched Open Graph title/description. `None`
    /// entry means "fetched, nothing usable" so we don't refetch.
    /// Link-preview cache: url → (fetched-at unix seconds, result). `None`
    /// result = fetched and found nothing (negative-cached, so a junk link
    /// isn't refetched every render). fetched-at 0 = in flight this session,
    /// never persisted. Loaded from and saved to previews.json so reopening
    /// the app doesn't refetch every card in the scrollback.
    pub previews:
        RefCell<std::collections::HashMap<String, (i64, Option<crate::media::Preview>)>>,

    /// Latest search results, and the token they answered — so a stale reply
    /// from a superseded query is dropped (§6's token discipline).
    /// Channel-list rows for the browser window, with the query they answer
    /// and whether a LIST refresh is still running.
    pub chanlist: RefCell<Vec<lurker_proto::ChanlistRow>>,
    pub chanlist_total: Cell<u32>,
    pub chanlist_loading: Cell<bool>,
    pub search_results: RefCell<Vec<lurker_proto::MessageEvent>>,
    pub search_token: Cell<i64>,
    pub search_more: Cell<bool>,

    /// Whether the server offers voice calls (from `/api/config`'s
    /// `voiceEnabled`). Gates the call UI; the media path is additionally a
    /// compile-time `voice` feature.
    pub voice_enabled: Cell<bool>,

    /// The resume cursor, mirrored out of the store.
    ///
    /// The store is `!Send` (it lives on the GTK main thread), but the socket
    /// task needs the newest cursor at every reconnect — so the value is
    /// published here after each frame rather than the task reaching across
    /// threads into the store.
    cursor: Arc<AtomicI64>,
}

impl App {
    pub fn new(gtk_app: gtk::Application) -> AppRef {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Rc::new(App {
            gtk_app,
            rt,
            store: RefCell::new(Store::new()),
            rest: RefCell::new(None),
            socket: RefCell::new(None),
            conn: RefCell::new(ConnState::Disconnected),
            account: RefCell::new(None),
            observers: RefCell::new(Vec::new()),
            next_observer: Cell::new(1),
            focused_windows: RefCell::new(HashMap::new()),
            next_token: Cell::new(1),
            live_tokens: RefCell::new(HashMap::new()),
            event_mode: Cell::new(EventMode::All),
            consolidate: Cell::new(true),
            chat_windows: RefCell::new(Vec::new()),
            login_window: RefCell::new(None),
            settings_registry: RefCell::new(Vec::new()),
            settings_values: RefCell::new(std::collections::HashMap::new()),
            settings_window: RefCell::new(None),
            device: RefCell::new(DeviceSettings::load()),
            images: crate::media::ImageCache::default(),
            previews: RefCell::new(load_previews()),
            chanlist: RefCell::new(Vec::new()),
            chanlist_total: Cell::new(0),
            chanlist_loading: Cell::new(false),
            search_results: RefCell::new(Vec::new()),
            search_token: Cell::new(0),
            search_more: Cell::new(false),
            voice_enabled: Cell::new(false),
            cursor: Arc::new(AtomicI64::new(0)),
        })
    }

    // ── Observers ─────────────────────────────────────────────────────────

    pub fn subscribe(&self, f: Rc<dyn Fn(&[StoreEvent])>) -> u64 {
        let id = self.next_observer.get();
        self.next_observer.set(id + 1);
        self.observers.borrow_mut().push((id, f));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.observers.borrow_mut().retain(|(i, _)| *i != id);
        self.focused_windows.borrow_mut().remove(&id);
    }

    pub fn notify(&self, events: &[StoreEvent]) {
        if events.is_empty() {
            return;
        }
        // Clone the list first: an observer may open or close a window, which
        // would otherwise re-enter the borrow.
        let observers: Vec<_> = self.observers.borrow().iter().map(|(_, f)| f.clone()).collect();
        for f in observers {
            f(events);
        }
    }

    // ── Session lifecycle ─────────────────────────────────────────────────

    /// Bring up a session: fetch the network roster, then open the socket.
    ///
    /// §5.1 / §12 suggest this order deliberately — the roster carries the
    /// network names the snapshot omits, and doubles as a token-validity check,
    /// so a dead token surfaces as a clean REST 401 before any socket exists.
    pub fn start_session(self: &AppRef, rest: Rest) {
        *self.rest.borrow_mut() = Some(rest.clone());

        // Say we are connecting, immediately. The roster fetch and the socket
        // both take a moment, and until now the window sat there with an empty
        // sidebar reading "disconnected" — which is exactly what a genuine
        // failure looks like, so startup was indistinguishable from breakage.
        *self.conn.borrow_mut() = ConnState::Connecting;
        self.notify(&[StoreEvent::BufferListChanged]);

        // Instance capabilities, on EVERY session start. Doing this only on the
        // password-login path left `voiceEnabled` false forever for anyone
        // resuming a stored token — which is the normal startup — so the call
        // UI never appeared no matter how the server was configured.
        {
            let probe = rest.clone();
            let app = self.clone();
            self.spawn_async(
                async move { probe.config().await },
                move |result| {
                    if let Ok(cfg) = result {
                        app.voice_enabled.set(cfg.voice_enabled);
                        // Windows are already built by now, so tell them to
                        // re-evaluate what the capability unlocks.
                        app.refresh_voice_ui();
                    }
                },
            );
        }

        let app = self.clone();
        self.spawn_async(
            async move { rest.networks().await },
            move |result| match result {
                Ok(rows) => {
                    app.store.borrow_mut().set_network_roster(
                        rows.into_iter().map(|n| (n.id, n.name, n.host)),
                    );
                    app.notify(&[StoreEvent::BufferListChanged]);
                    app.open_socket();
                    app.fetch_settings();
                }
                Err(e) => {
                    let msg = e.to_string();
                    if matches!(e, lurker_client::Error::Unauthorized) {
                        app.on_session_dead();
                    }
                    *app.conn.borrow_mut() = ConnState::Failed(msg.clone());
                    app.notify(&[StoreEvent::Error(msg)]);
                }
            },
        );
    }

    /// Re-evaluate voice-dependent UI in every open window. Called when the
    /// `voiceEnabled` capability lands (asynchronously, after the windows are
    /// already on screen).
    pub fn refresh_voice_ui(self: &AppRef) {
        for w in self.chat_windows.borrow().iter() {
            w.refresh_voice_ui();
        }
    }

    fn fetch_settings(self: &AppRef) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let app = self.clone();
        self.spawn_async(
            async move { rest.settings_bootstrap().await },
            move |result| match result {
                Ok(boot) => {
                    *app.settings_registry.borrow_mut() = boot.registry;
                    *app.settings_values.borrow_mut() = boot.values;
                    app.apply_settings();
                    app.notify(&[StoreEvent::SettingsChanged]);
                }
                Err(e) => tracing::warn!(error = %e, "settings bootstrap failed"),
            },
        );
    }

    /// The effective value of a setting: stored value, else registry default.
    pub fn setting(&self, key: &str) -> serde_json::Value {
        if let Some(v) = self.settings_values.borrow().get(key) {
            return v.clone();
        }
        self.settings_registry
            .borrow()
            .iter()
            .find(|o| o.key == key)
            .map(|o| o.default.clone())
            .unwrap_or(serde_json::Value::Null)
    }

    /// Push behavior-relevant settings into the fields the renderer reads.
    ///
    /// The tier is resolved client-side by design (`shared/eventFilter.ts`):
    /// the server has no business knowing this device's preference, but the
    /// page unit we request must match the unit we render in, which is why
    /// these two feed [`App::page_unit`].
    fn apply_settings(&self) {
        // Regenerate the display-wide CSS theme (colours, fonts) — the part
        // that makes the `look.*` registry keys real rather than decorative.
        // Safe to call repeatedly; the old provider is replaced, not stacked.
        if let Some(app) = crate::main_app() {
            crate::theme::apply(&app);
        }
        // Phone-class devices read the mobile tier key, mirroring the web's
        // viewport split — the two device classes tune noise independently.
        let tier_key =
            if crate::is_mobile_class() { "chat.events.mobile" } else { "chat.events" };
        let tier = self.setting(tier_key);
        self.event_mode.set(match tier.as_str() {
            Some("none") => EventMode::None,
            Some("smart") => EventMode::Smart,
            _ => EventMode::All,
        });
        if let Some(fold) = self.setting("chat.consolidate_joins").as_bool() {
            self.consolidate.set(fold);
        }
    }

    /// Write one setting. Optimistic locally; the PATCH reply is authoritative
    /// and other devices learn via the `settings` frame.
    pub fn set_setting(self: &AppRef, key: String, value: serde_json::Value) {
        self.settings_values.borrow_mut().insert(key.clone(), value.clone());
        self.apply_settings();
        self.notify(&[StoreEvent::SettingsChanged]);

        let Some(rest) = self.rest.borrow().clone() else { return };
        let app = self.clone();
        self.spawn_async(
            async move {
                let mut changes = std::collections::HashMap::new();
                changes.insert(key, value);
                rest.patch_settings(changes).await
            },
            move |result| match result {
                Ok(values) => {
                    *app.settings_values.borrow_mut() = values;
                    app.apply_settings();
                    app.notify(&[StoreEvent::SettingsChanged]);
                }
                Err(e) => {
                    // The reply is authoritative; surface the rejection and
                    // refetch so the optimistic write can't stick.
                    app.notify(&[StoreEvent::Error(format!("setting rejected: {e}"))]);
                    app.fetch_settings();
                }
            },
        );
    }

    /// Reset one setting to its registry default.
    pub fn reset_setting(self: &AppRef, key: String) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let app = self.clone();
        self.spawn_async(
            async move { rest.reset_setting(&key).await },
            move |result| {
                if let Ok(values) = result {
                    *app.settings_values.borrow_mut() = values;
                    app.apply_settings();
                    app.notify(&[StoreEvent::SettingsChanged]);
                }
            },
        );
    }

    fn open_socket(self: &AppRef) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let Some(token) = rest.token().map(str::to_string) else { return };

        let (tx, rx) = async_channel::unbounded::<ClientEvent>();

        // The cursor is read fresh on every connect attempt so a reconnect
        // resumes from the newest id the store has actually applied, not one
        // captured when the task was spawned (§4.4).
        let cursor_cell = self.cursor.clone();
        let cursor = move || cursor_cell.load(Ordering::Relaxed);

        let guard = self.rt.enter();
        let sock = socket::spawn(
            SocketConfig { base: rest.base().clone(), token },
            tx,
            cursor,
        );
        drop(guard);
        *self.socket.borrow_mut() = Some(sock);

        let app = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                app.on_client_event(event);
            }
        });
    }

    fn on_client_event(self: &AppRef, event: ClientEvent) {
        match event {
            ClientEvent::State(state) => {
                let reconnected = matches!(state, ConnState::Connected);
                if matches!(state, ConnState::Disconnected | ConnState::Backoff(_)) {
                    // §4.3: clear correlation state a dropped socket
                    // invalidates, or a reply that never arrived will claim
                    // someone else's open later.
                    self.store.borrow_mut().on_socket_lost();
                    self.live_tokens.borrow_mut().clear();
                }
                if let ConnState::Failed(ref msg) = state {
                    if msg.contains("session expired") {
                        self.on_session_dead();
                    }
                }
                *self.conn.borrow_mut() = state;
                if reconnected {
                    // §9.5: presence is per-socket and resets to false on every
                    // new socket, so it must be re-asserted on each reconnect —
                    // not just at startup.
                    self.assert_presence();
                }
                self.notify(&[StoreEvent::BufferListChanged]);
            }
            ClientEvent::Frame(frame) => {
                match frame.as_ref() {
                    lurker_proto::ServerFrame::SearchResult { token, results, has_more, .. } => {
                        // Drop a reply whose token we have already superseded.
                        if token.unwrap_or(0) >= self.search_token.get() {
                            *self.search_results.borrow_mut() = results.clone();
                            self.search_more.set(*has_more);
                            self.notify(&[StoreEvent::SearchResults]);
                        }
                    }
                    lurker_proto::ServerFrame::ChanlistResult { rows, total, in_progress, .. } => {
                        *self.chanlist.borrow_mut() = rows.clone();
                        self.chanlist_total.set(*total);
                        self.chanlist_loading.set(*in_progress);
                    }
                    lurker_proto::ServerFrame::ChanlistState { in_progress, .. } => {
                        self.chanlist_loading.set(*in_progress);
                    }
                    lurker_proto::ServerFrame::Settings { changes, .. } => {
                        if let Some(map) = changes.as_object() {
                            let mut values = self.settings_values.borrow_mut();
                            for (k, v) in map {
                                values.insert(k.clone(), v.clone());
                            }
                            drop(values);
                            self.apply_settings();
                            self.notify(&[StoreEvent::SettingsChanged]);
                        }
                    }
                    lurker_proto::ServerFrame::History(h) => tracing::info!(
                        target_ = %h.target, mode = ?h.mode, events = h.events.len(),
                        has_more_older = h.has_more_older, "← history reply"
                    ),
                    lurker_proto::ServerFrame::Backlog(b) => tracing::info!(
                        target_ = %b.target, mode = ?b.merge_mode(), events = b.events.len(),
                        "← backlog"
                    ),
                    // A summary, never the whole frame. Debug-formatting a
                    // MessageEvent prints its `members` vector in full, so a
                    // single NAMES burst on a large channel dumped hundreds of
                    // Member structs into one line — megabytes of formatting
                    // that also made the log useless to read.
                    lurker_proto::ServerFrame::Irc(e) => tracing::debug!(
                        kind = ?e.event_type,
                        target_ = e.target.as_deref().unwrap_or(""),
                        nick = e.nick.as_deref().unwrap_or(""),
                        id = ?e.id,
                        members = e.members.as_ref().map(|m| m.len()),
                        "← irc"
                    ),
                    other => tracing::debug!(frame = frame_label(other), "← server"),
                }
                let events = self.store.borrow_mut().apply_frame(*frame);
                // Publish the cursor for the socket task's next reconnect.
                self.cursor.store(self.store.borrow().cursor(), Ordering::Relaxed);
                let burst_done = events.iter().any(|e| matches!(e, StoreEvent::BacklogComplete));
                self.notify(&events);
                // The connect burst is done and networks are known — snapshot
                // active-call presence (the frame only carries deltas after this).
                if burst_done {
                    self.hydrate_call_presence();
                }
            }
            ClientEvent::Undecodable { raw, error } => {
                tracing::warn!(%error, %raw, "dropping a frame this client cannot parse");
            }
        }
    }

    /// A `401` anywhere means the session is dead (§3.4): drop the stored
    /// token so the next launch goes to login rather than retrying forever.
    fn on_session_dead(self: &AppRef) {
        if let Some((base, user)) = self.account.borrow().clone() {
            Credentials::for_server(&base, &user).clear();
        }
        *self.socket.borrow_mut() = None;
    }

    pub fn shutdown(&self) {
        if let Some(sock) = self.socket.borrow().as_ref() {
            sock.shutdown();
        }
    }

    /// Close every window and quit — the confirmed "close the main window"
    /// path. Draining the registry first drops each window's strong Rc so the
    /// GTK windows actually destroy.
    pub fn quit_all(self: &AppRef) {
        self.shutdown();
        let windows: Vec<_> = self.chat_windows.borrow_mut().drain(..).collect();
        for win in windows {
            win.close_now();
        }
        self.gtk_app.quit();
    }

    // ── Sending verbs ─────────────────────────────────────────────────────

    pub fn send(&self, verb: ClientVerb) -> bool {
        tracing::debug!(verb = ?verb, "→ server");
        match self.socket.borrow().as_ref() {
            Some(sock) => sock.send(verb),
            None => false,
        }
    }

    /// The canonical-cased target to put on the wire for a buffer.
    ///
    /// Never send the folded key: see [`Store::wire_target`].
    pub fn wire_target(&self, key: &BufferKey) -> String {
        self.store.borrow().wire_target(key)
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.conn.borrow(), ConnState::Connected)
    }

    fn page_unit(&self) -> PageUnit {
        self.event_mode.get().page_unit(self.consolidate.get())
    }

    fn token_for(&self, key: &BufferKey) -> i64 {
        let t = self.next_token.get();
        self.next_token.set(t + 1);
        self.live_tokens.borrow_mut().insert(key.clone(), t);
        t
    }

    /// Whether a reply's token is still the newest we issued for that buffer.
    pub fn token_is_current(&self, key: &BufferKey, token: Option<i64>) -> bool {
        match (self.live_tokens.borrow().get(key), token) {
            (Some(newest), Some(t)) => t >= *newest,
            _ => true,
        }
    }

    /// Fill a buffer's first screenful.
    ///
    /// Uses `history`/`latest`, never `open-buffer`: hydrating through the
    /// write verb would reopen the buffer on every device the user owns and
    /// would show a paused account nothing at all (§4.3).
    pub fn hydrate(&self, key: &BufferKey) {
        let token = self.token_for(key);
        tracing::info!(buffer = %key, wire_target = %self.wire_target(key), "→ history latest");
        self.send(ClientVerb::History {
            network_id: key.network_id,
            target: self.wire_target(key),
            mode: lurker_proto::HistoryMode::Latest,
            limit: HYDRATE_LIMIT,
            token: Some(token),
            count_by: Some(self.page_unit()),
            before: None,
            after_id: None,
            anchor_id: None,
        });
    }

    pub fn page_older(&self, key: &BufferKey, before: i64) {
        let token = self.token_for(key);
        self.send(ClientVerb::History {
            network_id: key.network_id,
            target: self.wire_target(key),
            mode: lurker_proto::HistoryMode::Before,
            limit: PAGE_LIMIT,
            token: Some(token),
            count_by: Some(self.page_unit()),
            before: Some(before),
            after_id: None,
            anchor_id: None,
        });
    }

    /// Run a message search. `scope` narrows to one buffer when given.
    ///
    /// The reply is matched by token so a slow answer to an abandoned query
    /// cannot overwrite newer results (§6).
    pub fn search(&self, query: &str, scope: Option<&BufferKey>) {
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        self.search_token.set(token);
        self.send(ClientVerb::Search {
            query: query.to_string(),
            network_id: scope.and_then(|k| k.network_id),
            target: scope.map(|k| self.wire_target(k)),
            nick: None,
            before: None,
            limit: Some(100),
            token: Some(token),
        });
    }

    /// Jump to a specific message: fetch a window around it, which DETACHES
    /// the buffer from live until the reader returns to the present (§8).
    pub fn jump_to(&self, key: &BufferKey, message_id: i64) {
        let token = self.token_for(key);
        self.send(ClientVerb::History {
            network_id: key.network_id,
            target: self.wire_target(key),
            mode: lurker_proto::HistoryMode::Around,
            limit: 60,
            token: Some(token),
            count_by: Some(self.page_unit()),
            before: None,
            after_id: None,
            anchor_id: Some(message_id),
        });
    }

    /// Return a detached buffer to the live present.
    pub fn reattach(&self, key: &BufferKey) {
        self.hydrate(key);
    }

    /// Start (or join) a voice call in `key`'s buffer. Mints a room-scoped token
    /// from the server, connects the native LiveKit engine, opens a call window,
    /// and announces the call in-channel (CTCP) so other Scully clients can
    /// join. All network work is off-thread; failures log rather than block.
    #[cfg(feature = "voice")]
    pub fn start_call(self: &AppRef, key: BufferKey, title: String) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let Some(network_id) = key.network_id else { return };
        let target = self.wire_target(&key);
        let app = self.clone();
        // Presence is announced by the server (LiveKit webhooks → `call-presence`
        // frame, Lurker #680), so the client sends no announcement itself —
        // joining the room is enough for peers to see the count tick up.
        self.spawn_async(
            async move { rest.voice_token(network_id, &target).await },
            move |result| match result {
                Ok(vt) => match crate::voice::Call::start(
                    app.rt.handle().clone(),
                    vt.url,
                    vt.token,
                ) {
                    Ok(call) => crate::callwindow::open(&app, call, &title),
                    Err(e) => eprintln!("[voice] could not start call: {e}"),
                },
                // e.g. 403 when a channel's call policy requires op/voice to join.
                Err(e) => eprintln!("[voice] token request failed: {e}"),
            },
        );
    }

    /// Fetch active-call presence for every known network and feed it to the
    /// store, so "call active (N)" badges appear even for calls that started
    /// while we were offline. Called on each connect burst (`backlog-complete`);
    /// the live `call-presence` frame handles deltas thereafter (Lurker #680).
    fn hydrate_call_presence(self: &AppRef) {
        if !self.voice_enabled.get() {
            return;
        }
        let Some(rest) = self.rest.borrow().clone() else { return };
        let ids: Vec<i64> = self.store.borrow().networks.keys().copied().collect();
        for id in ids {
            let rest = rest.clone();
            let app = self.clone();
            self.spawn_async(
                async move { rest.voice_presence(id).await },
                move |res| {
                    if let Ok(calls) = res {
                        let pairs: Vec<(String, u32)> =
                            calls.into_iter().map(|c| (c.target, c.count)).collect();
                        let changed = app.store.borrow_mut().hydrate_call_presence(id, &pairs);
                        let evs: Vec<StoreEvent> =
                            changed.into_iter().map(StoreEvent::CallPresenceChanged).collect();
                        if !evs.is_empty() {
                            app.notify(&evs);
                        }
                    }
                },
            );
        }
    }

    /// Mute or remove a participant from the call in `key` (Lurker #680).
    /// `action` is `"mute"` or `"remove"`; `identity` is the participant's IRC
    /// nick. Server-gated on op status — `report` surfaces the outcome.
    pub fn moderate_call(
        self: &AppRef,
        key: &BufferKey,
        identity: String,
        action: &'static str,
        report: impl Fn(Result<(), String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let Some(network_id) = key.network_id else { return };
        let target = self.wire_target(key);
        self.spawn_async(
            async move { rest.voice_moderate(network_id, &target, &identity, action).await },
            move |res| report(res.map_err(|e| e.to_string())),
        );
    }

    /// Read a channel's call join policy, then hand it to `done`.
    pub fn fetch_call_policy(
        self: &AppRef,
        key: &BufferKey,
        done: impl Fn(Result<crate::callmod::MinJoin, String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let Some(network_id) = key.network_id else { return };
        let target = self.wire_target(key);
        self.spawn_async(
            async move { rest.voice_policy(network_id, &target).await },
            move |res| {
                done(res
                    .map(|s| crate::callmod::MinJoin::from_wire(&s))
                    .map_err(|e| e.to_string()))
            },
        );
    }

    /// Set a channel's call join policy. Ops only, server-enforced.
    pub fn set_call_policy(
        self: &AppRef,
        key: &BufferKey,
        mode: crate::callmod::MinJoin,
        done: impl Fn(Result<crate::callmod::MinJoin, String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let Some(network_id) = key.network_id else { return };
        let target = self.wire_target(key);
        let wire = mode.to_wire();
        self.spawn_async(
            async move { rest.set_voice_policy(network_id, &target, wire).await },
            move |res| {
                done(res
                    .map(|s| crate::callmod::MinJoin::from_wire(&s))
                    .map_err(|e| e.to_string()))
            },
        );
    }

    /// Connect, disconnect or reconnect a network.
    ///
    /// The bouncer owns the IRC connection, so this is account-wide: taking a
    /// network down here takes it down for every device. `report` gets the
    /// outcome for the status line.
    pub fn network_action(
        self: &AppRef,
        id: i64,
        action: &'static str,
        report: impl Fn(Result<(), String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        self.spawn_async(
            async move { rest.network_action(id, action).await },
            move |res| report(res.map_err(|e| e.to_string())),
        );
    }

    /// Remove a network entirely, along with its buffers.
    pub fn delete_network(
        self: &AppRef,
        id: i64,
        report: impl Fn(Result<(), String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        self.spawn_async(
            async move { rest.delete_network(id).await },
            move |res| report(res.map_err(|e| e.to_string())),
        );
    }

    /// Add a network. `fields` is the server's own snake_case form payload.
    /// On success the roster is refetched so the new network appears without a
    /// reconnect.
    pub fn create_network(
        self: &AppRef,
        fields: serde_json::Value,
        report: impl Fn(Result<(), String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let app = self.clone();
        self.spawn_async(
            async move { rest.create_network(fields).await },
            move |res| match res {
                Ok(_) => {
                    app.refresh_networks();
                    report(Ok(()));
                }
                Err(e) => report(Err(e.to_string())),
            },
        );
    }

    /// Edit a network. Only the supplied keys change, so an omitted password
    /// is left as it was rather than cleared.
    pub fn update_network(
        self: &AppRef,
        id: i64,
        fields: serde_json::Value,
        report: impl Fn(Result<(), String>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let app = self.clone();
        self.spawn_async(
            async move { rest.update_network(id, fields).await },
            move |res| match res {
                Ok(()) => {
                    app.refresh_networks();
                    report(Ok(()));
                }
                Err(e) => report(Err(e.to_string())),
            },
        );
    }

    /// Fetch one network's full row (the roster carries fields the WS snapshot
    /// deliberately omits), for pre-filling the edit form.
    pub fn fetch_network_row(
        self: &AppRef,
        id: i64,
        done: impl Fn(Option<lurker_client::NetworkRow>) + 'static,
    ) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        self.spawn_async(
            async move { rest.networks().await },
            move |res| done(res.ok().and_then(|rows| rows.into_iter().find(|r| r.id == id))),
        );
    }

    /// Re-read the network roster into the store (after an add or removal).
    pub fn refresh_networks(self: &AppRef) {
        let Some(rest) = self.rest.borrow().clone() else { return };
        let app = self.clone();
        self.spawn_async(
            async move { rest.networks().await },
            move |res| {
                if let Ok(rows) = res {
                    app.store
                        .borrow_mut()
                        .set_network_roster(rows.into_iter().map(|n| (n.id, n.name, n.host)));
                    app.notify(&[StoreEvent::BufferListChanged]);
                }
            },
        );
    }

    /// Ask the server to refresh its cached channel list (sends a real `LIST`).
    pub fn list_channels(&self, network_id: i64) {
        self.chanlist_loading.set(true);
        self.send(ClientVerb::ListChannels { network_id });
    }

    /// Query the server's cached channel list. Paging and sorting happen
    /// server-side — a large network's list is far too big to hold client-side.
    pub fn chanlist_search(&self, network_id: i64, query: &str) {
        self.send(ClientVerb::ChanlistSearch {
            network_id,
            query: query.to_string(),
            sort_by: "users".into(),
            sort_dir: "desc".into(),
            offset: 0,
            limit: 200,
        });
    }

    /// Explicit user intent to open a buffer — the one place `open-buffer` is
    /// correct, because it is a write with side effects on every device.
    pub fn open_buffer(self: &AppRef, key: &BufferKey) {
        self.store.borrow_mut().note_pending_open(key.clone());
        self.send(ClientVerb::OpenBuffer {
            network_id: key.network_id,
            target: self.wire_target(key),
            count_by: Some(self.page_unit()),
        });
    }

    pub fn mark_read(&self, key: &BufferKey, message_id: i64) {
        // MAX-clamped and idempotent server-side, so re-sending a stale id is
        // safe (§6).
        self.send(ClientVerb::MarkRead {
            network_id: key.network_id,
            target: self.wire_target(key),
            message_id,
        });
    }

    // ── Presence (§9.5) ───────────────────────────────────────────────────

    pub fn set_window_focus(self: &AppRef, observer_id: u64, focused: bool) {
        self.focused_windows.borrow_mut().insert(observer_id, focused);
        self.assert_presence();
    }

    /// Assert visibility if *any* window is in front of the user.
    ///
    /// Presence gates push and auto-away, so with multiple windows the union is
    /// the correct signal — one window losing focus to another must not read as
    /// the user leaving.
    fn assert_presence(&self) {
        let visible = self.focused_windows.borrow().values().any(|v| *v);
        self.send(ClientVerb::Presence { visible });
    }

    /// Fetch an image for inline display, then redraw the buffers waiting on
    /// it. Deduped and size-capped by the cache.
    /// Play the bundled alert sound for a notification category
    /// (`highlight` / `dm` / `friend_online` / `always_notify`), honouring the
    /// website's per-category sound settings.
    ///
    /// The sound files are the web client's own — the Lurker server serves
    /// them at `/sounds/<choice>.mp3` — so the two clients literally play the
    /// same audio. Downloaded once into the data dir, then played from disk;
    /// a fetch failure just means silence this time (the notification itself
    /// already went up).
    pub fn play_notify_sound(self: &AppRef, category: &str) {
        let enabled = self
            .setting(&format!("notifications.{category}.sound.enabled"))
            .as_bool()
            .unwrap_or(false);
        if !enabled {
            return;
        }
        let choice = self
            .setting(&format!("notifications.{category}.sound.choice"))
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| "ping".to_string());
        // Server-supplied name, disk path: allow only the shapes the registry
        // enumerates so a hostile value can't traverse.
        if !choice.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return;
        }
        let volume = self
            .setting(&format!("notifications.{category}.sound.volume"))
            .as_f64()
            .unwrap_or(60.0)
            .clamp(0.0, 100.0)
            / 100.0;

        let path = crate::paths::data_dir().join("sounds").join(format!("{choice}.mp3"));
        if path.exists() {
            play_sound_file(&path, volume);
            return;
        }
        let Some(rest) = self.rest.borrow().clone() else { return };
        let Ok(url) = rest.base().join(&format!("sounds/{choice}.mp3")) else { return };
        self.spawn_async(
            async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(20))
                    .build()
                    .map_err(|e| e.to_string())?;
                let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("HTTP {}", resp.status()));
                }
                resp.bytes().await.map_err(|e| e.to_string())
            },
            move |result| match result {
                Ok(bytes) => {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if std::fs::write(&path, &bytes).is_ok() {
                        play_sound_file(&path, volume);
                    }
                }
                Err(e) => tracing::debug!(error = %e, "alert sound fetch failed"),
            },
        );
    }

    pub fn fetch_image(self: &AppRef, url: String, wake: BufferKey) {
        if !self.images.begin(&url) {
            return;
        }
        let Some(rest) = self.rest.borrow().clone() else { return };
        let _ = rest; // client reuse below builds its own; rest ensures a session exists
        let app = self.clone();
        let fetch_url = url.clone();
        self.spawn_async(
            async move {
                // A timeout is not optional here. Without one a host that
                // accepts the connection and then stalls leaves this task alive
                // forever AND leaves the url marked in-flight, so `begin()`
                // refuses every retry: the image silently never appears again
                // for the rest of the session. Generous, because image hosts
                // are routinely slow — catbox takes ~11s for a 640KB png — but
                // bounded.
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(60))
                    .connect_timeout(std::time::Duration::from_secs(15))
                    // Some hosts refuse or throttle requests without one.
                    .user_agent(concat!("Scully/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .ok()?;
                let resp = client.get(&fetch_url).send().await.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                let bytes = resp.bytes().await.ok()?.to_vec();
                (bytes.len() <= crate::media::MAX_IMAGE_BYTES).then_some(bytes)
            },
            move |result| {
                let ok = match result {
                    Some(bytes) => app.images.complete(&url, &bytes),
                    None => {
                        app.images.fail(&url);
                        false
                    }
                };
                if ok {
                    app.notify(&[StoreEvent::BufferChanged(wake)]);
                }
            },
        );
    }

    /// Fetch a YouTube link's Open Graph preview, then redraw the buffers
    /// waiting on it. Deduped via the cache; only called for YouTube URLs and
    /// only when the user enabled previews.
    pub fn fetch_preview(self: &AppRef, url: String, wake: BufferKey) {
        if self.previews.borrow().contains_key(&url) {
            return;
        }
        // Reserve the slot so concurrent renders don't refetch. fetched-at 0
        // marks in-flight; it renders as "nothing yet" and never persists.
        self.previews.borrow_mut().insert(url.clone(), (0, None));

        let app = self.clone();
        let fetch_url = url.clone();
        self.spawn_async(
            async move {
                let client = reqwest::Client::builder()
                    .user_agent("Mozilla/5.0 (compatible; Scully/0.1)")
                    .timeout(std::time::Duration::from_secs(20))
                    .build()
                    .ok()?;
                let resp = client.get(&fetch_url).send().await.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                // HTML only. The gate now admits ANY non-media link, and
                // without this a URL to a tarball would be downloaded to a
                // megabyte just to find no <meta> tags in it.
                let is_html = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| {
                        ct.starts_with("text/html") || ct.starts_with("application/xhtml")
                    });
                if !is_html {
                    return None;
                }
                // Scan a bounded prefix for og tags. YouTube front-loads
                // ~500 KB of inline JS before the <meta> tags, so the window
                // must be generous (but capped so a pathological page can't
                // blow up memory).
                let body = resp.text().await.ok()?;
                let head: String = body.chars().take(1_200_000).collect();
                let preview = crate::media::extract_preview(&head);
                (!preview.is_empty()).then_some(preview)
            },
            move |result| {
                app.previews.borrow_mut().insert(url, (unix_now(), result));
                save_previews(&app.previews.borrow());
                app.notify(&[StoreEvent::BufferChanged(wake)]);
            },
        );
    }

    /// Upload bytes through the instance's upload pipeline; the callback gets
    /// the public URL to paste. Checks the account's advertised cap first —
    /// `maxUploadBytes` is on the snapshot precisely so clients can refuse
    /// early (§10 Uploads).
    pub fn upload(
        self: &AppRef,
        filename: String,
        mime: String,
        bytes: Vec<u8>,
        done: impl FnOnce(Result<String, String>) + 'static,
    ) {
        if let Some(cap) = self.store.borrow().max_upload_bytes {
            if bytes.len() as u64 > cap {
                done(Err(format!(
                    "file is {} but this account's upload cap is {}",
                    human_bytes(bytes.len() as u64),
                    human_bytes(cap)
                )));
                return;
            }
        }
        let Some(rest) = self.rest.borrow().clone() else {
            done(Err("not connected".into()));
            return;
        };
        self.spawn_async(
            async move { rest.upload(&filename, &mime, bytes).await },
            move |result| match result {
                Ok(reply) => done(Ok(reply.url)),
                Err(e) => done(Err(e.to_string())),
            },
        );
    }

    // ── Async helper ──────────────────────────────────────────────────────

    /// Run a future on the tokio runtime and deliver its result on the GTK
    /// main thread.
    pub fn spawn_async<T, Fut, D>(&self, fut: Fut, done: D)
    where
        T: Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        D: FnOnce(T) + 'static,
    {
        let (tx, rx) = async_channel::bounded(1);
        self.rt.spawn(async move {
            let _ = tx.send(fut.await).await;
        });
        glib::spawn_future_local(async move {
            if let Ok(value) = rx.recv().await {
                done(value);
            }
        });
    }

    /// Reconnect delay used by the UI's status line, for display only.
    pub fn retry_hint(&self) -> Option<Duration> {
        match &*self.conn.borrow() {
            ConnState::Backoff(d) => Some(*d),
            _ => None,
        }
    }
}

/// A frame's variant name, for logging. Deliberately not `{:?}` of the frame:
/// several variants carry whole nicklists or backlog pages, and formatting
/// those to a log line is both slow and unreadable.
fn frame_label(frame: &lurker_proto::ServerFrame) -> &'static str {
    use lurker_proto::ServerFrame as F;
    match frame {
        F::Snapshot(_) => "snapshot",
        F::Backlog(_) => "backlog",
        F::History(_) => "history",
        F::Irc(_) => "irc",
        F::ReadState(_) => "read-state",
        F::BacklogComplete => "backlog-complete",
        F::ChanlistResult { .. } => "chanlist-result",
        F::ChanlistState { .. } => "chanlist-state",
        F::CallPresence { .. } => "call-presence",
        F::SearchResult { .. } => "search-result",
        F::Settings { .. } => "settings",
        F::Error { .. } => "error",
        _ => "other",
    }
}

fn human_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", n / 1024)
    }
}

/// Play an audio file once at the given volume (0.0–1.0). The MediaFile is
/// leaked into a static registry until it finishes — dropping it immediately
/// would cut the sound off.
fn play_sound_file(path: &std::path::Path, volume: f64) {
    let media = gtk::MediaFile::for_filename(path);
    media.set_volume(volume);
    media.play();
    // Keep it alive while playing; reap finished ones on each new play.
    thread_local! {
        static PLAYING: std::cell::RefCell<Vec<gtk::MediaFile>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    PLAYING.with(|p| {
        let mut v = p.borrow_mut();
        v.retain(|m| !m.is_ended());
        v.push(media);
    });
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn previews_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("previews.json")
}

/// Preview entries younger than this are trusted across restarts; older ones
/// refetch, since page metadata does drift.
const PREVIEW_TTL_SECS: i64 = 7 * 24 * 3600;
/// Newest entries kept at save time — a busy scrollback's worth, bounded.
const PREVIEW_CAP: usize = 500;

fn load_previews() -> std::collections::HashMap<String, (i64, Option<crate::media::Preview>)> {
    let now = unix_now();
    std::fs::read(previews_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<
            std::collections::HashMap<String, (i64, Option<crate::media::Preview>)>,
        >(&bytes).ok())
        .map(|m| {
            m.into_iter()
                .filter(|(_, (at, _))| *at > 0 && now - at < PREVIEW_TTL_SECS)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist the cache, newest PREVIEW_CAP entries, skipping in-flight slots.
/// Small (≤500 rows of text) and infrequent (once per completed fetch), so a
/// plain rewrite beats bookkeeping.
fn save_previews(
    map: &std::collections::HashMap<String, (i64, Option<crate::media::Preview>)>,
) {
    let mut rows: Vec<(&String, &(i64, Option<crate::media::Preview>))> =
        map.iter().filter(|(_, (at, _))| *at > 0).collect();
    rows.sort_by_key(|(_, (at, _))| std::cmp::Reverse(*at));
    rows.truncate(PREVIEW_CAP);
    let out: std::collections::HashMap<_, _> = rows.into_iter().collect();
    let path = previews_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(&out) {
        let _ = std::fs::write(path, json);
    }
}
