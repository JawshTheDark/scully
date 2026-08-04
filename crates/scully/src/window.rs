// A chat window.
//
// Several of these can exist at once over one store and one socket. A window
// owns only *view* state — which buffer it is looking at, and what it has drawn
// so far. Everything authoritative is read from the store on demand, which is
// what lets two windows show two buffers without any synchronisation between
// them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use lurker_client::StoreEvent;
use lurker_proto::consolidate::{self, Row};
use lurker_proto::{mirc, BufferKey, ClientVerb};

use crate::app::AppRef;

/// Ceiling used when the server has not told us its upload limit yet. Only a
/// guard against reading something enormous into memory; the server decides the
/// real limit.
const MAX_UPLOAD_FALLBACK: u64 = 64 * 1024 * 1024;

fn human_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", n / 1024)
    }
}
use crate::format;

/// Scroll within this many pixels of the top to trigger loading an older page.
const PAGE_TRIGGER_PX: f64 = 150.0;
/// Treat the view as "at the bottom" within this many pixels, for autoscroll.
const BOTTOM_EPSILON: f64 = 40.0;
/// Baseline top padding, before any bottom-anchoring pad is added.
const BASE_TOP_MARGIN: i32 = 6;

/// What the collapse rules need to remember about the row just appended:
/// its author (None for presence/server rows), its moment, and its rendered
/// timestamp string.
#[derive(Clone)]
struct PrevRow {
    author: Option<String>,
    unix: Option<i64>,
    stamp: String,
}

pub struct ChatWindow {
    app: AppRef,
    window: gtk::ApplicationWindow,
    observer_id: Cell<u64>,

    /// A popout window is pinned to one buffer: no sidebar, no auto-select,
    /// and the window closes when the buffer does. `None` is a full window.
    pinned: Option<BufferKey>,

    active: RefCell<Option<BufferKey>>,

    buffer_list: gtk::ListBox,
    member_list: gtk::ListBox,
    text_view: gtk::TextView,
    text: gtk::TextBuffer,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    title_label: gtk::Label,
    topic_label: gtk::Label,
    status_label: gtk::Label,
    member_count: gtk::Label,
    btn_addnet: gtk::Button,
    btn_search: gtk::Button,
    btn_popout: gtk::Button,
    btn_read: gtk::Button,
    btn_settings: gtk::Button,
    /// Voice-call affordances. Built unconditionally and shown only once the
    /// server advertises `voiceEnabled`, which lands after the window is up.
    btn_call: gtk::Button,
    call_panel: gtk::Box,
    call_status: gtk::Label,
    call_button: gtk::Button,
    overlay: gtk::Overlay,
    centre_pane: gtk::Box,
    sidebar_header: gtk::Box,
    members_header: gtk::Box,
    btn_members_back: gtk::Button,
    btn_members: gtk::Button,
    btn_attach: gtk::Button,
    btn_format: gtk::Button,
    suggestion_strip: gtk::Box,
    strip_scroll: gtk::ScrolledWindow,
    btn_back: gtk::Button,
    /// Narrow (phone) layout: one pane at a time instead of three side by
    /// side. Driven purely by window width, so it works on a Linux phone
    /// (FuriOS, Phosh) and on a desktop window dragged narrow alike.
    narrow: Cell<bool>,
    /// In narrow mode, whether the buffer list is showing rather than the
    /// conversation.
    narrow_showing_list: Cell<bool>,
    /// In narrow mode, whether the member list has been pulled up over the
    /// conversation. There is no room for it beside the messages, but "who is
    /// in here" is still worth being able to ask.
    narrow_showing_members: Cell<bool>,
    /// The lightbox currently over the conversation, if any.
    media_overlay: RefCell<Option<gtk::Box>>,
    buffer_pane: gtk::Box,
    member_pane: gtk::Box,
    header: gtk::Box,

    /// What the text view currently shows, so an incoming message can append
    /// instead of redrawing the whole scrollback.
    drawn: RefCell<Drawn>,
    /// Suppresses the row-selected handler while the list is being rebuilt.
    rebuilding: Cell<bool>,
    /// Rows in the buffer list, in display order.
    rows: RefCell<Vec<BufferKey>>,
    /// Buffer-list sections the user has folded away, by header title. Per
    /// window: two windows watching different networks should be able to
    /// collapse different things.
    collapsed: RefCell<std::collections::HashSet<String>>,
    /// Nicks this window issued a whois for while the "show in active buffer"
    /// setting was on, folded → the buffer to show the result in. Only the
    /// initiating window renders the fan-out result, so other windows don't
    /// each dump it into their own active buffer.
    pending_whois: RefCell<std::collections::HashMap<String, BufferKey>>,
    /// Child widgets embedded in the message TextView via anchors (inline
    /// images, media players). Tracked so they can be removed EXPLICITLY
    /// before the buffer is cleared: letting `set_text("")` tear them down
    /// crashed inside GTK (`gtk_text_view_remove` → unmap → selection query on
    /// a half-cleared buffer), especially when an image finished loading and
    /// forced a full redraw that destroyed the widget it had just added.
    embeds: RefCell<Vec<gtk::Widget>>,
    /// Weak handle to self, for deferred (idle) callbacks.
    weak_self: RefCell<std::rc::Weak<Self>>,
    /// Nicks shown in the member list, index-aligned with its rows.
    member_nicks: RefCell<Vec<String>>,
    /// The nick the context menu is currently open for.
    menu_nick: RefCell<String>,
    /// The live popover, kept only so a previous one can be dismissed before a
    /// new right-click opens another.
    nick_menu: RefCell<Option<gtk::PopoverMenu>>,
    /// The buffer-list context menu popover, and the buffer it was opened on.
    /// Same lifecycle as `nick_menu`: kept alive, replaced on each right-click.
    buffer_menu: RefCell<Option<gtk::PopoverMenu>>,
    menu_buffer: RefCell<Option<BufferKey>>,
    /// An older page is in flight; don't ask for another.
    paging: Cell<bool>,
    /// The unread divider position, pinned when the buffer became active
    /// (§9.4: the divider is client policy — snapshot `lastReadId` on
    /// activation and hold it until switch-away, or reading moves it under
    /// the reader's eyes).
    divider_after: Cell<Option<i64>>,
    /// Per-buffer input recall: position while cycling with Up/Down, and the
    /// uncommitted line saved when recall starts.
    history_pos: Cell<Option<usize>>,
    history_stash: RefCell<String>,
    /// Tab-completion cycle state: (start offset, candidates, index).
    completion: RefCell<Option<(i32, Vec<String>, usize)>>,
    /// Last outbound typing signal, to throttle the TAGMSGs.
    typing_sent: Cell<Option<std::time::Instant>>,
    /// Cached nick palette length, so renders don't re-read settings per line.
    palette_len: Cell<usize>,
    /// Cached strftime timestamp format.
    time_fmt: RefCell<String>,
    /// Settings snapshot for line rendering, rebuilt on SettingsChanged.
    render_opts: RefCell<format::RenderOpts>,
    /// look.message.collapse_authors / _window / collapse_timestamps.
    collapse_authors: Cell<bool>,
    collapse_window_secs: Cell<i64>,
    collapse_timestamps: Cell<bool>,
    /// look.nick.show_mode_prefix: @/+/% before the author in messages.
    show_mode_prefix: Cell<bool>,
    /// chat.keep_position_on_send: when false (the default), sending jumps
    /// the view to the newest message.
    keep_position_on_send: Cell<bool>,
    /// look.buffer_list.unread_display / unread_bold.
    unread_display: RefCell<String>,
    unread_bold: Cell<bool>,
    /// look.bar.*: lag thresholds and the status-bar clock format.
    lag_min_show_ms: Cell<i64>,
    lag_alarm_ms: Cell<i64>,
    lag_always_show: Cell<bool>,
    bar_time_fmt: RefCell<String>,
    /// look.color.mirc_colors overrides, index → colour, hex entries only.
    mirc_palette: RefCell<Vec<Option<String>>>,
    /// The previous appended row, for the collapse rules. Cleared whenever
    /// the buffer is redrawn from scratch.
    prev_row: RefCell<Option<PrevRow>>,
    /// Counts appended rows for the alternate-row striping.
    row_parity: Cell<u64>,
    /// input.suggestion_strip_on_desktop, cached — read per keystroke.
    strip_on_desktop: Cell<bool>,
    /// The value behind each pooled suggestion chip, by child index.
    chip_values: RefCell<Vec<String>>,
    /// chat.smart_filter_*, cached — read per RENDER, which is per incoming
    /// message; five registry lookups per message was pure waste.
    smart_join: Cell<bool>,
    smart_quit: Cell<bool>,
    smart_nick: Cell<bool>,
    smart_delay_secs: Cell<i64>,
    smart_unmask_secs: Cell<i64>,
    /// Armed after the first Send press on a message that will split
    /// (`chat.allow_split_messages` off): the second press confirms. Cleared
    /// by any edit or buffer switch.
    pending_split_confirm: Cell<bool>,
    /// Whether the view should follow new content to the bottom. Set by real
    /// user scrolls only; a programmatic scroll must not flip it (hence
    /// [`ChatWindow::programmatic`]). This replaces snapshotting "am I at the
    /// bottom" at render time — that snapshot went stale the moment async
    /// image loads changed the height, which was the scroll jerk.
    stick_bottom: Cell<bool>,
    /// True while the code (not the user) is moving the scrollbar, so the
    /// value-changed handler ignores its own writes.
    programmatic: Cell<bool>,
    /// The adjustment's `upper` at the last value-changed. A value-changed
    /// where `upper` also moved is a LAYOUT change (content grew, view
    /// resized), not a user scroll — reclassifying stick-to-bottom on those
    /// was what stranded a freshly-hydrated buffer at the top: value sat at 0
    /// while the 60 lines were still being laid out, and the handler read that
    /// as "scrolled up".
    last_upper: Cell<f64>,
    /// An older page is in flight and the scroll position has not settled.
    settling: Cell<bool>,
}

/// A rendered row's identity: an event's id, or a summary's run extent.
type RowSig = (Option<i64>, Option<i64>);

#[derive(Default, Clone)]
struct Drawn {
    key: Option<BufferKey>,
    /// Rendered ROW count, which is not the event count once runs are folded.
    len: usize,
    first: Option<RowSig>,
    /// Identity of the last row drawn. If a new event merges into that run,
    /// this changes and the append fast-path correctly falls back to a redraw.
    last: Option<RowSig>,
}

fn row_sig(row: &Row<'_>) -> RowSig {
    match row {
        Row::Event(e) => (e.id, e.id),
        Row::Summary(s) => (s.first_id, s.last_id),
    }
}

impl ChatWindow {
    pub fn new(app: &AppRef) -> Rc<Self> {
        Self::build(app, None)
    }

    /// A popout: one buffer, no sidebars — several of these tile nicely.
    pub fn popout(app: &AppRef, key: BufferKey) -> Rc<Self> {
        Self::build(app, Some(key))
    }

    fn build(app: &AppRef, pinned: Option<BufferKey>) -> Rc<Self> {
        let (width, height) = if pinned.is_some() { (760, 540) } else { (1280, 800) };
        let window = gtk::ApplicationWindow::builder()
            .application(&app.gtk_app)
            .title("Scully")
            .default_width(width)
            .default_height(height)
            .build();

        // ── Left: networks and buffers ──
        let buffer_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["buffer-list"])
            .build();
        let buffer_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&buffer_list)
            .vexpand(true)
            .build();
        let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
        left.add_css_class("sidebar");
        // Sidebar header — brand, then the window's tools. They live here
        // rather than over the message pane because they act on the session,
        // not on the conversation you happen to be reading.
        let sidebar_header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        sidebar_header.add_css_class("sidebar-header");
        sidebar_header.append(
            &gtk::Label::builder()
                .label("SCULLY")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["brand-mark"])
                .build(),
        );
        left.append(&sidebar_header);
        left.append(&buffer_scroll);
        let buffer_pane = left.clone();

        // ── Centre: title, messages, status, input ──
        let title_label = gtk::Label::builder().xalign(0.0).css_classes(["buffer-title"]).build();
        let topic_label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["topic"])
            .hexpand(true)
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("header");
        let header_box = header.clone();
        header.append(&title_label);
        header.append(&topic_label);

        // ── Toolbar buttons (right-aligned) ──
        //
        // Symbolic theme icons, not typeface glyphs. Text glyphs came from
        // whichever font happened to carry each codepoint — mismatched
        // weights, mismatched sizes, and one emoji font drawing a giant
        // telephone. The symbolic set is drawn as a family, monochrome,
        // recoloured by the button's own CSS `color`.
        let tool = |icon: &str, tip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .css_classes(["toolbtn"])
                .valign(gtk::Align::Center)
                .build()
        };
        // Only ever visible in the narrow (phone) layout.
        let btn_back = tool("go-previous-symbolic", "Back to conversations");
        btn_back.set_visible(false);
        header.prepend(&btn_back);
        // Phone-only: there is no room for the nicklist beside the messages,
        // so it becomes a view you can raise instead of a pane you lose.
        let btn_members = tool("system-users-symbolic", "Show who is in this channel");
        btn_members.set_visible(false);
        header.append(&btn_members);
        let btn_addnet = tool("list-add-symbolic", "Add a network");
        let btn_search = tool("system-search-symbolic", "Search messages (Ctrl+F)");
        let btn_popout = tool("window-new-symbolic", "Pop this channel out into its own window");
        let btn_read = tool("object-select-symbolic", "Mark everything read");
        let btn_settings = tool("emblem-system-symbolic", "Settings");
        // Hidden until `voiceEnabled` arrives — see refresh_voice_ui.
        let btn_call = tool("call-start-symbolic", "Start or join a voice call (/call)");
        btn_call.set_visible(false);
        // The tools sit in the sidebar header (see above). A popout has no
        // sidebar, so it simply has no toolbar — which it already didn't.
        sidebar_header.append(&btn_addnet);
        sidebar_header.append(&btn_search);
        sidebar_header.append(&btn_call);
        sidebar_header.append(&btn_popout);
        sidebar_header.append(&btn_read);
        sidebar_header.append(&btn_settings);

        let text = gtk::TextBuffer::new(None);
        let text_view = gtk::TextView::builder()
            .buffer(&text)
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .css_classes(["messages"])
            .top_margin(BASE_TOP_MARGIN)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&text_view)
            .vexpand(true)
            .build();

        let status_label =
            gtk::Label::builder().xalign(0.0).css_classes(["status"]).hexpand(true).build();
        let entry = gtk::Entry::builder()
            .placeholder_text("Message, or /help")
            .css_classes(["composer"])
            // Required since the composer became a horizontal row: a vertical
            // box stretches its children across, a horizontal one does not, so
            // without this the entry collapses to its natural width.
            .hexpand(true)
            .build();

        let centre = gtk::Box::new(gtk::Orientation::Vertical, 0);
        centre.append(&header);
        centre.append(&scroller);
        centre.append(&status_label);
        // Composer row. The attach button matters most on a phone, where there
        // is no drag-and-drop and the clipboard only helps for something you
        // already copied — but it is the obvious affordance on a desktop too,
        // so it is not gated on the narrow layout.
        let btn_attach = gtk::Button::builder()
            .icon_name("mail-attachment-symbolic")
            .tooltip_text("Attach a file or image")
            .css_classes(["toolbtn", "attach"])
            .valign(gtk::Align::Center)
            .build();
        // `input.show_format_button`: the mIRC palette. Hidden until the
        // setting says otherwise (apply_display_settings), like the web.
        let btn_format = gtk::Button::builder()
            .icon_name("color-select-symbolic")
            .tooltip_text("Formatting: colours, bold, italic…")
            .css_classes(["toolbtn", "attach"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();

        // The suggestion strip: completion candidates as tappable chips above
        // the composer — the phone's substitute for Tab, and available on the
        // desktop via `input.suggestion_strip_on_desktop`.
        let suggestion_strip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .css_classes(["suggestion-strip"])
            .visible(false)
            .build();
        let strip_scroll = gtk::ScrolledWindow::builder()
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hscrollbar_policy(gtk::PolicyType::External)
            .child(&suggestion_strip)
            .visible(false)
            .build();
        centre.append(&strip_scroll);

        let composer_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        composer_row.add_css_class("composer-row");
        composer_row.append(&btn_attach);
        composer_row.append(&btn_format);
        composer_row.append(&entry);
        centre.append(&composer_row);
        let centre_pane = centre.clone();

        // ── Right: nicklist ──
        let member_count = gtk::Label::builder().xalign(0.0).css_classes(["member-count"]).build();
        let member_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["member-list"])
            .build();
        let member_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&member_list)
            .vexpand(true)
            .build();
        // Call panel: pinned above the nicklist, so a live call is visible
        // wherever you are in the channel rather than hidden behind a toolbar
        // button. Built always and hidden until the server says voice exists —
        // the capability arrives asynchronously, long after this runs.
        let call_status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["call-status"])
            .build();
        let call_button = gtk::Button::builder().css_classes(["call-join"]).build();
        let call_panel = gtk::Box::new(gtk::Orientation::Vertical, 4);
        call_panel.add_css_class("call-panel");
        call_panel.append(&call_status);
        call_panel.append(&call_button);
        call_panel.set_visible(false);

        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.add_css_class("sidebar");
        // The narrow layout shows this pane INSTEAD of the conversation, and
        // the conversation header — which owns the back button — goes with it.
        // So this pane carries its own way out, or the member list is a room
        // with no door.
        let members_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        members_header.add_css_class("sidebar-header");
        let btn_members_back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back to the conversation")
            .css_classes(["toolbtn"])
            .build();
        members_header.append(&btn_members_back);
        members_header.append(
            &gtk::Label::builder().label("Members").xalign(0.0).hexpand(true)
                .css_classes(["brand-mark"]).build(),
        );
        members_header.set_visible(false);
        right.append(&members_header);
        right.append(&call_panel);
        right.append(&member_count);
        right.append(&member_scroll);
        let member_pane = right.clone();

        // The nicklist is a fixed-width sidebar: give it a width request and
        // stop the paned from resizing or shrinking it, rather than setting a
        // pixel `position`, which collapses the pane to nothing on any window
        // wider than the guess.
        right.set_size_request(190, -1);
        let inner = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&centre)
            .end_child(&right)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_end_child(false)
            .build();
        let outer = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&left)
            .end_child(&inner)
            // Wide enough for the brand plus six full-size toolbar buttons —
            // the header's natural width governs, since shrink_start_child
            // (false) below stops the pane going under its minimum anyway.
            .position(290)
            .resize_start_child(false)
            // Without this the pane may be allocated LESS than the sidebar's
            // minimum width, and GTK then overflows the child instead of
            // widening — which clipped the left edge off every row in the list.
            .shrink_start_child(false)
            .build();
        // The root is an Overlay so media can be shown *inside* this window —
        // a lightbox over the conversation rather than a separate window that
        // lands wherever the compositor decides.
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&outer));
        window.set_child(Some(&overlay));
        // On a handset the chat window should own the screen too.
        crate::fit_to_screen(&window);

        let this = Rc::new(ChatWindow {
            app: app.clone(),
            window,
            observer_id: Cell::new(0),
            pinned: pinned.clone(),
            active: RefCell::new(None),
            buffer_list,
            member_list,
            text_view,
            text,
            scroller,
            entry,
            title_label,
            topic_label,
            status_label,
            member_count,
            btn_call,
            call_panel,
            call_status,
            call_button,
            btn_addnet,
            btn_search,
            btn_popout,
            btn_read,
            btn_settings,
            overlay,
            centre_pane,
            sidebar_header,
            members_header,
            btn_members_back,
            btn_members,
            btn_attach,
            btn_format,
            suggestion_strip,
            strip_scroll,
            btn_back,
            narrow: Cell::new(false),
            narrow_showing_list: Cell::new(true),
            narrow_showing_members: Cell::new(false),
            media_overlay: RefCell::new(None),
            buffer_pane,
            member_pane,
            header: header_box,
            drawn: RefCell::new(Drawn::default()),
            rebuilding: Cell::new(false),
            rows: RefCell::new(Vec::new()),
            collapsed: RefCell::new(std::collections::HashSet::new()),
            embeds: RefCell::new(Vec::new()),
            pending_whois: RefCell::new(std::collections::HashMap::new()),
            weak_self: RefCell::new(std::rc::Weak::new()),
            member_nicks: RefCell::new(Vec::new()),
            menu_nick: RefCell::new(String::new()),
            nick_menu: RefCell::new(None),
            buffer_menu: RefCell::new(None),
            menu_buffer: RefCell::new(None),
            divider_after: Cell::new(None),
            history_pos: Cell::new(None),
            history_stash: RefCell::new(String::new()),
            completion: RefCell::new(None),
            typing_sent: Cell::new(None),
            palette_len: Cell::new(format::DEFAULT_NICK_PALETTE.len()),
            render_opts: RefCell::new(format::RenderOpts::default()),
            collapse_authors: Cell::new(false),
            collapse_window_secs: Cell::new(300),
            collapse_timestamps: Cell::new(true),
            show_mode_prefix: Cell::new(false),
            keep_position_on_send: Cell::new(false),
            unread_display: RefCell::new("full".to_string()),
            unread_bold: Cell::new(false),
            lag_min_show_ms: Cell::new(500),
            lag_alarm_ms: Cell::new(2000),
            lag_always_show: Cell::new(false),
            bar_time_fmt: RefCell::new(String::new()),
            mirc_palette: RefCell::new(Vec::new()),
            prev_row: RefCell::new(None),
            row_parity: Cell::new(0),
            strip_on_desktop: Cell::new(false),
            chip_values: RefCell::new(Vec::new()),
            smart_join: Cell::new(true),
            smart_quit: Cell::new(true),
            smart_nick: Cell::new(true),
            smart_delay_secs: Cell::new(300),
            smart_unmask_secs: Cell::new(1800),
            pending_split_confirm: Cell::new(false),
            time_fmt: RefCell::new("%H:%M:%S".to_string()),
            paging: Cell::new(false),
            stick_bottom: Cell::new(true),
            programmatic: Cell::new(false),
            last_upper: Cell::new(0.0),
            settling: Cell::new(false),
        });
        *this.weak_self.borrow_mut() = Rc::downgrade(&this);

        this.install_tags();
        this.connect_signals();

        // Register as an observer of the shared store.
        let weak = Rc::downgrade(&this);
        let id = app.subscribe(Rc::new(move |events: &[StoreEvent]| {
            if let Some(w) = weak.upgrade() {
                w.on_store_events(events);
            }
        }));
        this.observer_id.set(id);

        if let Some(key) = pinned {
            // A popout is JUST the conversation: messages, nicklist, editbox.
            // No buffer sidebar, no toolbar, no status line, no topic header —
            // the window title carries the channel name. Anything more and it
            // reads as another full app window instead of a popped-out channel.
            this.buffer_pane.set_visible(false);
            this.header.set_visible(false);
            this.status_label.set_visible(false);
            this.activate(&key);
        } else {
            this.rebuild_buffer_list();
        }
        this.apply_display_settings();
        this.update_status();
        this
    }

    /// Open a buffer by key, as if the user had clicked its row.
    ///
    /// Also the entry point a notification tap needs (§9.1's navigate-by-key
    /// case), which is why it lives here rather than in the click handler.
    pub fn open_key(self: &Rc<Self>, key: &BufferKey) {
        self.activate(key);
        self.window.present();
    }

    /// Whether this is the first (main) chat window — the one that owns
    /// app-wide side effects like notifications, so N open windows don't each
    /// raise the same alert.
    fn is_primary(&self) -> bool {
        self.app
            .chat_windows
            .borrow()
            .first()
            .is_some_and(|w| w.observer_id.get() == self.observer_id.get())
    }

    /// The buffer this window is pinned to, if it is a popout.
    pub fn pinned_key(&self) -> Option<&BufferKey> {
        self.pinned.as_ref()
    }

    /// Tidy up as this window goes away: dismiss its menu, save its draft, and
    /// drop it from the app registry (so popout dedupe can't find a dead one).
    fn detach(&self) {
        if let Some(menu) = self.nick_menu.borrow_mut().take() {
            menu.unparent();
        }
        self.save_draft();
        self.app.unsubscribe(self.observer_id.get());
        let id = self.observer_id.get();
        self.app.chat_windows.borrow_mut().retain(|w| w.observer_id.get() != id);
    }

    /// Ask before closing the main window when other windows (popouts) are
    /// open, then quit the whole app on confirmation.
    fn confirm_quit(self: &Rc<Self>) {
        let extra = self.app.chat_windows.borrow().len() - 1;
        let body = format!(
            "Closing the main window will quit Scully and close {extra} other              window{}.",
            if extra == 1 { "" } else { "s" }
        );
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Quit Scully?")
            .detail(body)
            .buttons(["Cancel", "Quit"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let app = self.app.clone();
        dialog.choose(Some(&self.window), gtk::gio::Cancellable::NONE, move |answer| {
            if answer == Ok(1) {
                app.quit_all();
            }
        });
    }

    /// Force this window closed during a quit-all, bypassing the confirm
    /// handler (the app already decided to quit).
    pub fn close_now(&self) {
        self.detach();
        self.window.destroy();
    }

    pub fn present(&self) {
        self.window.present();
        self.entry.grab_focus();
    }

    // ── Text tags ─────────────────────────────────────────────────────────

    fn install_tags(&self) {
        let tags = self.text.tag_table();
        // The tag name must be set through the builder: `set_name` on a
        // constructed TextTag resolves to GObject's unrelated type-module API.
        let add = |tag: gtk::TextTag| {
            tags.add(&tag);
        };

        // The retro-terminal palette. These have to be concrete colours rather
        // than CSS classes, because GtkTextTag styling is not CSS-driven.
        add(gtk::TextTag::builder().name("no-hyphen").insert_hyphens(false).build());
        add(gtk::TextTag::builder()
            .name("divider")
            .foreground("#ffcb6b")
            .justification(gtk::Justification::Center)
            .build());
        add(gtk::TextTag::builder().name("time").foreground("#5a5a6e").build());
        // Weight only. The old grouped layout gave headings 8px of air above;
        // in the classic per-line layout every message row carries this tag,
        // so that padding would space out the entire log.
        add(gtk::TextTag::builder().name("author").build());
        add(gtk::TextTag::builder().name("msg").foreground("#c9c9d4").build());
        add(gtk::TextTag::builder().name("msg-self").foreground("#9aa0b0").build());
        add(gtk::TextTag::builder().name("msg-highlight").foreground("#ffd479").weight(700).build(),
        );
        add(gtk::TextTag::builder().name("action").foreground("#c792ea").style(gtk::pango::Style::Italic).build());
        add(gtk::TextTag::builder().name("notice").foreground("#89ddff").build());
        add(gtk::TextTag::builder().name("join").foreground("#4f8f5f").build());
        add(gtk::TextTag::builder().name("leave").foreground("#8f5f5f").build());
        add(gtk::TextTag::builder().name("kick").foreground("#d16969").build());
        add(gtk::TextTag::builder().name("nickchange").foreground("#6b7089").build());
        add(gtk::TextTag::builder().name("mode").foreground("#6b7089").build());
        add(gtk::TextTag::builder().name("topic").foreground("#82aaff").build());
        add(gtk::TextTag::builder().name("server").foreground("#6b6b7b").build());
        add(gtk::TextTag::builder().name("error").foreground("#ff5370").build());
        add(gtk::TextTag::builder().name("nick-plain").foreground("#6b7089").build());
        add(gtk::TextTag::builder()
            .name("link")
            .foreground("#82aaff")
            .underline(gtk::pango::Underline::Single)
            .build());

        self.retint_nick_tags();
    }

    /// (Re)create the nick-colour tags from the settings palette.
    ///
    /// Called at build and again on every settings change, so editing
    /// `look.nick.colors` in any client retints this window live. Existing
    /// text keeps its tags; only the colours behind them change.
    fn retint_nick_tags(&self) {
        let palette = crate::theme::nick_palette(&self.app);
        self.palette_len.set(palette.len());
        let table = self.text.tag_table();
        for (i, colour) in palette.iter().enumerate() {
            let name = format!("nick-{i}");
            match table.lookup(&name) {
                Some(tag) => tag.set_property("foreground", colour),
                None => {
                    let tag = gtk::TextTag::builder()
                        .name(&name)
                        .foreground(colour)
                        .weight(600)
                        .build();
                    table.add(&tag);
                }
            }
        }
        // `look.color.link`: the clickable-URL colour, live like the rest of
        // the palette.
        if let Some(link) = self
            .app
            .setting("look.color.link")
            .as_str()
            .filter(|c| c.starts_with('#'))
        {
            if let Some(tag) = table.lookup("link") {
                tag.set_property("foreground", link);
            }
        }
        // `look.action.italic`: whether /me lines slant.
        if let Some(tag) = table.lookup("action") {
            let italic = self.app.setting("look.action.italic").as_bool().unwrap_or(true);
            tag.set_property(
                "style",
                if italic { gtk::pango::Style::Italic } else { gtk::pango::Style::Normal },
            );
        }
        let self_colour =
            crate::theme::self_color(&self.app).unwrap_or_else(|| "#9aa0b0".to_string());
        match table.lookup("nick-self") {
            Some(tag) => tag.set_property("foreground", &self_colour),
            None => {
                let tag = gtk::TextTag::builder()
                    .name("nick-self")
                    .foreground(&self_colour)
                    .weight(600)
                    .build();
                table.add(&tag);
            }
        }
    }

    /// Hanging indent so wrapped lines align under the message column.
    ///
    /// Measured from the actual font rather than assumed, because the whole
    /// three-column layout depends on the monospace advance width.
    /// Fraction of the message pane the aligned gutter may occupy.
    ///
    /// The three-column layout is only worth having while the message column
    /// stays readable. On a narrow window a full-width gutter squeezes text to
    /// a few characters per line, so alignment yields to legibility.
    const MAX_INDENT_FRACTION: f32 = 0.35;

    /// Size the hanging indent so wrapped lines align under the message column.
    ///
    /// Measures a real layout in the widget's own font rather than deriving
    /// from `approximate_char_width`: that metric is an *average* over the font
    /// and is taken from the Pango context's default font, not the monospace
    /// face the CSS applies here — so it over-measured badly enough to leave
    /// about fifteen characters of usable width.
    fn apply_indent(&self) {
        let width = self.text_view.width();
        if width <= 1 {
            // Not allocated yet; re-run once we have a real width.
            return;
        }

        // Every row opens with the timestamp column, so that is where wrapped
        // lines should hang — under the text, not back under the time.
        // Zero when the user has turned timestamps off, in which case there is
        // no column to hang under.
        let sample = " ".repeat(self.text_column());
        let layout = self.text_view.create_pango_layout(Some(sample.as_str()));
        let (prefix_px, _) = layout.pixel_size();

        let max_indent = (width as f32 * Self::MAX_INDENT_FRACTION) as i32;
        let indent = prefix_px.clamp(0, max_indent.max(0));

        // Pango's negative-indent semantics: the FIRST line stays at the
        // margin and *subsequent* (wrapped) lines are indented by `-indent`.
        // So the margin stays at the base 8px — the first line runs flush left
        // through time and nick, and wrapped lines hang under the text column.
        // (Adding the indent to the margin as well, as an earlier revision
        // did, shifts the first line right by one gutter and wrapped lines by
        // two — the whole buffer floats mid-pane.)
        self.text_view.set_left_margin(8);
        self.text_view.set_indent(-indent);
    }

    // ── Signals ───────────────────────────────────────────────────────────

    fn connect_signals(self: &Rc<Self>) {
        let this = self.clone();
        self.buffer_list.connect_row_selected(move |_, row| {
            if this.rebuilding.get() {
                return;
            }
            let Some(row) = row else { return };
            let idx = row.index() as usize;
            let key = this.rows.borrow().get(idx).cloned();
            if let Some(key) = key {
                this.activate(&key);
            }
        });

        // Single click on a URL opens it externally; a double click in the
        // message area opens channel control. One gesture handles both on
        // release, so a link click and the channel-control double click don't
        // fight over the press.
        let this = self.clone();
        let clicks = gtk::GestureClick::builder().button(1).build();
        clicks.connect_released(move |gesture, n_press, x, y| {
            if n_press == 1 {
                if let Some(url) = this.link_at(x, y) {
                    // `chat.image_modal.enabled` (default on): clicking an
                    // image link opens the in-app viewer when we already hold
                    // the texture (the inline embed fetched it). Ctrl-click,
                    // like the web's Cmd/Ctrl-click, always goes to the
                    // browser; so does everything that isn't a loaded image.
                    let modal = this
                        .app
                        .setting("chat.image_modal.enabled")
                        .as_bool()
                        .unwrap_or(true);
                    let ctrl = gesture
                        .current_event_state()
                        .contains(gtk::gdk::ModifierType::CONTROL_MASK);
                    let viewed = modal
                        && !ctrl
                        && matches!(
                            crate::media::classify(&url),
                            Some(crate::media::MediaKind::Image)
                        )
                        && this
                            .app
                            .images
                            .get(&url)
                            .inspect(|tex| this.view_image(tex, &url))
                            .is_some();
                    if !viewed {
                        this.open_url(&url);
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            } else if n_press == 2 && this.link_at(x, y).is_none() {
                if let Some(key) = this.active.borrow().clone() {
                    if key.is_channel() {
                        crate::channeldialog::open(&this.app, &key);
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                }
            }
        });
        self.text_view.add_controller(clicks);

        // DM person actions (#6) ride the TextView's own context menu via
        // set_extra_menu (populated per-buffer in activate): whois, CTCP and
        // add-to-friends appear UNDER the built-in Copy/Select-All items.
        // The first version claimed every right-click and long-press with
        // custom gestures, which made copying DM text impossible with a
        // mouse and hijacked touch text-selection — the built-in menu also
        // handles long-press for us, so touch needs nothing extra. Only the
        // action group is installed here; it is per-window state.
        let this = self.clone();
        let peer_group = gio::SimpleActionGroup::new();
        let peer_action = gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        peer_action.connect_activate(move |_, param| {
            if let Some(id) = param.and_then(|p| p.get::<String>()) {
                this.run_nick_command(&id);
            }
        });
        peer_group.add_action(&peer_action);
        self.text_view.insert_action_group("nick", Some(&peer_group));

        // Pointer cursor over links.
        let this = self.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            let name = if this.link_at(x, y).is_some() { "pointer" } else { "text" };
            this.text_view.set_cursor_from_name(Some(name));
        });
        self.text_view.add_controller(motion);
        let this = self.clone();
        let dbl_title = gtk::GestureClick::builder().button(1).build();
        dbl_title.connect_pressed(move |gesture, n_press, _, _| {
            if n_press == 2 {
                if let Some(key) = this.active.borrow().clone() {
                    if key.is_channel() {
                        crate::channeldialog::open(&this.app, &key);
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                }
            }
        });
        self.title_label.add_controller(dbl_title);

        // Right-click a member: the standard nicklist menu. One parameterised
        // action carries the command id; the nick is whatever row the menu was
        // opened on.
        let this = self.clone();
        let group = gio::SimpleActionGroup::new();
        let cmd_action =
            gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        cmd_action.connect_activate(move |_, param| {
            let Some(id) = param.and_then(|p| p.get::<String>()) else { return };
            this.run_nick_command(&id);
        });
        group.add_action(&cmd_action);
        self.member_list.insert_action_group("nick", Some(&group));

        let this = self.clone();
        let right = gtk::GestureClick::builder().button(3).build();
        right.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = this.member_list.row_at_y(y as i32) else { return };
            let idx = row.index() as usize;
            let Some(nick) = this.member_nicks.borrow().get(idx).cloned() else { return };
            *this.menu_nick.borrow_mut() = nick.clone();
            this.open_nick_menu(&nick, x as i32, y as i32);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.member_list.add_controller(right);

        // Touch equivalent. A phone has no right button, so press-and-hold is
        // how these menus are reached there; the handler is the same one, so
        // the two input methods can never drift apart.
        let this = self.clone();
        let hold = gtk::GestureLongPress::new();
        hold.connect_pressed(move |gesture, x, y| {
            let Some(row) = this.member_list.row_at_y(y as i32) else { return };
            let idx = row.index() as usize;
            let Some(nick) = this.member_nicks.borrow().get(idx).cloned() else { return };
            *this.menu_nick.borrow_mut() = nick.clone();
            this.open_nick_menu(&nick, x as i32, y as i32);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.member_list.add_controller(hold);

        // Middle-click a buffer row: pop that channel out without switching
        // the main window's own view.
        let this = self.clone();
        let middle = gtk::GestureClick::builder().button(2).build();
        middle.connect_pressed(move |gesture, _, _, y| {
            let list = this.buffer_list.clone();
            let Some(row) = list.row_at_y(y as i32) else { return };
            let idx = row.index() as usize;
            let key = this.rows.borrow().get(idx).cloned();
            if let Some(key) = key {
                if this.app.store.borrow().buffer(&key).is_some() {
                    crate::open_popout(&this.app, key);
                }
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.buffer_list.add_controller(middle);

        // Right-click a buffer row: context menu (pop out, mark read, pin,
        // leave/rejoin, close), tailored to the buffer's kind and state.
        let this = self.clone();
        let buf_right = gtk::GestureClick::builder().button(3).build();
        buf_right.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = this.buffer_list.row_at_y(y as i32) else { return };
            let idx = row.index() as usize;
            let Some(key) = this.rows.borrow().get(idx).cloned() else { return };
            this.open_buffer_menu(key, x as i32, y as i32);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.buffer_list.add_controller(buf_right);

        let this = self.clone();
        let buf_hold = gtk::GestureLongPress::new();
        buf_hold.connect_pressed(move |gesture, x, y| {
            let Some(row) = this.buffer_list.row_at_y(y as i32) else { return };
            let idx = row.index() as usize;
            let Some(key) = this.rows.borrow().get(idx).cloned() else { return };
            this.open_buffer_menu(key, x as i32, y as i32);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.buffer_list.add_controller(buf_hold);

        let this = self.clone();
        self.entry.connect_activate(move |entry| {
            let text = entry.text().to_string();
            if text.trim().is_empty() {
                return;
            }
            if this.submit(&text) {
                entry.set_text("");
                this.history_pos.set(None);
                this.signal_typing("done");
                // `chat.keep_position_on_send` (default off): sending jumps
                // to the newest message so your own line is visible when it
                // echoes back. On: hold the reading position — the web's
                // read-back-while-replying mode.
                if !this.keep_position_on_send.get() {
                    this.stick_bottom.set(true);
                    this.reflow();
                }
                // The sent line joins recall history immediately; the server
                // echoes it back via `input-history-added` for other devices.
                if let Some(key) = this.active.borrow().clone() {
                    let mut store = this.app.store.borrow_mut();
                    if let Some(buf) = store.buffers.get_mut(&key) {
                        buf.input_history.push(text.clone());
                    }
                    drop(store);
                    this.app.send(ClientVerb::InputHistoryAdd {
                        network_id: key.network_id,
                        target: this.app.wire_target(&key),
                        text,
                    });
                }
            }
        });

        // Tab completion, Up/Down recall. A capture-phase controller, because
        // GtkEntry consumes Tab for focus-chain navigation otherwise.
        let this = self.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, keyval, _, state| {
            if state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                // Ctrl+V with a file or image on the clipboard uploads it and
                // pastes the link; a text clipboard falls through to the
                // normal paste.
                if matches!(keyval, gtk::gdk::Key::v | gtk::gdk::Key::V)
                    && this.paste_media()
                {
                    return glib::Propagation::Stop;
                }
                // mIRC formatting, matching the web's shortcuts. These work
                // whether or not the palette button is shown.
                let code = match keyval {
                    gtk::gdk::Key::b | gtk::gdk::Key::B => Some("\u{02}"),
                    gtk::gdk::Key::i | gtk::gdk::Key::I => Some("\u{1D}"),
                    gtk::gdk::Key::u | gtk::gdk::Key::U => Some("\u{1F}"),
                    _ => None,
                };
                if let Some(code) = code {
                    this.insert_format(code);
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }
            match keyval {
                // Shift+Tab arrives as ISO_Left_Tab on X11/Wayland, not as Tab
                // with a modifier, so both spellings are checked.
                gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab => {
                    let back = keyval == gtk::gdk::Key::ISO_Left_Tab
                        || state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                    this.complete_nick(if back { -1 } else { 1 });
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Up => {
                    this.recall(-1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Down => {
                    this.recall(1);
                    glib::Propagation::Stop
                }
                _ => {
                    // Any other key ends a completion cycle.
                    *this.completion.borrow_mut() = None;
                    glib::Propagation::Proceed
                }
            }
        });
        self.entry.add_controller(keys);

        // Typing signals ride composition, not submission.
        let this = self.clone();
        self.entry.connect_changed(move |entry| {
            // Any edit invalidates a pending split confirmation — the message
            // being confirmed is no longer the message in the box.
            this.pending_split_confirm.set(false);
            this.refresh_suggestion_strip();
            if !entry.text().is_empty() {
                this.signal_typing("active");
            }
        });

        let this = self.clone();
        self.btn_format.connect_clicked(move |_| this.open_format_popover());

        // The status-bar clock (`look.bar.time_format`) ticks once a second
        // while a format is set. Weak, as below: the ticker must not keep a
        // closed window alive.
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(1, move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            if !this.bar_time_fmt.borrow().is_empty() {
                this.update_status();
            }
            glib::ControlFlow::Continue
        });

        // The typing display expires by time, so tick the status line while
        // anyone is marked as typing. Weak: the ticker must not keep a closed
        // window alive.
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(2, move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            let has_typists = this
                .active
                .borrow()
                .as_ref()
                .and_then(|k| {
                    let store = this.app.store.borrow();
                    store.buffer(k).map(|b| !b.typing.is_empty())
                })
                .unwrap_or(false);
            if has_typists {
                this.update_status();
            }
            glib::ControlFlow::Continue
        });

        // Load an older page when the user scrolls near the top.
        let this = self.clone();
        self.scroller.vadjustment().connect_value_changed(move |adj| {
            let upper = adj.upper();
            let upper_moved = (upper - this.last_upper.get()).abs() > 0.5;
            this.last_upper.set(upper);

            // Reclassify stick-to-bottom only on a GENUINE user scroll: not our
            // own programmatic writes, not the layout-driven value changes that
            // ride an `upper` change (content growing, view resizing).
            if !this.programmatic.get() && !upper_moved && !this.settling.get() {
                let at_bottom = adj.value() + adj.page_size() >= upper - BOTTOM_EPSILON;
                this.stick_bottom.set(at_bottom);

                // Page older on a real scroll to the top.
                if !this.paging.get()
                    && upper > adj.page_size() + 1.0
                    && adj.value() <= PAGE_TRIGGER_PX
                {
                    this.request_older_page();
                }
            }
        });

        // §9.5: presence is the union across windows, so each window reports
        // its own focus and the app decides.
        let this = self.clone();
        self.window.connect_is_active_notify(move |w| {
            this.app.set_window_focus(this.observer_id.get(), w.is_active());
            if w.is_active() {
                // §9.4: mark on focus-IN. Marking on focus-out loses the
                // tab-close race.
                this.mark_read_to_tail();
            }
        });

        let this = self.clone();
        self.window.connect_close_request(move |_| {
            // Closing the MAIN window (the only one with the buffer sidebar)
            // means quitting the whole app — but only after confirmation, and
            // only if it isn't the last remaining window regardless. A popout
            // just closes itself.
            let is_main = this.pinned.is_none();
            let other_windows = this.app.chat_windows.borrow().len() > 1;
            if is_main && other_windows {
                this.confirm_quit();
                return glib::Propagation::Stop; // wait for the dialog's answer
            }
            this.detach();
            glib::Propagation::Proceed
        });

        let this = self.clone();
        self.btn_search.connect_clicked(move |_| this.open_search(false));

        let this = self.clone();
        self.btn_popout.connect_clicked(move |_| {
            let Some(key) = this.active.borrow().clone() else { return };
            crate::open_popout(&this.app, key);
        });

        let this = self.clone();
        self.btn_read.connect_clicked(move |_| {
            // MAX-clamped and idempotent server-side; the read-state fan-out
            // repaints every window's badges (§9.4).
            this.app.send(ClientVerb::MarkAllRead);
        });
        let this = self.clone();
        self.btn_settings.connect_clicked(move |_| {
            crate::settings::SettingsWindow::open(&this.app);
        });
        let this = self.clone();
        self.btn_addnet.connect_clicked(move |_| {
            crate::networkdialog::open(&this.app, this.window.clone().upcast_ref());
        });

        // Voice call: toolbar button and the nicklist panel's join button both
        // route through try_start_call, which reports precisely why nothing
        // happened when it can't proceed.
        let this = self.clone();
        self.btn_call.connect_clicked(move |_| this.try_start_call());
        let this = self.clone();
        self.call_button.connect_clicked(move |_| this.try_start_call());

        // A context menu no longer takes a grab (see open_nick_menu), so
        // dismissing it is our job. Capture phase, on the root: any press that
        // lands outside the open menu closes it, and the event still travels on
        // to whatever was clicked — which is what lets a second right-click
        // open a new menu at the new place instead of merely closing the old.
        let this = self.clone();
        let dismiss = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        dismiss.connect_pressed(move |_, _, x, y| {
            let open = this
                .nick_menu
                .borrow()
                .clone()
                .map(|p| p.upcast::<gtk::Widget>())
                .or_else(|| this.buffer_menu.borrow().clone().map(|p| p.upcast::<gtk::Widget>()));
            let Some(menu) = open else { return };
            let inside = menu
                .compute_bounds(&this.overlay)
                .is_some_and(|r| r.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
            if !inside {
                this.close_menus();
            }
        });
        self.overlay.add_controller(dismiss);

        // Follow the window width into and out of the phone layout. Width, not
        // a device check: a desktop window dragged narrow gets the same layout,
        // which is also the only way to exercise it without a phone.
        // Follow the ALLOCATED width, via the frame clock. `default-width` is
        // the size we asked for, and a compositor that maximises us — which is
        // exactly what a phone shell does — never changes it, so watching that
        // property meant the layout never re-evaluated on the one device this
        // is for. The callback compares an i32 per frame and does nothing until
        // it actually changes.
        let weak = self.clone_handle();
        let last_width = Cell::new(-1i32);
        self.window.add_tick_callback(move |w, _| {
            let width = w.width();
            if width != last_width.get() {
                last_width.set(width);
                if let Some(this) = weak.upgrade() {
                    this.apply_narrow_layout();
                }
            }
            glib::ControlFlow::Continue
        });
        let this = self.clone();
        self.btn_back.connect_clicked(move |_| this.narrow_show_list());
        let this = self.clone();
        self.btn_members.connect_clicked(move |_| this.narrow_toggle_members());
        let this = self.clone();
        self.btn_attach.connect_clicked(move |_| this.pick_and_upload());
        let this = self.clone();
        self.btn_members_back.connect_clicked(move |_| this.narrow_show_list());

        // Window-level shortcuts: Ctrl+F search here, Ctrl+Shift+F search
        // everywhere, Ctrl+K quick switcher, Escape returns a detached buffer
        // to the live present.
        let this = self.clone();
        let shortcuts = gtk::EventControllerKey::new();
        shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
        shortcuts.connect_key_pressed(move |_, key, _, state| {
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);

            // Alt+1…Alt+9 jumps to the Nth buffer, counting what a person can
            // see rather than what the list contains (section headers occupy
            // rows too).
            if alt {
                if let Some(digit) = key.to_unicode().and_then(|c| c.to_digit(10)) {
                    if digit >= 1 {
                        this.jump_to_nth(digit as usize);
                        return glib::Propagation::Stop;
                    }
                }
            }

            match key {
                // Alt+A: the next buffer that wants you, highlights first.
                gtk::gdk::Key::a | gtk::gdk::Key::A if alt => {
                    this.jump_to_attention();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::f | gtk::gdk::Key::F if ctrl => {
                    this.open_search(shift);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::k | gtk::gdk::Key::K if ctrl => {
                    this.open_switcher();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Escape => {
                    // A context menu is the frontmost thing Escape can mean,
                    // then the lightbox.
                    if this.close_menus() {
                        return glib::Propagation::Stop;
                    }
                    if this.dismiss_overlay() {
                        return glib::Propagation::Stop;
                    }
                    // On a phone, Escape is "back" once nothing is layered.
                    if this.narrow.get() && !this.narrow_showing_list.get() {
                        this.narrow_show_list();
                        return glib::Propagation::Stop;
                    }
                    // Otherwise only meaningful when detached by a jump.
                    if this.return_to_present() {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(shortcuts);

        // Dropping a file anywhere on the window uploads it too.
        let this = self.clone();
        let drop = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );
        drop.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gtk::gdk::FileList>() else { return false };
            for file in files.files() {
                let Some(path) = file.path() else { continue };
                if let Ok(bytes) = std::fs::read(&path) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".into());
                    let mime = mime_for(&name);
                    this.upload_and_insert(name, mime, bytes);
                }
            }
            true
        });
        self.window.add_controller(drop);

        let this = self.clone();
        self.text_view.connect_realize(move |_| this.apply_indent());
        let this = self.clone();
        self.window.connect_realize(move |_| this.apply_narrow_layout());

        // Re-anchor and re-stick whenever the scrollable geometry settles:
        // content added, window resized, or an image finishing its load.
        let adj = self.scroller.vadjustment();
        let this = self.clone();
        adj.connect_upper_notify(move |_| this.reflow());
        let this = self.clone();
        self.scroller.vadjustment().connect_page_size_notify(move |_| this.reflow());
    }

    // ── Store events ──────────────────────────────────────────────────────

    fn on_store_events(self: &Rc<Self>, events: &[StoreEvent]) {
        let active = self.active.borrow().clone();
        for e in events {
            if let StoreEvent::BufferChanged(k) = e {
                tracing::debug!(
                    changed = %k,
                    active = ?active.as_ref().map(ToString::to_string),
                    matches = (Some(k) == active.as_ref()),
                    "buffer-changed"
                );
            }
        }
        let mut relist = false;
        let mut redraw = false;
        let mut remembers = false;
        let mut status = false;

        for event in events {
            match event {
                // Both change what the FRIENDS section shows: who is in it, and
                // which of them is reachable (which also sets the header dot).
                StoreEvent::ContactsChanged
                | StoreEvent::FavoritesChanged
                | StoreEvent::PresenceChanged(_) => relist = true,
                StoreEvent::FriendCameOnline(name) => {
                    // `notifications.friend_online.enabled` — the per-category
                    // gate; the per-friend gate (notify_online) already ran in
                    // the store. Only the first window raises it, or N open
                    // windows mean N identical notifications.
                    if self.is_primary()
                        && self
                            .app
                            .setting("notifications.friend_online.enabled")
                            .as_bool()
                            .unwrap_or(true)
                    {
                        let n = gio::Notification::new(&format!("{name} is online"));
                        self.app.gtk_app.send_notification(None, &n);
                        self.app.play_notify_sound("friend_online");
                    }
                }
                StoreEvent::BufferListChanged => {
                    relist = true;
                    status = true;
                    // A popout whose buffer was closed (here or on another
                    // device) closes with it — closed = absent (§9.1).
                    if let Some(key) = &self.pinned {
                        if self.app.store.borrow().buffer(key).is_none()
                            && self.app.store.borrow().backlog_complete
                        {
                            self.window.close();
                            return;
                        }
                    }
                }
                StoreEvent::ReadStateChanged(_) => relist = true,
                StoreEvent::BufferChanged(k) => {
                    if Some(k) == active.as_ref() {
                        redraw = true;
                    } else {
                        relist = true;
                    }
                }
                StoreEvent::MembersChanged(k) => {
                    if Some(k) == active.as_ref() {
                        remembers = true;
                    }
                }
                StoreEvent::NetworkChanged(_) => {
                    relist = true;
                    status = true;
                }
                StoreEvent::BacklogComplete => {
                    relist = true;
                    status = true;
                    // Nothing was selected yet — land the user somewhere real
                    // now that absence is finally meaningful (§4.3). A pinned
                    // popout never auto-selects; and if its buffer still has
                    // no row after the burst, absence is proof (§9.1): say so
                    // rather than spinning.
                    match &self.pinned {
                        None => {
                            if self.active.borrow().is_none() {
                                self.select_first_buffer();
                            }
                        }
                        Some(key) => {
                            if self.app.store.borrow().buffer(key).is_none() {
                                self.status_label
                                    .set_text("this buffer is not open on the server");
                            }
                        }
                    }
                    #[cfg(debug_assertions)]
                    if self.pinned.is_none() {
                        if let Ok(spec) = std::env::var("SCULLY_TEST_CHANCTL") {
                            if let Some((net, target)) = spec.split_once('/') {
                                let key = BufferKey::new(net.parse().ok(), target);
                                // Defer to the main loop rather than opening
                                // synchronously inside this observer callback,
                                // matching how a real double-click reaches it.
                                let app = self.app.clone();
                                glib::idle_add_local_once(move || {
                                    crate::channeldialog::open(&app, &key);
                                });
                            }
                        }
                    }
                    // Dev-build test hook: pop out a buffer named by env var
                    // once the burst lands, so popouts can be exercised
                    // headlessly (D-Bus/key injection both proved unreliable).
                    #[cfg(debug_assertions)]
                    if self.pinned.is_none() {
                        if let Ok(spec) = std::env::var("SCULLY_TEST_POPOUT") {
                            if let Some((net, target)) = spec.split_once('/') {
                                let key = BufferKey::new(net.parse().ok(), target);
                                if self.app.store.borrow().buffer(&key).is_some() {
                                    crate::open_popout(&self.app, key);
                                }
                            }
                        }
                    }
                }
                StoreEvent::SettingsChanged => {
                    self.apply_display_settings();
                    // Tier, consolidation, palette or timestamp format may
                    // have changed: invalidate the append fast-path so the
                    // whole buffer redraws consistently.
                    *self.drawn.borrow_mut() = Drawn::default();
                    redraw = true;
                    relist = true;
                    remembers = true;
                    status = true;
                }
                StoreEvent::TypingChanged(k) => {
                    if Some(k) == active.as_ref() {
                        status = true;
                    }
                }
                StoreEvent::WhoisResult(event) => self.show_whois(event),
                // The finder windows own their own result rendering.
                StoreEvent::SearchResults => {}
                // The channel browser owns its own rendering.
                StoreEvent::ChanlistResult | StoreEvent::ChanlistState => {}
                StoreEvent::PausedChanged(_) => status = true,
                // A call started/ended/changed size somewhere — repaint the
                // buffer list so its "call active (N)" badge is current.
                StoreEvent::CallPresenceChanged(_) => relist = true,
                StoreEvent::Notify(key, event) => self.raise_notification(key, event),
                StoreEvent::Error(msg) => {
                    self.status_label.set_text(msg);
                }
            }
        }

        if relist {
            self.rebuild_buffer_list();
        }
        if redraw {
            self.render_active();
            if self.window.is_active() {
                self.mark_read_to_tail();
            }
        }
        if remembers {
            self.rebuild_member_list();
        }
        // Cheap, and depends on several of the above (presence, active buffer),
        // so it runs once per batch rather than being threaded through each.
        self.refresh_voice_ui();
        if status {
            self.update_status();
        }
    }

    /// Width below which the three-pane layout stops fitting and Scully shows
    /// one pane at a time.
    ///
    /// 800, not 640: a FuriPhone is 720 logical pixels wide in portrait, so a
    /// 640 threshold never fired on the exact device this exists for — it just
    /// crushed the message column to a sliver between the sidebar and the
    /// nicklist. A desktop window dragged this narrow wants the same treatment,
    /// so the trigger stays width and never a device check.
    const NARROW_WIDTH: i32 = 800;

    /// Re-evaluate the phone layout for the current window width.
    ///
    /// Narrow: the buffer list and the conversation take turns filling the
    /// window, and the nicklist is dropped entirely — at this width a list of
    /// names costs more than it tells you. Wide: hand control back to the
    /// ordinary settings-driven layout.
    fn apply_narrow_layout(self: &Rc<Self>) {
        if self.pinned.is_some() {
            return; // a popout is already a single pane
        }
        let was = self.narrow.get();
        let now = self.window.width() > 0 && self.window.width() < Self::NARROW_WIDTH;
        self.narrow.set(now);
        if was != now {
            // Width class feeds `look.message.layout = auto` (the compact time
            // format engages on phone-sized windows), so crossing the
            // threshold re-derives the settings snapshot.
            self.apply_display_settings();
        }

        if !now {
            if was {
                // Leaving narrow mode: put the tools back and restore whatever
                // the settings say about the panes.
                self.move_tools(false);
                self.apply_display_settings();
                self.btn_back.set_visible(false);
                self.btn_members.set_visible(false);
                self.members_header.set_visible(false);
            }
            return;
        }

        // Entering narrow mode with a conversation already open should show it,
        // not bounce the reader back to the list.
        if !was {
            self.narrow_showing_list.set(self.active.borrow().is_none());
            self.narrow_showing_members.set(false);
            // The tools live in the sidebar header, which is hidden while a
            // conversation is up — so on a phone they move to the conversation
            // header, where they are reachable from where you actually are.
            self.move_tools(true);
        }
        let list = self.narrow_showing_list.get();
        let members = !list && self.narrow_showing_members.get();
        self.buffer_pane.set_visible(list);
        self.centre_pane.set_visible(!list && !members);
        self.member_pane.set_visible(members);
        self.btn_back.set_visible(!list);
        self.btn_members.set_visible(!list);
        self.members_header.set_visible(members);
    }

    /// The narrow layout's Back: members → conversation → list, one step at a
    /// time, so Back always undoes exactly the last navigation.
    fn narrow_show_list(self: &Rc<Self>) {
        if !self.narrow.get() {
            return;
        }
        if self.narrow_showing_members.get() {
            self.narrow_showing_members.set(false);
        } else {
            self.narrow_showing_list.set(true);
        }
        self.apply_narrow_layout();
    }

    /// Toggle the member list over the conversation (narrow mode only).
    fn narrow_toggle_members(self: &Rc<Self>) {
        if !self.narrow.get() || self.narrow_showing_list.get() {
            return;
        }
        let now = !self.narrow_showing_members.get();
        self.narrow_showing_members.set(now);
        self.apply_narrow_layout();
    }

    /// Move the toolbar between the sidebar header (wide) and the conversation
    /// header (narrow). Moved rather than duplicated so there is only ever one
    /// of each button, and so their existing handlers keep working.
    fn move_tools(&self, to_conversation: bool) {
        let tools = [
            &self.btn_addnet,
            &self.btn_search,
            &self.btn_call,
            &self.btn_popout,
            &self.btn_read,
            &self.btn_settings,
        ];
        for b in tools {
            if to_conversation {
                self.sidebar_header.remove(b.upcast_ref::<gtk::Widget>());
                self.header.append(b);
            } else {
                self.header.remove(b.upcast_ref::<gtk::Widget>());
                self.sidebar_header.append(b);
            }
        }
    }

    /// In narrow mode, reveal the conversation (after picking a buffer).
    fn narrow_show_conversation(self: &Rc<Self>) {
        if !self.narrow.get() {
            return;
        }
        self.narrow_showing_members.set(false);
        self.narrow_showing_list.set(false);
        self.apply_narrow_layout();
    }

    /// Pull display-relevant settings into this window's cached state and
    /// layout: sidebar visibility, timestamp format, nick palette.
    fn apply_display_settings(&self) {
        self.retint_nick_tags();
        // Striping colours may have changed; drop the cached tag so the next
        // redraw rebuilds it from the new values.
        if let Some(tag) = self.text.tag_table().lookup("row-alt") {
            self.text.tag_table().remove(&tag);
        }
        if let Some(tag) = self.text.tag_table().lookup("row-highlight") {
            self.text.tag_table().remove(&tag);
        }
        // The nicklist derives its colours from the same palette, but its
        // labels are plain widgets, not retintable tags — rebuild them.
        self.rebuild_member_list();
        let is_popout = self.pinned.is_some();
        let get_str = |key: &str, dflt: &str| {
            self.app.setting(key).as_str().map(str::to_string).unwrap_or_else(|| dflt.to_string())
        };
        let get_bool = |key: &str, dflt: bool| self.app.setting(key).as_bool().unwrap_or(dflt);
        let get_int = |key: &str, dflt: i64| self.app.setting(key).as_i64().unwrap_or(dflt);

        // `look.message.layout`: compact swaps in the compact time format.
        // "auto" means compact on a phone-sized window, standard otherwise.
        let layout = get_str("look.message.layout", "auto");
        let compact = layout == "compact" || (layout == "auto" && self.narrow.get());
        let fmt_key =
            if compact { "look.buffer.time_format_compact" } else { "look.buffer.time_format" };
        let fmt = format::time_format_to_strftime(&get_str(
            fmt_key,
            if compact { "HH:mm" } else { "HH:mm:ss" },
        ));
        *self.time_fmt.borrow_mut() = fmt.clone();

        *self.render_opts.borrow_mut() = format::RenderOpts {
            strftime: fmt,
            palette_len: self.palette_len.get(),
            stop_chars: get_str("look.nick.color_stop_chars", "_|"),
            show_event_host: get_bool("chat.show_event_host", false),
            show_join_account: get_bool("chat.show_join_account", false),
        };
        self.collapse_authors.set(get_bool("look.message.collapse_authors", false));
        self.collapse_window_secs
            .set(get_int("look.message.collapse_authors_window", 5).max(0) * 60);
        self.collapse_timestamps.set(get_bool("look.message.collapse_timestamps", true));
        self.show_mode_prefix.set(get_bool("look.nick.show_mode_prefix", false));
        self.keep_position_on_send.set(get_bool("chat.keep_position_on_send", false));
        // `input.show_format_button`: the palette lives in the composer row.
        self.btn_format.set_visible(get_bool("input.show_format_button", false));
        self.strip_on_desktop.set(get_bool("input.suggestion_strip_on_desktop", false));
        self.smart_join.set(get_bool("chat.smart_filter_join", true));
        self.smart_quit.set(get_bool("chat.smart_filter_quit", true));
        self.smart_nick.set(get_bool("chat.smart_filter_nick", true));
        self.smart_delay_secs.set(get_int("chat.smart_filter_delay", 5).max(0) * 60);
        self.smart_unmask_secs.set(get_int("chat.smart_filter_join_unmask", 30).max(0) * 60);
        *self.unread_display.borrow_mut() = get_str("look.buffer_list.unread_display", "full");
        self.unread_bold.set(get_bool("look.buffer_list.unread_bold", false));
        self.lag_min_show_ms.set(get_int("look.bar.lag_min_show_ms", 500));
        self.lag_alarm_ms.set(get_int("look.bar.lag_alarm_ms", 2000));
        self.lag_always_show.set(get_bool("look.bar.lag_always_show", false));
        *self.bar_time_fmt.borrow_mut() = self
            .app
            .setting("look.bar.time_format")
            .as_str()
            .map(format::time_format_to_strftime)
            .unwrap_or_else(|| "%H:%M:%S".to_string());
        // mIRC palette overrides: only well-formed hex entries take effect —
        // the web default carries CSS var() strings GTK cannot resolve, and
        // those fall back to the built-in table per-slot.
        *self.mirc_palette.borrow_mut() = self
            .app
            .setting("look.color.mirc_colors")
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|c| c.starts_with('#') && (c.len() == 7 || c.len() == 4))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The hanging indent is measured from the timestamp column, so a
        // format change moves it. Without this, switching to a wider format
        // leaves wrapped lines hanging under the old column.
        self.apply_indent();

        // Layout toggles govern full windows; a popout's whole point is the
        // missing sidebar, so the setting must not resurrect it.
        if !is_popout {
            if let Some(show) = self.app.setting("look.layout.show_channel_list").as_bool() {
                self.buffer_pane.set_visible(show);
            }
        }
        self.update_member_pane();
    }

    /// Show or hide the nicklist for whatever is currently open.
    ///
    /// A DM has two people in it and you are one of them, so a member list
    /// there is a column of nothing — channels only. This has to run on every
    /// buffer switch, not just when settings change: it depends on the active
    /// buffer, and at construction there isn't one, so a rule evaluated only
    /// from `apply_display_settings` latches hidden and the nicklist never
    /// comes back.
    fn update_member_pane(&self) {
        // Narrow mode owns the panes outright — there the nicklist is a
        // separate view reached from a button, not a column.
        if self.narrow.get() || self.pinned.is_some() {
            return;
        }
        let is_channel = self.active.borrow().as_ref().is_some_and(|k| k.is_channel());
        let show = self.app.setting("look.layout.show_member_list").as_bool().unwrap_or(true);
        self.member_pane.set_visible(show && is_channel);
    }

    fn raise_notification(&self, key: &BufferKey, event: &lurker_proto::MessageEvent) {
        // The store has already applied the server's notify verdict and the
        // freshness check (§5.3, §9.6). What remains is per-CATEGORY user
        // preference: the raw signals stay on the wire beside `notify`
        // precisely so the client can pick the alert kind per signal type.
        let category = if event.matched {
            "highlight"
        } else if event.dm {
            "dm"
        } else if event.notify_always {
            "always_notify"
        } else {
            ""
        };
        let enabled = category.is_empty()
            || self
                .app
                .setting(&format!("notifications.{category}.enabled"))
                .as_bool()
                .unwrap_or(true);
        if !enabled {
            return;
        }
        // The category's sound rides the same gate as its banner
        // (notifications.<cat>.sound.*), played from the server's own files.
        if !category.is_empty() {
            self.app.play_notify_sound(category);
        }
        let store = self.app.store.borrow();
        let title = store
            .buffer(key)
            .map(|b| b.display_name.clone())
            .unwrap_or_else(|| key.target.clone());
        drop(store);

        let body = format!(
            "{}: {}",
            event.nick.clone().unwrap_or_default(),
            mirc::strip(event.text.as_deref().unwrap_or(""))
        );
        let notification = gio::Notification::new(&title);
        notification.set_body(Some(&body));
        // Tapping the banner lands you in the conversation it came from —
        // the web behaves the same. The action already exists for exactly
        // this (see install_actions); the target is "<networkId>/<target>".
        let spec = format!(
            "{}/{}",
            key.network_id.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            key.target
        );
        notification.set_default_action_and_target_value(
            "app.activate-buffer",
            Some(&spec.to_variant()),
        );
        self.app.gtk_app.send_notification(None, &notification);
    }

    // ── Buffer list ───────────────────────────────────────────────────────

    fn rebuild_buffer_list(&self) {
        if self.pinned.is_some() {
            return;
        }
        let store = self.app.store.borrow();
        // clear happens below via clear_rows
        let active = self.active.borrow().clone();

        // Sections, top to bottom:
        //   ⚑ system log · ★ pinned · each network's channels · ✉ DMs · ⇄ DCC.
        // Pinned buffers (of any kind) float to their own section; DMs and DCC
        // chats are pulled out of their networks into dedicated sections so
        // they are easy to find regardless of which network they belong to.
        /// One sidebar row. Usually just a buffer, but a FRIENDS row shows the
        /// contact's display name and presence rather than the DM's nick —
        /// which is the entire point of a contact: one person, several nicks.
        struct SidebarRow {
            key: BufferKey,
            label: Option<String>,
            presence: Option<lurker_client::Presence>,
            contact: Option<i64>,
        }
        impl SidebarRow {
            fn plain(key: BufferKey) -> Self {
                Self { key, label: None, presence: None, contact: None }
            }
        }

        struct Section {
            header: Option<String>,
            offline: bool,
            keys: Vec<SidebarRow>,
            /// Section that mixes buffers from several networks (pinned, DMs,
            /// DCC). Its rows carry the network name, because pulling a buffer
            /// out of its network otherwise loses the only thing that says
            /// which `amiantos` this is.
            cross_network: bool,
            /// The network this section IS, when it is one. Only these headers
            /// offer connect/disconnect.
            network_id: Option<i64>,
            /// The FRIENDS section, whose header offers "Add a friend".
            is_friends: bool,
            /// Extra CSS class for the header's status dot, or `None` for no
            /// dot at all. Networks report their connection; FRIENDS reports
            /// whether friends are actually reachable; PINNED / DMs / DCC have
            /// no single state to report, so they get nothing.
            dot: Option<&'static str>,
        }
        let mut sections: Vec<Section> = Vec::new();
        let sort_key = |k: &BufferKey| (k.is_dm(), k.is_dcc(), k.target.clone());

        // System log (app-scoped), no header.
        let system: Vec<BufferKey> =
            store.buffers.keys().filter(|k| k.network_id.is_none()).cloned().collect();
        if !system.is_empty() {
            sections.push(Section {
                header: None,
                offline: false,
                keys: system.into_iter().map(SidebarRow::plain).collect(),
                cross_network: false,
                network_id: None,
                is_friends: false,
                dot: None,
            });
        }

        // FRIENDS — favorited DMs under the favorites model (upstream #721:
        // contacts are gone; a friend IS a favorited DM), or the legacy
        // contacts list against an older server. FAVORITES — favorited
        // channels, the model's second half.
        let mut favorite_channels: Vec<SidebarRow> = Vec::new();
        let friends: Vec<SidebarRow> = if store.favorites_model() {
            let mut friends = Vec::new();
            for f in store.favorites() {
                let key = BufferKey::new(Some(f.network_id), &f.target);
                let row = SidebarRow {
                    label: store.buffer(&key).map(|b| b.display_name.clone()),
                    presence: key
                        .is_dm()
                        .then(|| store.presence(f.network_id, &f.target)),
                    contact: None,
                    key,
                };
                if row.key.is_dm() {
                    friends.push(row);
                } else if row.key.is_channel() {
                    favorite_channels.push(row);
                }
            }
            friends
        } else {
            store
                .contacts()
                .into_iter()
                .filter_map(|c| {
                    let key = store.contact_dm_key(c)?;
                    Some(SidebarRow {
                        key,
                        label: Some(c.display_name.clone()),
                        presence: Some(store.contact_presence(c)),
                        contact: Some(c.id),
                    })
                })
                .collect()
        };
        if !friends.is_empty() {
            // The dot answers "is anyone actually there?", not "am I connected?".
            // Green when a friend is reachable, amber when the only ones on are
            // away, red when nobody is.
            let dot = if friends.iter().any(|r| r.presence == Some(lurker_client::Presence::Online))
            {
                ""
            } else if friends.iter().any(|r| r.presence == Some(lurker_client::Presence::Away)) {
                "away"
            } else {
                "offline"
            };
            sections.push(Section {
                header: Some("☺ FRIENDS".to_string()),
                offline: false,
                keys: friends,
                cross_network: true,
                network_id: None,
                is_friends: true,
                dot: Some(dot),
            });
        }
        if !favorite_channels.is_empty() {
            sections.push(Section {
                header: Some("★ FAVORITES".to_string()),
                offline: false,
                keys: favorite_channels,
                cross_network: true,
                network_id: None,
                is_friends: false,
                dot: None,
            });
        }

        // Pinned — across all networks, any buffer kind.
        let mut pinned: Vec<BufferKey> = store
            .buffers
            .iter()
            // The server unpins on favorite, but an out-of-date pin row must
            // not double-place a buffer while frames race.
            .filter(|(k, b)| b.pinned && !store.is_favorite(k))
            .map(|(k, _)| k.clone())
            .collect();
        pinned.sort_by_key(&sort_key);
        if !pinned.is_empty() {
            sections.push(Section {
                header: Some("★ PINNED".to_string()),
                offline: false,
                keys: pinned.into_iter().map(SidebarRow::plain).collect(),
                cross_network: true,
                network_id: None,
                is_friends: false,
                dot: None,
            });
        }

        // Direct messages sit directly under the pinned channels: they are
        // people, not places, and a conversation you are actually having
        // outranks the list of rooms you happen to be in.
        let mut dms: Vec<BufferKey> = store
            .buffers
            .iter()
            // A friend's DM renders under FRIENDS — favorited (new model) or
            // a contact's primary (legacy) — so hide it here, or the same
            // conversation appears twice under two names.
            .filter(|(k, b)| {
                k.is_dm() && !b.pinned && !store.is_favorite(k) && !store.is_friend_primary_dm(k)
            })
            .map(|(k, _)| k.clone())
            .collect();
        dms.sort_by_key(|k| k.target.clone());
        if !dms.is_empty() {
            sections.push(Section {
                header: Some("✉ DIRECT MESSAGES".to_string()),
                offline: false,
                keys: dms.into_iter().map(SidebarRow::plain).collect(),
                cross_network: true,
                network_id: None,
                is_friends: false,
                dot: None,
            });
        }

        // Each network: server log + its non-pinned CHANNELS (DMs and DCC go
        // to their own sections).
        for (id, net) in store.networks.iter() {
            let mut keys: Vec<BufferKey> = store
                .buffers
                .iter()
                .filter(|(k, b)| {
                    k.network_id == Some(*id)
                        && !b.pinned
                        && (k.is_server_log() || k.is_channel())
                        // Favorited channels live in FAVORITES instead.
                        && !store.is_favorite(k)
                })
                .map(|(k, _)| k.clone())
                .collect();
            keys.sort_by_key(|k| (!k.is_server_log(), k.target.clone()));
            let name = if net.name.is_empty() {
                format!("network {id}")
            } else {
                net.name.to_uppercase()
            };
            let offline = net.state != lurker_proto::NetworkState::Connected;
            sections.push(Section {
                header: Some(name),
                offline,
                keys: keys.into_iter().map(SidebarRow::plain).collect(),
                cross_network: false,
                network_id: Some(*id),
                is_friends: false,
                dot: Some(if offline { "offline" } else { "" }),
            });
        }

        // DCC chats (=nick) — non-pinned, all networks.
        let mut dcc: Vec<BufferKey> = store
            .buffers
            .iter()
            .filter(|(k, b)| k.is_dcc() && !b.pinned)
            .map(|(k, _)| k.clone())
            .collect();
        dcc.sort_by_key(|k| k.target.clone());
        if !dcc.is_empty() {
            sections.push(Section {
                header: Some("⇄ DCC CHATS".to_string()),
                offline: false,
                keys: dcc.into_iter().map(SidebarRow::plain).collect(),
                cross_network: true,
                network_id: None,
                is_friends: false,
                dot: None,
            });
        }

        self.rebuilding.set(true);
        clear_list(&self.buffer_list);
        let mut rows = Vec::new();

        // An empty sidebar is ambiguous: still starting up, or an account with
        // nothing in it? Say which, rather than leaving a blank column that
        // reads as breakage.
        if sections.is_empty() {
            let msg = match &*self.app.conn.borrow() {
                lurker_client::ConnState::Connecting => "connecting…",
                lurker_client::ConnState::Backoff(_) => "reconnecting…",
                lurker_client::ConnState::Failed(_) => "not connected",
                _ => "no conversations yet",
            };
            let row = gtk::ListBoxRow::builder()
                .child(
                    &gtk::Label::builder()
                        .label(msg)
                        .xalign(0.0)
                        .css_classes(["network-header"])
                        .build(),
                )
                .selectable(false)
                .activatable(false)
                .build();
            self.buffer_list.append(&row);
        }
        let mut select_index: Option<i32> = None;

        for section in sections {
            // A section with a header can be folded away by clicking it.
            let folded = section
                .header
                .as_ref()
                .is_some_and(|t| self.collapsed.borrow().contains(t));
            let has_header = section.header.is_some();

            if let Some(title) = &section.header {
                // Collapsing must not hide the fact that something wants
                // attention, so a folded section carries its contents' unread
                // and highlight totals on the header itself.
                let (unread, highlights) = section.keys.iter().fold((0, 0), |(u, h), r| {
                    store.buffer(&r.key).map_or((u, h), |b| (u + b.unread, h + b.highlights))
                });

                let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 5);
                header_box.add_css_class("section-header-row");
                // Baseline, not centre: these are three runs of text at three
                // different sizes. Centring their BOXES leaves the bigger
                // glyphs sitting low against the name; aligning their
                // baselines is what makes them read as one line.
                header_box.set_baseline_position(gtk::BaselinePosition::Center);
                let caret = gtk::Label::builder()
                    .label(if folded { "▸" } else { "▾" })
                    // Its own class, not the header's: the caret is sized
                    // independently of the header text.
                    .css_classes(["section-caret"])
                    .valign(gtk::Align::Baseline)
                    .build();
                // Connection dot: readable at a glance without reading the name.
                let dot = gtk::Label::builder()
                    .label("\u{25CF}")
                    .css_classes(["net-dot"])
                    .valign(gtk::Align::Baseline)
                    .build();
                if let Some(extra) = section.dot.filter(|c| !c.is_empty()) {
                    dot.add_css_class(extra);
                }
                let header = gtk::Label::builder()
                    .xalign(0.0)
                    .label(title)
                    .hexpand(true)
                    .valign(gtk::Align::Baseline)
                    .css_classes(["network-header"])
                    .build();
                if section.offline {
                    header.add_css_class("offline");
                }
                header_box.append(&caret);
                if section.dot.is_some() {
                    header_box.append(&dot);
                }
                header_box.append(&header);
                if folded && (unread > 0 || highlights > 0) {
                    let badge = gtk::Label::builder()
                        .label(if highlights > 0 {
                            highlights.to_string()
                        } else {
                            unread.to_string()
                        })
                        .css_classes(if highlights > 0 {
                            vec!["badge", "badge-highlight"]
                        } else {
                            vec!["badge"]
                        })
                        .build();
                    header_box.append(&badge);
                }

                let row = gtk::ListBoxRow::builder()
                    .child(&header_box)
                    .selectable(false)
                    .activatable(false)
                    .build();

                // Toggle on click. The controller lives on the header widget
                // rather than the list, so no row-index bookkeeping is needed
                // to work out which section was hit.
                let click = gtk::GestureClick::builder().button(1).build();
                let weak = self.clone_handle();
                let net_title = title.clone();
                let title = title.clone();
                click.connect_pressed(move |gesture, _, _, _| {
                    let Some(this) = weak.upgrade() else { return };
                    {
                        let mut set = this.collapsed.borrow_mut();
                        if !set.remove(&title) {
                            set.insert(title.clone());
                        }
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    // Rebuild on the next idle rather than here: this handler is
                    // running *inside* the very row the rebuild destroys, and
                    // tearing a widget down underneath its own live gesture
                    // controller is how this client has crashed before.
                    let weak = this.clone_handle();
                    glib::idle_add_local_once(move || {
                        if let Some(this) = weak.upgrade() {
                            this.rebuild_buffer_list();
                        }
                    });
                });
                row.add_controller(click);

                // Right-click the FRIENDS header to add one. (The nick menu and
                // `/friend` are the other two ways in.)
                if section.is_friends {
                    let right = gtk::GestureClick::builder().button(3).build();
                    let weak = self.clone_handle();
                    right.connect_pressed(move |g, _, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        if let Some(this) = weak.upgrade() {
                            crate::frienddialog::FriendDialog::add(&this.app);
                        }
                    });
                    row.add_controller(right);
                }

                // Right-click a NETWORK header: bring it up or down, or remove
                // it. Only real networks — the cross-network sections have no
                // single connection to act on.
                if let Some(net_id) = section.network_id {
                    let right = gtk::GestureClick::builder().button(3).build();
                    let weak = self.clone_handle();
                    let offline = section.offline;
                    let net_name = net_title.clone();
                    // This controller is on the ROW, so its coordinates are
                    // row-relative — unlike the buffer and nick menus, whose
                    // gestures sit on the list. Map from the row itself or every
                    // network's menu opens at the top of the pane.
                    let anchor = row.clone();
                    right.connect_pressed(move |gesture, _, x, y| {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        if let Some(this) = weak.upgrade() {
                            let (px, py) = anchor
                                .compute_point(
                                    &this.buffer_pane,
                                    &gtk::graphene::Point::new(x as f32, y as f32),
                                )
                                .map(|p| (p.x() as i32, p.y() as i32))
                                .unwrap_or((x as i32, y as i32));
                            this.open_network_menu(net_id, &net_name, offline, px, py);
                        }
                    });
                    row.add_controller(right);

                    // Same menu by press-and-hold, for touch.
                    let hold = gtk::GestureLongPress::new();
                    let weak = self.clone_handle();
                    let net_name = net_title.clone();
                    let anchor = row.clone();
                    hold.connect_pressed(move |gesture, x, y| {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        if let Some(this) = weak.upgrade() {
                            let (px, py) = anchor
                                .compute_point(
                                    &this.buffer_pane,
                                    &gtk::graphene::Point::new(x as f32, y as f32),
                                )
                                .map(|p| (p.x() as i32, p.y() as i32))
                                .unwrap_or((x as i32, y as i32));
                            this.open_network_menu(net_id, &net_name, offline, px, py);
                        }
                    });
                    row.add_controller(hold);
                }

                self.buffer_list.append(&row);
                // A non-selectable header still occupies a ListBox index, so
                // `rows` needs a placeholder to stay index-aligned. A sentinel
                // server key is harmless — header rows are never activated.
                rows.push(BufferKey::system());
            }

            if folded {
                continue;
            }

            for entry in section.keys {
                let key = entry.key;
                let buf = store.buffer(&key);
                let label = entry
                    .label
                    .clone()
                    .or_else(|| buf.map(|b| b.display_name.clone()))
                    .unwrap_or_else(|| key.target.clone());
                // A friend's row is titled by the contact, so none of the
                // buffer-kind decoration below applies to it.
                let display = if entry.label.is_some() {
                    label
                } else if key.is_server_log() {
                    "server".to_string()
                } else if key.is_system() {
                    "⚑ lurker".to_string()
                } else if key.is_dcc() {
                    // DCC chats read `=nick`; show the nick with a DCC marker.
                    format!("⇄ {}", label.trim_start_matches('='))
                } else {
                    label
                };

                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                // Friend rows carry their own presence dot (#7): every sidebar
                // label is muted by default, so without one an online friend
                // and an offline friend read identically "gray". Green
                // reachable, amber away, red offline, hollow unknown (a live
                // network that offers no MONITOR).
                if let Some(p) = entry.presence {
                    let (glyph, class) = match p {
                        lurker_client::Presence::Online => ("\u{25CF}", "online"),
                        lurker_client::Presence::Away => ("\u{25CF}", "away"),
                        lurker_client::Presence::Offline => ("\u{25CF}", "offline"),
                        lurker_client::Presence::Unknown => ("\u{25CB}", "unknown"),
                    };
                    row_box.append(
                        &gtk::Label::builder()
                            .label(glyph)
                            .css_classes(["friend-dot", class])
                            .build(),
                    );
                }
                let name = gtk::Label::builder()
                    .xalign(0.0)
                    .label(&display)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                row_box.append(&name);

                // Voice-call badge (Lurker #680): shown for any channel with an
                // active call, whether or not you're in it. The 📞 toolbar
                // button / `/call` joins it.
                let call_n = store.call_count(&key);
                if call_n > 0 {
                    let call_badge = gtk::Label::builder()
                        .label(format!("\u{260E}\u{FE0E} {call_n}"))
                        .css_classes(["badge", "badge-call"])
                        .tooltip_text("Voice call in progress — press 📞 or /call to join")
                        .build();
                    row_box.append(&call_badge);
                }

                // In a section that mixes networks, say which one this is —
                // the same nick can be two different people on two networks,
                // and the row is otherwise indistinguishable.
                if section.cross_network {
                    if let Some(net) = key
                        .network_id
                        .and_then(|id| store.networks.get(&id))
                        .map(|n| n.name.clone())
                        .filter(|n| !n.is_empty())
                    {
                        row_box.append(
                            &gtk::Label::builder()
                                .label(net)
                                // Ellipsize, or a long network name widens the
                                // row past the sidebar and pushes the badges
                                // out of sight.
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .max_width_chars(12)
                                .css_classes(["buffer-net"])
                                .build(),
                        );
                    }
                }

                let (unread, highlights) =
                    buf.map(|b| (b.unread, b.highlights)).unwrap_or((0, 0));
                // `look.buffer_list.unread_display` — how loud the badges are:
                //   full        count, highlights taking precedence (default)
                //   highlights  only highlight counts; the noisy total hidden
                //   badge       a bare dot, no numbers
                //   off         nothing at all
                let display = self.unread_display.borrow().clone();
                let badge_spec: Option<(String, bool)> = match display.as_str() {
                    "off" => None,
                    "badge" => (unread > 0 || highlights > 0)
                        .then(|| ("\u{25CF}".to_string(), highlights > 0)),
                    "highlights" => {
                        (highlights > 0).then(|| (highlights.to_string(), true))
                    }
                    _ => {
                        if highlights > 0 {
                            Some((highlights.to_string(), true))
                        } else if unread > 0 {
                            Some((unread.to_string(), false))
                        } else {
                            None
                        }
                    }
                };
                if let Some((text, hi)) = badge_spec {
                    let badge = gtk::Label::builder()
                        .label(text)
                        .css_classes(if hi {
                            vec!["badge", "badge-highlight"]
                        } else {
                            vec!["badge"]
                        })
                        .build();
                    row_box.append(&badge);
                }

                let row = gtk::ListBoxRow::builder().child(&row_box).build();
                row.add_css_class("buffer-row");
                // Rows under a header are indented behind a hairline, so a long
                // list reads as grouped rather than as one flat column.
                if has_header {
                    row.add_css_class("child");
                }
                if buf.is_some_and(|b| !b.joined) && key.is_channel() {
                    row.add_css_class("parted");
                }
                if key.is_dcc() {
                    row.add_css_class("dcc");
                }
                // A friend who isn't reachable recedes, the same way a parted
                // channel does — the row still works, it just isn't going
                // anywhere right now.
                match entry.presence {
                    Some(lurker_client::Presence::Offline) => row.add_css_class("peer-offline"),
                    Some(lurker_client::Presence::Away) => row.add_css_class("peer-away"),
                    _ => {}
                }
                if let Some(contact_id) = entry.contact {
                    // Friend rows get their own menu: this row is a person, not
                    // a buffer, so close/leave/pin would be answering the wrong
                    // question.
                    // Coordinates from a gesture on the ROW are row-relative,
                    // so map them through the row itself — mapping them as
                    // list-relative opens every menu at the top of the pane.
                    let menu_at = {
                        let anchor = row.clone();
                        move |this: &Rc<Self>, x: f64, y: f64| {
                            let (px, py) = anchor
                                .compute_point(
                                    &this.buffer_pane,
                                    &gtk::graphene::Point::new(x as f32, y as f32),
                                )
                                .map(|p| (p.x() as i32, p.y() as i32))
                                .unwrap_or((x as i32, y as i32));
                            this.open_friend_menu(contact_id, px, py);
                        }
                    };

                    let right = gtk::GestureClick::builder().button(3).build();
                    let weak = self.clone_handle();
                    let open = menu_at.clone();
                    right.connect_pressed(move |g, _, x, y| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        if let Some(this) = weak.upgrade() {
                            open(&this, x, y);
                        }
                    });
                    row.add_controller(right);

                    // Same menu by press-and-hold, for touch.
                    let hold = gtk::GestureLongPress::new();
                    let weak = self.clone_handle();
                    hold.connect_pressed(move |g, x, y| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        if let Some(this) = weak.upgrade() {
                            menu_at(&this, x, y);
                        }
                    });
                    row.add_controller(hold);
                }
                if unread > 0 || highlights > 0 {
                    row.add_css_class("has-unread");
                    // `look.buffer_list.unread_bold`: weight on top of the
                    // colour, for those who want the louder cue.
                    if self.unread_bold.get() {
                        row.add_css_class("unread-bold");
                    }
                }
                self.buffer_list.append(&row);

                if Some(&key) == active.as_ref() {
                    select_index = Some(rows.len() as i32);
                }
                rows.push(key);
            }
        }

        *self.rows.borrow_mut() = rows;
        if let Some(idx) = select_index {
            if let Some(row) = self.buffer_list.row_at_index(idx) {
                self.buffer_list.select_row(Some(&row));
            }
        }
        self.rebuilding.set(false);
    }

    /// Land the user somewhere useful once `backlog-complete` makes absence
    /// meaningful (§4.3).
    ///
    /// Prefers a joined channel, then any channel, then a DM, and only falls
    /// back to a `:server:` log if there is genuinely nothing else — opening on
    /// a wall of MOTD is a poor first impression when a real conversation
    /// exists.
    fn select_first_buffer(self: &Rc<Self>) {
        let first = {
            let store = self.app.store.borrow();
            let pick = |f: &dyn Fn(&BufferKey) -> bool| {
                store.buffers.keys().find(|k| f(k)).cloned()
            };
            pick(&|k| k.is_channel() && store.buffer(k).is_some_and(|b| b.joined))
                .or_else(|| pick(&|k| k.is_channel()))
                .or_else(|| pick(&|k| k.is_dm()))
                .or_else(|| store.buffers.keys().next().cloned())
        };
        if let Some(key) = first {
            tracing::info!(buffer = %key, "opening initial buffer");
            self.activate(&key);
        }
    }

    // ── Activation ────────────────────────────────────────────────────────

    fn activate(self: &Rc<Self>, key: &BufferKey) {
        // Leaving a buffer: persist whatever is in the composer as its draft
        // so it survives the switch (and reaches the user's other devices).
        self.save_draft();

        *self.active.borrow_mut() = Some(key.clone());
        self.pending_split_confirm.set(false);
        self.update_member_pane();
        self.update_peer_menu(key);
        // Chips from the previous buffer must not survive the switch: the
        // changed signal alone won't fire when the restored draft is
        // textually identical (both empty is the common case).
        self.refresh_suggestion_strip();
        self.paging.set(false);
        self.history_pos.set(None);
        *self.completion.borrow_mut() = None;

        {
            let store = self.app.store.borrow();
            let buf = store.buffer(key);
            // Pin the unread divider at activation (§9.4). `None` when
            // everything is read — a divider under the last line is noise.
            let last_read = buf.and_then(|b| b.last_read_id);
            let newest = buf.and_then(|b| b.newest_id);
            self.divider_after.set(match (last_read, newest) {
                (Some(read), Some(new)) if new > read => Some(read),
                _ => None,
            });
            // Restore this buffer's draft into the composer.
            let draft = buf.and_then(|b| b.draft.clone()).unwrap_or_default();
            self.entry.set_text(&draft);
            self.entry.set_position(-1);
        }

        let needs_hydrate = {
            let store = self.app.store.borrow();
            store.buffer(key).is_some_and(|b| {
                // A shell has never been filled. The second clause is the
                // belt-and-braces case: a buffer that believes it is hydrated
                // but holds nothing while the server says older rows exist is
                // indistinguishable, to the reader, from a broken client — so
                // ask again rather than presenting a permanently blank pane.
                !b.hydrated || (b.events.is_empty() && b.has_more_older)
            })
        };
        tracing::info!(buffer = %key, needs_hydrate, "activating buffer");
        if needs_hydrate {
            // §4.3: hydrate with `history`/`latest`, never `open-buffer` —
            // that verb is a WRITE that would reopen the buffer on every device
            // the user owns just because they clicked a row here.
            self.app.hydrate(key);
        }

        self.render_active();
        self.rebuild_member_list();
        self.update_status();
        self.rebuild_buffer_list();
        self.refresh_voice_ui();
        // On a phone, choosing a conversation is a navigation: show it.
        self.narrow_show_conversation();
        if self.window.is_active() {
            self.mark_read_to_tail();
        }
    }

    // ── Message rendering ─────────────────────────────────────────────────

    fn render_active(&self) {
        let Some(key) = self.active.borrow().clone() else {
            self.clear_embeds();
            *self.prev_row.borrow_mut() = None;
            self.row_parity.set(0);
            self.text.set_text("");
            return;
        };
        let store = self.app.store.borrow();
        let Some(buf) = store.buffer(&key) else {
            self.clear_embeds();
            *self.prev_row.borrow_mut() = None;
            self.row_parity.set(0);
            self.text.set_text("");
            return;
        };

        self.title_label.set_text(&buf.display_name);
        self.topic_label.set_text(&mirc::strip(buf.topic.as_deref().unwrap_or("")));
        if self.pinned.is_some() {
            self.window.set_title(Some(&format!("{} — Scully", buf.display_name)));
        }

        // When each nick last spoke — maintained by the store at ingest
        // (Buffer::last_spoke), so rendering costs zero timestamp parses.
        let last_spoke = &buf.last_spoke;
        let own_nick = key
            .network_id
            .and_then(|id| store.networks.get(&id))
            .and_then(|n| n.nick.as_deref().map(lurker_proto::fold));

        // The event-noise tier, resolved client-side (`shared/eventFilter.ts`):
        // `none` hides the noise set entirely; `smart` keeps presence events
        // only around nicks who recently spoke, tuned by the
        // `chat.smart_filter_*` settings with the web's exact semantics
        // (MessageList.vue): joins/parts+quits+chghost/nicks each toggleable,
        // "recently" is the delay window BEFORE the event, and a join is
        // revealed retroactively if the joiner speaks within the unmask
        // window AFTER it. Your own events never filter.
        let tier = self.app.event_mode.get();
        let f_join = self.smart_join.get();
        let f_quit = self.smart_quit.get();
        let f_nick = self.smart_nick.get();
        let delay_secs = self.smart_delay_secs.get();
        let unmask_secs = self.smart_unmask_secs.get();
        let events: Vec<_> = buf
            .events
            .iter()
            .filter(|e| match tier {
                lurker_proto::EventMode::None => !e.event_type.is_noise(),
                lurker_proto::EventMode::Smart => {
                    use lurker_proto::EventType as T;
                    let filterable = match e.event_type {
                        T::Join => f_join,
                        T::Part | T::Quit | T::Chghost => f_quit,
                        T::Nick => f_nick,
                        _ => false,
                    };
                    if !filterable {
                        return true;
                    }
                    // fold(), matching last_spoke's keys: `Alice[]` and
                    // `alice{}` are one person under IRC casefold.
                    let nick_lc = e.nick.as_deref().map(lurker_proto::fold);
                    if nick_lc.is_none() || nick_lc == own_nick {
                        return true;
                    }
                    let spoke = nick_lc.as_ref().and_then(|n| last_spoke.get(n)).copied();
                    let when = event_unix(e);
                    match (spoke, when) {
                        (Some(s), Some(t)) => {
                            let recently = s <= t && t - s <= delay_secs;
                            let unmasked = e.event_type == T::Join
                                && unmask_secs > 0
                                && s > t
                                && s - t <= unmask_secs;
                            recently || unmasked
                        }
                        // No timestamp to reason with — the old membership
                        // heuristic is better than hiding blind.
                        _ => nick_lc.as_ref().is_some_and(|n| last_spoke.contains_key(n)),
                    }
                }
                lurker_proto::EventMode::All => true,
            })
            .cloned()
            .collect();

        let max_names = self
            .app
            .setting("chat.consolidate_max_names")
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(consolidate::DEFAULT_MAX_NAMES);
        let opts = consolidate::Options {
            enabled: self.app.consolidate.get(),
            // The store's ingest-time speaker clock doubles as the "who
            // spoke" set — same rule (chat, not self), zero per-render scans.
            recent_speakers: Some(last_spoke.keys().cloned().collect()),
            max_names,
        };
        let rows = consolidate::consolidate(&events, &opts);
        tracing::info!(
            buffer = %key,
            stored_events = buf.events.len(),
            rendered_rows = rows.len(),
            "render"
        );

        let sigs: Vec<RowSig> = rows.iter().map(row_sig).collect();
        let first = sigs.first().copied();
        let last = sigs.last().copied();
        let prev = self.drawn.borrow().clone();

        // Append-only fast path: same buffer, same head, and the previously
        // drawn tail still sits where it was. A new event that merges into the
        // trailing run changes that row's identity, so this correctly falls
        // back to a full redraw rather than drawing the run twice.
        let can_append = prev.key.as_ref() == Some(&key)
            && prev.first == first
            && rows.len() > prev.len
            && prev.len > 0
            && sigs.get(prev.len - 1).copied() == prev.last;

        // Whether a page was prepended (older history) — used to hold the
        // reader's place instead of following the bottom.
        let prepended = !can_append && self.paging.get();
        let adj = self.scroller.vadjustment();
        let old_upper = adj.upper();
        let old_value = adj.value();

        let start_index = if can_append {
            prev.len
        } else {
            self.clear_embeds();
            *self.prev_row.borrow_mut() = None;
            self.row_parity.set(0);
            self.text.set_text("");
            0
        };

        let strftime = self.time_fmt.borrow().clone();
        let opts = self.render_opts.borrow().clone();
        let divider_after = self.divider_after.get();
        let mut divider_drawn = start_index > 0; // never re-draw mid-append
        for row in rows.iter().skip(start_index) {
            // The unread divider sits before the first row past the pinned
            // read position (§9.4 — client policy, pinned at activation).
            if !divider_drawn {
                if let (Some(after), (Some(first), _)) = (divider_after, row_sig(row)) {
                    if first > after {
                        divider_drawn = true;
                        self.append_divider();
                    }
                }
            }
            match row {
                Row::Event(e) => {
                    if let Some(line) = format::line_for(e, &opts) {
                        self.append_line(&line);
                    }
                    if e.event_type.is_chat() {
                        if let Some(text) = &e.text {
                            self.embed_media(&key, text);
                            self.embed_previews(&key, text);
                        }
                    }
                }
                Row::Summary(s) => {
                    self.append_line(&format::summary_line(s, &strftime))
                }
            }
        }

        *self.drawn.borrow_mut() = Drawn { key: Some(key), len: rows.len(), first, last };

        // Settle the view one idle tick later, once the new text is laid out.
        // `reflow` (on the adjustment's notify) does the bottom-anchor and the
        // stick-to-bottom; here we only special-case a prepended older page,
        // which must hold the reader's place rather than follow the bottom.
        self.settling.set(true);
        let this = self.clone_handle();
        glib::idle_add_local_once(move || {
            let Some(w) = this.upgrade() else { return };
            if prepended {
                w.programmatic.set(true);
                let adj = w.scroller.vadjustment();
                let delta = adj.upper() - old_upper;
                if delta > 0.0 {
                    adj.set_value(old_value + delta);
                }
                w.programmatic.set(false);
            } else {
                w.reflow();
            }
            w.settling.set(false);
            w.paging.set(false);
        });
    }


    /// A weak self-handle for deferred callbacks.
    fn clone_handle(&self) -> std::rc::Weak<Self> {
        self.weak_self.borrow().clone()
    }

    /// Insert `text` carrying every tag in `tags`.
    ///
    /// Every run also gets `no-hyphen`: Pango otherwise inserts soft hyphens
    /// when breaking inside a word, which mangles URLs into things like
    /// `cdn.lurker.ch-` / `at/` that no longer copy-paste as a link.
    fn insert_with_tags(&self, text: &str, tags: &[&str]) {
        let mut end = self.text.end_iter();
        let offset = end.offset();
        self.text.insert(&mut end, text);
        let start = self.text.iter_at_offset(offset);
        let end = self.text.end_iter();
        let table = self.text.tag_table();
        // Look the tag up and apply the object, rather than `apply_tag_by_name`
        // — which aborts the process on an unknown name (a panic across the
        // GTK C boundary cannot unwind). A missing tag now logs and is skipped.
        let apply = |name: &str| match table.lookup(name) {
            Some(tag) => self.text.apply_tag(&tag, &start, &end),
            None => tracing::warn!(tag = name, "skipping unregistered text tag"),
        };
        apply("no-hyphen");
        for tag in tags {
            apply(tag);
        }
    }

    /// Ensure a tag exists, creating it on first use.
    ///
    /// mIRC allows 99 foreground colours crossed with 99 backgrounds plus
    /// arbitrary hex, so tags are minted lazily rather than pre-registered.
    ///
    /// The name is a **construct-only** property on `GtkTextTag`, so the
    /// builder is handed to the closure already carrying it — setting `name`
    /// after `build()` aborts the process (it did: the ergo MOTD colour chart
    /// and any coloured server-buffer line hit this path).
    fn ensure_tag(&self, name: &str, build: impl FnOnce(gtk::builders::TextTagBuilder) -> gtk::TextTag) {
        let table = self.text.tag_table();
        if table.lookup(name).is_none() {
            let tag = build(gtk::TextTag::builder().name(name));
            table.add(&tag);
        }
    }

    /// Tag names for one mIRC style run.
    fn tags_for_style(&self, style: &mirc::Style) -> Vec<String> {
        let mut names = Vec::new();
        if style.bold {
            self.ensure_tag("m-bold", |b| b.weight(700).build());
            names.push("m-bold".to_string());
        }
        if style.italic {
            self.ensure_tag("m-italic", |b| b.style(gtk::pango::Style::Italic).build());
            names.push("m-italic".to_string());
        }
        if style.underline {
            self.ensure_tag("m-underline", |b| b.underline(gtk::pango::Underline::Single).build());
            names.push("m-underline".to_string());
        }
        if style.strikethrough {
            self.ensure_tag("m-strike", |b| b.strikethrough(true).build());
            names.push("m-strike".to_string());
        }
        // `look.color.mirc_colors`: a user override for the 16 basic slots
        // takes precedence over the built-in table; hex escapes and slots the
        // user left as CSS vars fall through to the default resolution.
        let user_slot = |idx: Option<u8>| -> Option<String> {
            let i = idx? as usize;
            self.mirc_palette.borrow().get(i).cloned().flatten()
        };
        let (fg_idx, bg_idx) =
            if style.reverse { (style.bg, style.fg) } else { (style.fg, style.bg) };
        if let Some(fg) = user_slot(fg_idx).or_else(|| style.fg_color()) {
            let name = format!("mfg{fg}");
            self.ensure_tag(&name, |b| b.foreground(&fg).build());
            names.push(name);
        }
        if let Some(bg) = user_slot(bg_idx).or_else(|| style.bg_color()) {
            let name = format!("mbg{bg}");
            self.ensure_tag(&name, |b| b.background(&bg).build());
            names.push(name);
        }
        names
    }

    /// The URL under a window-relative pointer position, if the character
    /// there carries the `link` tag. Expands to the tag's run and reads that
    /// text back out of the buffer.
    fn link_at(&self, x: f64, y: f64) -> Option<String> {
        let (bx, by) = self.text_view.window_to_buffer_coords(
            gtk::TextWindowType::Widget,
            x as i32,
            y as i32,
        );
        let iter = self.text_view.iter_at_location(bx, by)?;
        let table = self.text.tag_table();
        let link = table.lookup("link")?;
        if !iter.has_tag(&link) {
            return None;
        }
        let mut start = iter;
        if !start.starts_tag(Some(&link)) {
            start.backward_to_tag_toggle(Some(&link));
        }
        let mut end = iter;
        if !end.ends_tag(Some(&link)) {
            end.forward_to_tag_toggle(Some(&link));
        }
        Some(self.text.text(&start, &end, false).to_string())
    }

    /// Open a URL in the user's default browser. §Privacy: only URLs the user
    /// clicked, never anything auto-fetched.
    fn open_url(&self, url: &str) {
        let launcher = gtk::UriLauncher::new(url);
        launcher.launch(
            Some(&self.window),
            gtk::gio::Cancellable::NONE,
            |result| {
                if let Err(e) = result {
                    tracing::warn!(error = %e, "could not open link");
                }
            },
        );
    }

    /// A poster button that only constructs the real media widget — and with
    /// it the native pipeline — when the user presses play.
    fn deferred_player(url: &str, audio: bool) -> gtk::Widget {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let name = url.rsplit('/').next().unwrap_or(url);
        let poster = gtk::Button::builder()
            .label(format!("▶ {name}"))
            .tooltip_text(if audio { "Play audio" } else { "Play video" })
            .css_classes(["embed", "embed-poster"])
            .build();
        container.append(&poster);

        // Weak, or the closure (owned by the poster, owned by the container)
        // would strongly capture the container: a reference cycle that leaks
        // every embed and keeps a playing MediaFile alive past clear_embeds —
        // ghost audio with no widget left to stop it.
        let container_ref = container.downgrade();
        let url = url.to_string();
        poster.connect_clicked(move |btn| {
            let Some(container_ref) = container_ref.upgrade() else { return };
            let media = gtk::MediaFile::for_file(&gtk::gio::File::for_uri(&url));
            let widget: gtk::Widget = if audio {
                let controls = gtk::MediaControls::new(Some(&media));
                controls.set_size_request(360, -1);
                controls.upcast()
            } else {
                let video = gtk::Video::builder().media_stream(&media).build();
                video.set_size_request(420, 260);
                video.upcast()
            };
            widget.add_css_class("embed");
            container_ref.remove(btn);
            container_ref.append(&widget);

            // A backend-less GTK (the Windows bundle ships no gstreamer)
            // errors the stream instead of playing. Say so, in place of the
            // dead blank widget the user would otherwise poke at — and
            // detach the errored stream immediately, which is also what
            // makes later teardown safe (#11).
            let holder = container_ref.downgrade();
            media.connect_error_notify(move |m| {
                let Some(err) = m.error() else { return };
                let Some(holder) = holder.upgrade() else { return };
                tracing::warn!(error = %err, "media playback unavailable");
                // Detach BEFORE removing: dropping a player that still holds
                // the errored stream is the native crash this exists to stop.
                defuse_media(&holder.clone().upcast());
                clear_box(&holder);
                holder.append(
                    &gtk::Label::builder()
                        .label(format!("⚠ can't play here: {err}"))
                        .wrap(true)
                        // The cards' one-letter-column lesson, applied here
                        // too: a wrapping label in a TextView anchor gets its
                        // MINIMUM width, which is one character unless a
                        // floor says otherwise (field screenshot: the note
                        // rendered as a vertical strand of words).
                        .width_chars(32)
                        .css_classes(["embed-note"])
                        .build(),
                );
            });
            media.play();
        });
        container.upcast()
    }

    /// Remove every embedded child widget from the TextView while the buffer
    /// is still valid. Call this immediately before `set_text("")` so GTK
    /// never unmaps a child against a half-cleared buffer (the SIGSEGV in
    /// `gtk_text_view_remove` the coredump pinned).
    fn clear_embeds(&self) {
        for widget in self.embeds.borrow_mut().drain(..) {
            // Detach any media stream FIRST (#11): tearing down a gtk::Video
            // whose MediaFile is errored — a backend-less Windows install is
            // the field case — crashes natively inside unrealize. A detached
            // player tears down as an ordinary widget. Field-reproduced: play
            // a video with no gstreamer, switch buffers, native crash.
            defuse_media(&widget);
            // The widget's parent is the TextView; remove there.
            if widget.parent().is_some() {
                self.text_view.remove(&widget);
            }
        }
    }

    fn append_divider(&self) {
        let mut end = self.text.end_iter();
        if self.text.char_count() > 0 {
            self.text.insert(&mut end, "\n");
        }
        self.insert_with_tags("── unread ──", &["divider"]);
    }

    fn append_line(&self, line: &format::Line) {
        let mut end = self.text.end_iter();
        if self.text.char_count() > 0 {
            self.text.insert(&mut end, "\n");
        }
        let row_start = self.text.end_iter().offset();
        let _ = row_start;

        // Classic IRC rows: `time nick: message`, every line. Grouping exists
        // only as the server-synced settings the web client honours —
        // `look.message.collapse_timestamps` blanks a repeated identical
        // stamp, `look.message.collapse_authors` blanks a repeated author
        // within its window. Both default to what the user asked for here
        // (timestamps on, author collapse off), and both follow the website.
        let prev = self.prev_row.borrow().clone();
        let stamp = line.time.trim().to_string();
        let hide_stamp = self.collapse_timestamps.get()
            && !stamp.is_empty()
            && prev.as_ref().is_some_and(|p| p.stamp == stamp);
        if hide_stamp {
            let width = format::time_width(&self.time_fmt.borrow());
            if width > 0 {
                self.insert_with_tags(&" ".repeat(width + 1), &["time"]);
            }
        } else {
            self.insert_time(line);
        }
        match &line.author {
            Some(author) => {
                // Only plain messages collapse; a highlight names its target
                // and must never read as anonymous. Window 0 means "same
                // rendered stamp only", larger values bridge pauses.
                let window = self.collapse_window_secs.get();
                let same_author = prev
                    .as_ref()
                    .is_some_and(|p| p.author.as_deref() == Some(author.as_str()));
                let within = if window == 0 {
                    prev.as_ref().is_some_and(|p| p.stamp == stamp)
                } else {
                    match (prev.as_ref().and_then(|p| p.unix), line.unix) {
                        (Some(a), Some(b)) => (b - a).abs() <= window,
                        _ => false,
                    }
                };
                let collapse = self.collapse_authors.get()
                    && same_author
                    && within
                    && line.kind != format::LineKind::Highlight;
                if collapse {
                    // The author column is blanked, not removed — the message
                    // text stays in its column.
                    let pad = author.chars().count() + 2;
                    self.insert_with_tags(&" ".repeat(pad), &["time"]);
                } else {
                    // `look.nick.show_mode_prefix`: the speaker's current
                    // channel rank (@/%/+…) before their nick, from the live
                    // member list — absent for someone who has since left.
                    if self.show_mode_prefix.get() {
                        if let Some(sigil) = self.active.borrow().as_ref().and_then(|k| {
                            let store = self.app.store.borrow();
                            let sig = store
                                .buffer(k)
                                .and_then(|b| b.members.get(&lurker_proto::fold(author)))
                                .and_then(|m| m.sigil());
                            sig
                        }) {
                            self.insert_with_tags(&sigil.to_string(), &["time"]);
                        }
                    }
                    self.insert_with_tags(
                        author,
                        &[line.nick_tag.as_deref().unwrap_or("nick-plain"), "author"],
                    );
                    self.insert_with_tags(": ", &["time"]);
                }
            }
            None => {
                // Presence, modes, server text: the marker column (-->, <--,
                // --) stands where the nick would.
                self.insert_with_tags(line.nick.trim_start(), &[line
                    .nick_tag
                    .as_deref()
                    .unwrap_or("nick-plain")]);
                self.insert_with_tags(" ", &["time"]);
            }
        }
        *self.prev_row.borrow_mut() =
            Some(PrevRow { author: line.author.clone(), unix: line.unix, stamp });

        // The message body carries IRC formatting codes inline. Parsing splits
        // it into styled runs and drops the control characters, which is what
        // stops a colour chart rendering as a row of `▯` boxes.
        let base = line.kind.tag();
        for segment in mirc::parse(&line.text) {
            // Offset before this segment, so link ranges can be computed in
            // absolute buffer coordinates.
            let seg_start = self.text.end_iter().offset();
            if segment.style.is_plain() {
                self.insert_with_tags(&segment.text, &[base]);
            } else {
                // The inline style wins over the row's base colour, but the
                // base still applies underneath so an unstyled attribute (say
                // bold with no colour) keeps the row's own hue.
                let owned = self.tags_for_style(&segment.style);
                let mut tags: Vec<&str> = vec![base];
                tags.extend(owned.iter().map(String::as_str));
                self.insert_with_tags(&segment.text, &tags);
            }
            // Make any URLs in this segment clickable.
            for (a, b, _url) in crate::media::find_links(&segment.text) {
                let start = self.text.iter_at_offset(seg_start + a as i32);
                let end = self.text.iter_at_offset(seg_start + b as i32);
                self.text.apply_tag_by_name("link", &start, &end);
            }
        }

        // Row-level washes go on AFTER the body exists, or the tag's range
        // covers only the time/nick columns — it painted anyway (paragraph
        // backgrounds bleed across the display line) but any consumer of the
        // tag's character range would miss the message text entirely.
        //
        // `look.color.message.alt_bg` / `alt_fg`: every other row striped, as
        // the website does. Priority 0, beneath every other tag, so nick
        // colours and inline formatting still win; hex values only (the web
        // default is a CSS var GTK cannot resolve, which means "off").
        let n = self.row_parity.get();
        self.row_parity.set(n.wrapping_add(1));
        if n % 2 == 1 {
            if let Some(tag) = self.alt_row_tag() {
                let end = self.text.end_iter();
                let mut start = end;
                start.set_line_offset(0);
                self.text.apply_tag(&tag, &start, &end);
            }
        }
        // A highlight paints its WHOLE line, Spooky-style (#8): a dark-gold
        // wash makes mentions findable while scrolling. Derived from the warn
        // colour at low alpha so a custom palette recolours it too.
        if line.kind == format::LineKind::Highlight || line.matched {
            if let Some(tag) = self.highlight_row_tag() {
                let end = self.text.end_iter();
                let mut start = end;
                start.set_line_offset(0);
                self.text.apply_tag(&tag, &start, &end);
            }
        }
    }

    /// The striping tag for alternate rows, or `None` when striping is off.
    /// Cached in the tag table; `apply_display_settings` drops it so a
    /// palette edit takes effect on the next redraw.
    fn alt_row_tag(&self) -> Option<gtk::TextTag> {
        let bg = self.app.setting("look.color.message.alt_bg");
        let bg = bg.as_str().filter(|c| c.starts_with('#'))?;
        // Same colour as the base background means striping is disabled —
        // the registry's documented off switch.
        if Some(bg) == self.app.setting("look.color.bg").as_str() {
            return None;
        }
        let table = self.text.tag_table();
        if let Some(tag) = table.lookup("row-alt") {
            return Some(tag);
        }
        let builder = gtk::TextTag::builder().name("row-alt").paragraph_background(bg);
        let fg_setting = self.app.setting("look.color.message.alt_fg");
        let tag = match fg_setting.as_str().filter(|c| c.starts_with('#')) {
            Some(fg) => builder.foreground(fg).build(),
            None => builder.build(),
        };
        table.add(&tag);
        tag.set_priority(0);
        Some(tag)
    }

    /// The whole-line wash behind a highlight row. Warn colour at low alpha,
    /// so `look.color.warn` recolours it; dropped on settings changes like
    /// row-alt so edits take effect on the next redraw.
    fn highlight_row_tag(&self) -> Option<gtk::TextTag> {
        let table = self.text.tag_table();
        if let Some(tag) = table.lookup("row-highlight") {
            return Some(tag);
        }
        let warn = self
            .app
            .setting("look.color.warn")
            .as_str()
            .filter(|c| c.starts_with('#'))
            .unwrap_or("#f9d978")
            .to_string();
        let mut rgba = gtk::gdk::RGBA::parse(&warn).ok()?;
        rgba.set_alpha(0.16);
        let tag = gtk::TextTag::builder().name("row-highlight").build();
        tag.set_paragraph_background_rgba(Some(&rgba));
        table.add(&tag);
        // Above row-alt (priority 0), below every explicit style.
        tag.set_priority(1.min(table.size() - 1));
        Some(tag)
    }

    /// Write the leading timestamp column for a row.
    ///
    /// Padded to the format's own rendered width rather than trimmed, so rows
    /// that carry no time still occupy the column and the text beside them
    /// stays aligned.
    fn insert_time(&self, line: &format::Line) {
        let width = format::time_width(&self.time_fmt.borrow());
        if width == 0 {
            return;
        }
        let stamp = line.time.trim();
        let padded = format!("{stamp:<width$} ");
        self.insert_with_tags(&padded, &["time"]);
    }

    /// Columns before the message text — the timestamp plus its trailing space.
    ///
    /// Everything that has to line up with the text uses this: the hanging
    /// indent for wrapped lines, and the blank run that pushes an inline
    /// preview into the same column. Inline previews are the one thing that
    /// does *not* get a timestamp of its own; it belongs to the row above.
    fn text_column(&self) -> usize {
        match format::time_width(&self.time_fmt.borrow()) {
            0 => 0,
            w => w + 1,
        }
    }

    /// Push short content down so the buffer reads up from the bottom edge.
    ///
    /// A `GtkTextView` is a `GtkScrollable`, so a `ScrolledWindow` always
    /// allocates it the full viewport and `valign` has no effect. The only
    /// lever is the top margin: pad by whatever the content is short by, which
    /// gives the terminal-like behaviour of new lines always appearing in the
    /// same place instead of the buffer growing downward from the top.
    /// Bottom-anchor short content and keep a following view pinned to the
    /// bottom — driven by the scroll adjustment, which reports the real content
    /// height (a `GtkTextView`'s own `measure()` does not; it is scrollable and
    /// returns a minimal height, which once padded the whole viewport and
    /// pushed every line but the first off-screen).
    ///
    /// Runs on `notify::upper`/`notify::page-size`, i.e. whenever layout
    /// settles — new lines, a resize, or an async image finishing — so it is
    /// always working with final geometry.
    fn reflow(&self) {
        if self.programmatic.get() {
            return;
        }
        let adj = self.scroller.vadjustment();
        let page = adj.page_size();
        if page <= 0.0 {
            return;
        }
        self.programmatic.set(true);

        // Content height excluding the pad we previously added.
        let margin = self.text_view.top_margin() as f64;
        let content = (adj.upper() - margin).max(0.0);
        let target = if content + f64::from(BASE_TOP_MARGIN) < page {
            // Short buffer: pad the top so it reads up from the bottom edge.
            (page - content) as i32
        } else {
            // Overflows the viewport: minimal margin; scrolling does the rest.
            BASE_TOP_MARGIN
        };
        if target != margin as i32 {
            self.text_view.set_top_margin(target);
        }

        if self.stick_bottom.get() {
            let adj = self.scroller.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        }
        self.programmatic.set(false);
    }

    fn request_older_page(&self) {
        let Some(key) = self.active.borrow().clone() else { return };
        let store = self.app.store.borrow();
        let Some(buf) = store.buffer(&key) else { return };
        // Never page a buffer that has not been filled yet — that is what
        // hydration is for, and asking here would race it.
        if !buf.has_more_older || !buf.hydrated || buf.events.is_empty() {
            return;
        }
        let Some(oldest) = buf.events.iter().find_map(|e| e.id) else { return };
        drop(store);
        self.paging.set(true);
        self.app.page_older(&key, oldest);
    }

    fn mark_read_to_tail(&self) {
        let Some(key) = self.active.borrow().clone() else { return };
        let store = self.app.store.borrow();
        let Some(buf) = store.buffer(&key) else { return };
        // The system buffer has its own id sequence but marking is per-buffer,
        // so its own newest id is the right value here.
        let Some(newest) = buf.events.iter().filter_map(|e| e.id).max() else { return };
        if buf.last_read_id == Some(newest) {
            return;
        }
        drop(store);
        self.app.mark_read(&key, newest);
    }

    /// A snapshot of the sidebar's activity, index-aligned with `rows`.
    fn activity_rows(&self) -> Vec<crate::attention::Activity> {
        let store = self.app.store.borrow();
        let rows = self.rows.borrow();
        rows.iter()
            .enumerate()
            .map(|(i, key)| {
                // Header placeholders reuse the system key; the real system
                // buffer is only ever at a row that the list actually selected.
                let is_header = self
                    .buffer_list
                    .row_at_index(i as i32)
                    .is_some_and(|r| !r.is_selectable());
                match store.buffer(key) {
                    Some(b) if !is_header => crate::attention::Activity {
                        unread: b.unread,
                        highlights: b.highlights,
                        selectable: true,
                    },
                    // A row with no buffer yet (a shell) is still somewhere the
                    // user can go, it just has nothing waiting.
                    _ => crate::attention::Activity {
                        selectable: !is_header,
                        ..crate::attention::Activity::none()
                    },
                }
            })
            .collect()
    }

    /// Jump to the next buffer with unread activity (Alt+A), preferring
    /// highlights. Says so in the status line when there is nothing to go to,
    /// rather than appearing to do nothing.
    fn jump_to_attention(self: &Rc<Self>) {
        let rows = self.activity_rows();
        let current = self
            .active
            .borrow()
            .as_ref()
            .and_then(|k| self.rows.borrow().iter().position(|r| r == k));
        match crate::attention::next_attention(&rows, current) {
            Some(idx) => {
                let key = self.rows.borrow().get(idx).cloned();
                if let Some(key) = key {
                    self.activate(&key);
                }
            }
            None => self.status_label.set_text("nothing unread"),
        }
    }

    /// Jump to the Nth buffer in the sidebar (Alt+1…Alt+9).
    fn jump_to_nth(self: &Rc<Self>, nth: usize) {
        let rows = self.activity_rows();
        if let Some(idx) = crate::attention::nth_selectable(&rows, nth) {
            let key = self.rows.borrow().get(idx).cloned();
            if let Some(key) = key {
                self.activate(&key);
            }
        }
    }

    /// Show or hide the voice-call affordances and refresh their labels.
    ///
    /// Called whenever anything they depend on moves: the `voiceEnabled`
    /// capability landing (asynchronously, after this window exists), the active
    /// buffer changing, or a `call-presence` update. Cheap enough to run
    /// unconditionally rather than tracking which input changed.
    pub fn refresh_voice_ui(&self) {
        let key = self.active.borrow().clone();
        // Calls are per-conversation: channels and DMs, not server/system logs.
        let callable = key.as_ref().is_some_and(|k| k.is_channel() || k.is_dm());
        let show = self.app.voice_enabled.get() && callable;

        self.btn_call.set_visible(show);
        self.call_panel.set_visible(show);
        if !show {
            return;
        }

        let count = key.map(|k| self.app.store.borrow().call_count(&k)).unwrap_or(0);

        self.call_button.set_visible(true);
        if count > 0 {
            self.call_status.set_text(&format!("\u{260E}\u{FE0E} Call in progress — {count} in call"));
            self.call_button.set_label("Join call");
        } else {
            self.call_status.set_text("No voice call here yet");
            self.call_button.set_label("Start call");
        }
    }

    /// Start or join a call in the active buffer, or say why we can't.
    ///
    /// The two ways this used to fail silently — a build without the `voice`
    /// feature, and a server that doesn't offer voice — now each report
    /// themselves in the status line instead of doing nothing (or, for `/call`,
    /// falling through to the raw IRC fallback and returning "Unknown command").
    fn try_start_call(self: &Rc<Self>) {
        let Some(key) = self.active.borrow().clone() else {
            self.status_label.set_text("no conversation is active");
            return;
        };
        if !(key.is_channel() || key.is_dm()) {
            self.status_label.set_text("voice calls only work in a channel or DM");
            return;
        }
        if !self.app.voice_enabled.get() {
            self.status_label
                .set_text("this server does not have voice calls enabled");
            return;
        }
        #[cfg(feature = "voice")]
        {
            self.status_label.set_text("connecting to the call…");
            self.app.start_call(key, self.title_label.text().to_string());
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = key;
            self.status_label.set_text(
                "this build has no voice support — rebuild with: cargo run -p scully --features voice",
            );
        }
    }

    /// The clicker's own authority in the active channel — from their own
    /// nick's highest prefix mode.
    fn my_rank(&self) -> crate::nickmenu::Rank {
        let Some(key) = self.active.borrow().clone() else {
            return crate::nickmenu::Rank::None;
        };
        let store = self.app.store.borrow();
        let my_nick = key
            .network_id
            .and_then(|id| store.networks.get(&id))
            .and_then(|n| n.nick.clone());
        let Some(my_nick) = my_nick else { return crate::nickmenu::Rank::None };
        store
            .buffer(&key)
            .and_then(|b| b.members.get(&lurker_proto::fold(&my_nick)))
            .map(|m| crate::nickmenu::Rank::from_mode(m.highest_mode()))
            .unwrap_or(crate::nickmenu::Rank::None)
    }

    /// Build and pop up the nick menu, gated to the clicker's authority.
    ///
    /// The menu is rebuilt each time (rank and target vary) and the action
    /// group is attached to the popover itself, so every item resolves —
    /// which is what stops actions rendering greyed-out and dead, the symptom
    /// of an action group the menu could not see.
    fn open_nick_menu(self: &Rc<Self>, nick: &str, x: i32, y: i32) {
        // Dismiss any lingering menu.
        if let Some(old) = self.nick_menu.borrow_mut().take() {
            old.unparent();
        }

        let rank = self.my_rank();
        let model = crate::nickmenu::menu_model(rank);

        // Call moderation, appended only when there IS a call in this channel
        // and the clicker can moderate it (Lurker #680). Hidden otherwise, so
        // the menu never offers an action that can only 403.
        let call_live = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|k| self.app.store.borrow().call_count(k) > 0);
        if call_live && crate::callmod::can_moderate_call(rank) {
            let call = gio::Menu::new();
            call.append_item(&gio::MenuItem::new(
                Some("Mute in call"),
                Some("nick.cmd::call-mute"),
            ));
            call.append_item(&gio::MenuItem::new(
                Some("Remove from call"),
                Some("nick.cmd::call-remove"),
            ));
            model.append_section(Some("Voice call"), &call);
        }

        // Friends. The label states which it will do, so the item never
        // silently acts on state the user forgot. Under the favorites model
        // (upstream #721) a friend IS a favorited DM; against a legacy server
        // it is a contact.
        let already = self
            .active
            .borrow()
            .as_ref()
            .and_then(|k| k.network_id)
            .is_some_and(|net| {
                let store = self.app.store.borrow();
                if store.favorites_model() {
                    store.is_favorite(&BufferKey::new(Some(net), nick))
                } else {
                    store.contact_for(net, nick).is_some()
                }
            });
        let friend = gio::Menu::new();
        friend.append_item(&gio::MenuItem::new(
            Some(if already {
                if self.app.store.borrow().favorites_model() {
                    "Remove from friends"
                } else {
                    "Edit friend\u{2026}"
                }
            } else {
                "Add to friends\u{2026}"
            }),
            Some("nick.cmd::friend"),
        ));
        model.append_section(None, &friend);

        let this = self.clone();
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, param| {
            // Without a grab the menu does not dismiss itself on activate.
            this.close_menus();
            if let Some(id) = param.and_then(|p| p.get::<String>()) {
                this.run_nick_command(&id);
            }
        });
        group.add_action(&action);

        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.insert_action_group("nick", Some(&group));
        // Parented to the PANE, not the list. A popover anchored inside a
        // ScrolledWindow is constrained to the scrolled viewport, so GTK gives
        // it its own scrollbar — a five-item menu that scrolls. The pane does
        // not scroll, so the menu is free to be its natural height.
        let (px, py) = self
            .member_list
            .compute_point(&self.member_pane, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (p.x() as i32, p.y() as i32))
            .unwrap_or((x, y));
        popover.set_parent(&self.member_pane);
        popover.set_autohide(false);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(px, py, 1, 1)));

        popover.popup();
        *self.nick_menu.borrow_mut() = Some(popover);
    }

    /// Point the conversation's context menu at the current buffer: in a DM
    /// the built-in Copy/Select-All menu gains the peer's person actions
    /// (#6); everywhere else it stays stock.
    fn update_peer_menu(&self, key: &BufferKey) {
        if !key.is_dm() {
            self.text_view.set_extra_menu(None::<&gio::MenuModel>);
            return;
        }
        let store = self.app.store.borrow();
        let peer = store
            .buffer(key)
            .map(|b| b.display_name.clone())
            .unwrap_or_else(|| key.target.clone());
        let already = key
            .network_id
            .is_some_and(|net| store.contact_for(net, &peer).is_some());
        drop(store);
        *self.menu_nick.borrow_mut() = peer;

        // Rank::None: there is no channel here, so no mode ladder — just the
        // person actions (whois, query, slap, CTCP, ignore) plus friends.
        let model = crate::nickmenu::menu_model(crate::nickmenu::Rank::None);
        let friend = gio::Menu::new();
        friend.append_item(&gio::MenuItem::new(
            Some(if already {
                if self.app.store.borrow().favorites_model() {
                    "Remove from friends"
                } else {
                    "Edit friend\u{2026}"
                }
            } else {
                "Add to friends\u{2026}"
            }),
            Some("nick.cmd::friend"),
        ));
        model.append_section(None, &friend);
        self.text_view.set_extra_menu(Some(&model));
    }

    /// Open the buffer-list context menu for `key` at (x, y) in the list. The
    /// item set is tailored to the buffer's kind and state (see buffermenu.rs);
    /// the action group is attached to the popover so every item resolves.
    fn open_buffer_menu(self: &Rc<Self>, key: BufferKey, x: i32, y: i32) {
        if let Some(old) = self.buffer_menu.borrow_mut().take() {
            old.unparent();
        }

        let (in_store, joined, pinned) = {
            let store = self.app.store.borrow();
            match store.buffer(&key) {
                Some(b) => (true, b.joined, b.pinned),
                None => (false, false, false),
            }
        };
        let (favorite, favorites_model) = {
            let store = self.app.store.borrow();
            (store.is_favorite(&key), store.favorites_model())
        };
        let cx = crate::buffermenu::BufContext {
            is_channel: key.is_channel(),
            is_dm: key.is_dm(),
            in_store,
            joined,
            pinned,
            favorite,
            favorites_model,
        };
        let model = crate::buffermenu::menu_model(&cx);

        let this = self.clone();
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, param| {
            this.close_menus();
            if let Some(id) = param.and_then(|p| p.get::<String>()) {
                this.run_buffer_command(&id);
            }
        });
        group.add_action(&action);

        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.insert_action_group("buf", Some(&group));
        // Parented to the pane rather than the scrolled list — see
        // open_nick_menu: anchoring inside a ScrolledWindow makes GTK clamp the
        // menu to the viewport and give a four-item menu a scrollbar.
        let (px, py) = self
            .buffer_list
            .compute_point(&self.buffer_pane, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (p.x() as i32, p.y() as i32))
            .unwrap_or((x, y));
        popover.set_parent(&self.buffer_pane);
        popover.set_autohide(false);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(px, py, 1, 1)));

        *self.menu_buffer.borrow_mut() = Some(key);
        popover.popup();
        *self.buffer_menu.borrow_mut() = Some(popover);
    }

    /// Open the context menu for a FRIENDS row.
    ///
    /// Deliberately short: this row is a *person*, so the buffer menu's
    /// close/leave/pin would be answering the wrong question. Removal sits
    /// behind the editor's own Remove button rather than one stray click away.
    fn open_friend_menu(self: &Rc<Self>, contact_id: i64, x: i32, y: i32) {
        if let Some(old) = self.buffer_menu.borrow_mut().take() {
            old.unparent();
        }

        let model = gio::Menu::new();
        model.append_item(&gio::MenuItem::new(Some("Open DM"), Some("friend.open")));
        model.append_item(&gio::MenuItem::new(Some("Edit friend…"), Some("friend.edit")));

        let group = gio::SimpleActionGroup::new();

        let open = gio::SimpleAction::new("open", None);
        let this = self.clone();
        open.connect_activate(move |_, _| {
            this.close_menus();
            let key = {
                let store = this.app.store.borrow();
                store.contact(contact_id).and_then(|c| store.contact_dm_key(c))
            };
            if let Some(key) = key {
                this.open_friend_dm(&key);
            }
        });
        group.add_action(&open);

        let edit = gio::SimpleAction::new("edit", None);
        let this = self.clone();
        edit.connect_activate(move |_, _| {
            this.close_menus();
            crate::frienddialog::FriendDialog::edit(&this.app, contact_id);
        });
        group.add_action(&edit);

        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.insert_action_group("friend", Some(&group));
        popover.set_parent(&self.buffer_pane);
        popover.set_autohide(false);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x, y, 1, 1)));
        popover.popup();
        *self.buffer_menu.borrow_mut() = Some(popover);
    }

    /// Open a friend's DM, asking the server for it first if we have never
    /// held that buffer. Clicking a friend is explicit user intent, which is
    /// the one case `open-buffer` is for (§4.3).
    pub fn open_friend_dm(self: &Rc<Self>, key: &BufferKey) {
        if !self.app.store.borrow().buffers.contains_key(key) {
            self.app.open_buffer(key);
        }
        self.open_key(key);
    }

    /// Show `content` as a lightbox over this window's conversation.
    ///
    /// Dismissed by clicking the backdrop or pressing Escape — clicks on the
    /// content itself are swallowed, so interacting with the media does not
    /// close it. Only one is ever up; opening a second replaces the first.
    fn show_overlay(self: &Rc<Self>, content: &impl IsA<gtk::Widget>, caption: &str) {
        self.dismiss_overlay();

        let scrim = gtk::Box::new(gtk::Orientation::Vertical, 8);
        scrim.add_css_class("media-scrim");

        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.add_css_class("media-body");
        body.set_halign(gtk::Align::Center);
        body.set_valign(gtk::Align::Center);
        body.append(content);
        if !caption.is_empty() {
            body.append(
                &gtk::Label::builder()
                    .label(caption)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .css_classes(["media-caption"])
                    .build(),
            );
        }

        // Clicks on the body stop here, so only the backdrop dismisses.
        let swallow = gtk::GestureClick::builder().button(0).build();
        swallow.connect_pressed(|g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
        });
        body.add_controller(swallow);

        scrim.append(&body);

        let weak = self.clone_handle();
        let close = gtk::GestureClick::builder().button(1).build();
        close.connect_released(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.dismiss_overlay();
            }
        });
        scrim.add_controller(close);

        self.overlay.add_overlay(&scrim);
        *self.media_overlay.borrow_mut() = Some(scrim);
    }

    /// Take down the lightbox, if one is up. Returns whether it did anything,
    /// so Escape can fall through to its other meanings when it isn't.
    fn dismiss_overlay(&self) -> bool {
        match self.media_overlay.borrow_mut().take() {
            Some(scrim) => {
                self.overlay.remove_overlay(&scrim);
                true
            }
            None => false,
        }
    }

    /// Open an image in the lightbox, with click-to-toggle actual size.
    fn view_image(self: &Rc<Self>, texture: &gtk::gdk::Texture, title: &str) {
        let picture = gtk::Picture::for_paintable(texture);
        picture.set_can_shrink(true);
        picture.set_halign(gtk::Align::Center);
        picture.set_valign(gtk::Align::Center);
        // Fit inside the window with a margin, rather than at native size.
        let (w, h) = (texture.width() as f64, texture.height() as f64);
        let (max_w, max_h) =
            ((self.window.width() as f64 - 120.0).max(320.0), (self.window.height() as f64 - 160.0).max(240.0));
        let scale = (max_w / w).min(max_h / h).min(1.0);
        picture.set_size_request((w * scale) as i32, (h * scale) as i32);
        picture.set_cursor_from_name(Some("zoom-in"));

        // Click the image to toggle between fitted and actual size.
        let actual = std::rc::Rc::new(std::cell::Cell::new(false));
        let tex = texture.clone();
        let pic = picture.clone();
        let toggle = gtk::GestureClick::builder().button(1).build();
        toggle.connect_released(move |_, _, _, _| {
            let now = !actual.get();
            actual.set(now);
            if now {
                pic.set_size_request(tex.width(), tex.height());
                pic.set_cursor_from_name(Some("zoom-out"));
            } else {
                pic.set_size_request((w * scale) as i32, (h * scale) as i32);
                pic.set_cursor_from_name(Some("zoom-in"));
            }
        });
        picture.add_controller(toggle);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&picture)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .build();
        scroller.set_size_request((w * scale) as i32, (h * scale) as i32);
        self.show_overlay(&scroller, title);
    }

    /// Ask for a channel to join on `net_id`, then join it.
    ///
    /// A small prompt rather than a full dialog: the only real input is the
    /// name, and an optional key for `+k` channels.
    fn prompt_join(self: &Rc<Self>, net_id: i64) {
        let window = gtk::Window::builder()
            .title("Join a channel")
            .default_width(360)
            .transient_for(&self.window)
            .destroy_with_parent(true)
            .build();
        window.add_css_class("chanctl");

        let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
        outer.set_margin_top(14);
        outer.set_margin_bottom(14);
        outer.set_margin_start(16);
        outer.set_margin_end(16);
        let name = gtk::Entry::builder().placeholder_text("#channel").build();
        let key = gtk::Entry::builder().placeholder_text("key (optional, for +k)").build();
        key.set_visibility(false);
        outer.append(&name);
        outer.append(&key);
        let go = gtk::Button::with_label("Join");
        outer.append(&go);
        window.set_child(Some(&outer));

        let this = self.clone();
        let win = window.clone();
        let name_e = name.clone();
        let key_e = key.clone();
        let join = move || {
            let mut channel = name_e.text().trim().to_string();
            if channel.is_empty() {
                return;
            }
            // A bare name is almost always meant as a channel; prepending the
            // sigil saves the server rejecting it.
            if !channel.starts_with(['#', '&', '!', '+']) {
                channel.insert(0, '#');
            }
            let k = key_e.text().trim().to_string();
            let chan_key = BufferKey::new(Some(net_id), &channel);
            this.app.store.borrow_mut().note_pending_join(chan_key);
            this.app.send(ClientVerb::Join {
                network_id: net_id,
                channel,
                key: (!k.is_empty()).then_some(k),
            });
            win.close();
        };
        let j = join.clone();
        go.connect_clicked(move |_| j());
        name.connect_activate(move |_| join());
        window.present();
    }

    /// Context menu for a network header: connect / disconnect / reconnect,
    /// add another, or remove this one.
    ///
    /// These act on the *bouncer's* connection, so they reach every device on
    /// the account — which is why removal asks first.
    /// Close any open context menu. Returns whether one was up.
    fn close_menus(&self) -> bool {
        let mut closed = false;
        if let Some(p) = self.nick_menu.borrow_mut().take() {
            p.unparent();
            closed = true;
        }
        if let Some(p) = self.buffer_menu.borrow_mut().take() {
            p.unparent();
            closed = true;
        }
        closed
    }

    /// `x`/`y` are in the sidebar PANE's coordinate space, already mapped by
    /// the caller — only it knows which row was clicked.
    fn open_network_menu(
        self: &Rc<Self>,
        net_id: i64,
        net_name: &str,
        offline: bool,
        x: i32,
        y: i32,
    ) {
        if let Some(old) = self.buffer_menu.borrow_mut().take() {
            old.unparent();
        }

        let model = gio::Menu::new();
        let conn = gio::Menu::new();
        if offline {
            conn.append_item(&gio::MenuItem::new(Some("Connect"), Some("net.cmd::connect")));
        } else {
            conn.append_item(&gio::MenuItem::new(Some("Disconnect"), Some("net.cmd::disconnect")));
            conn.append_item(&gio::MenuItem::new(Some("Reconnect"), Some("net.cmd::reconnect")));
        }
        model.append_section(None, &conn);
        let manage = gio::Menu::new();
        if !offline {
            manage.append_item(&gio::MenuItem::new(
                Some("Join a channel…"),
                Some("net.cmd::join"),
            ));
        }
        manage.append_item(&gio::MenuItem::new(
            Some("Browse channels…"),
            Some("net.cmd::browse"),
        ));
        manage.append_item(&gio::MenuItem::new(Some("Edit this network…"), Some("net.cmd::edit")));
        manage.append_item(&gio::MenuItem::new(Some("Add a network…"), Some("net.cmd::add")));
        manage.append_item(&gio::MenuItem::new(
            Some("Remove this network…"),
            Some("net.cmd::remove"),
        ));
        model.append_section(None, &manage);

        let this = self.clone();
        let name = net_name.to_string();
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, param| {
            this.close_menus();
            let Some(id) = param.and_then(|p| p.get::<String>()) else { return };
            this.run_network_command(net_id, &name, &id);
        });
        group.add_action(&action);

        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.insert_action_group("net", Some(&group));
        // x/y arrive already mapped into the pane by the caller, which is the
        // only place that knows which row was clicked.
        let (px, py) = (x, y);
        popover.set_parent(&self.buffer_pane);
        popover.set_autohide(false);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(px, py, 1, 1)));
        popover.popup();
        *self.buffer_menu.borrow_mut() = Some(popover);
    }

    fn run_network_command(self: &Rc<Self>, net_id: i64, net_name: &str, id: &str) {
        let status = self.status_label.clone();
        match id {
            "connect" | "disconnect" | "reconnect" => {
                let action: &'static str = match id {
                    "connect" => "connect",
                    "disconnect" => "disconnect",
                    _ => "reconnect",
                };
                let name = net_name.to_string();
                status.set_text(&format!("{action}ing {name}…"));
                let done = status.clone();
                self.app.network_action(net_id, action, move |res| match res {
                    Ok(()) => done.set_text(&format!("{name}: {action} requested")),
                    Err(e) => done.set_text(&format!("{action} failed — {e}")),
                });
            }
            "add" => crate::networkdialog::open(&self.app, self.window.clone().upcast_ref()),
            "edit" => {
                // The roster carries fields the WS snapshot omits, so fetch the
                // row before showing a form that would otherwise blank them.
                let win = self.window.clone();
                let app = self.app.clone();
                let status = status.clone();
                self.app.fetch_network_row(net_id, move |row| match row {
                    Some(row) => {
                        crate::networkdialog::open_edit(&app, win.clone().upcast_ref(), row)
                    }
                    None => status.set_text("could not read that network"),
                });
            }
            "join" => self.prompt_join(net_id),
            "browse" => {
                crate::chanlist::open(&self.app, net_id, self.window.clone().upcast_ref())
            }
            "remove" => {
                // Destructive and account-wide: confirm, and name the network so
                // there is no doubt which one is about to go.
                let dialog = gtk::AlertDialog::builder()
                    .message(format!("Remove {net_name}?"))
                    .detail(
                        "This deletes the network and its buffers for every device on your \
                         account. It cannot be undone.",
                    )
                    .buttons(["Cancel", "Remove"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                let app = self.app.clone();
                let status = status.clone();
                let name = net_name.to_string();
                dialog.choose(
                    Some(&self.window),
                    gtk::gio::Cancellable::NONE,
                    move |answer| {
                        if answer.ok() != Some(1) {
                            return;
                        }
                        let status = status.clone();
                        let name = name.clone();
                        app.delete_network(net_id, move |res| match res {
                            Ok(()) => status.set_text(&format!("{name} removed")),
                            Err(e) => status.set_text(&format!("could not remove — {e}")),
                        });
                    },
                );
            }
            _ => {}
        }
    }

    /// Dispatch a buffer-menu id against the buffer the menu was opened on.
    fn run_buffer_command(self: &Rc<Self>, id: &str) {
        debug_assert!(crate::buffermenu::id_is_known(id), "unknown buffer-menu id {id}");
        let Some(key) = self.menu_buffer.borrow().clone() else { return };
        let target = self.app.wire_target(&key);
        match id {
            "popout" => {
                let exists = self.app.store.borrow().buffer(&key).is_some();
                if exists {
                    crate::open_popout(&self.app, key);
                }
            }
            "favorite" => {
                if let Some(net) = key.network_id {
                    self.app.send(ClientVerb::FavoriteBuffer { network_id: net, target });
                }
            }
            "unfavorite" => {
                if let Some(net) = key.network_id {
                    self.app.send(ClientVerb::UnfavoriteBuffer { network_id: net, target });
                }
            }
            "whois" => {
                // Only offered on DM rows, whose target is the peer's nick.
                // The reply is routed to the DM that was right-clicked, NOT
                // the active buffer — whois for one person must never render
                // into an unrelated channel that happened to be on screen.
                if let Some(net) = key.network_id {
                    self.note_whois_for(&target, key.clone());
                    for verb in crate::nickmenu::verbs_for(
                        crate::nickmenu::Cmd::Whois,
                        net,
                        "",
                        &target,
                        None,
                    ) {
                        self.app.send(verb);
                    }
                }
            }
            "read" => {
                let newest = self.app.store.borrow().buffer(&key).and_then(|b| b.newest_id);
                if let Some(mid) = newest {
                    self.app.mark_read(&key, mid);
                }
            }
            "pin" => {
                self.app.send(ClientVerb::PinBuffer { network_id: key.network_id, target });
            }
            "unpin" => {
                self.app.send(ClientVerb::UnpinBuffer { network_id: key.network_id, target });
            }
            "part" => {
                if let Some(net) = key.network_id {
                    self.app.send(ClientVerb::Part { network_id: net, channel: target, reason: None });
                }
            }
            "join" => {
                if let Some(net) = key.network_id {
                    self.app.store.borrow_mut().note_pending_join(key.clone());
                    self.app.send(ClientVerb::Join { network_id: net, channel: target, key: None });
                }
            }
            "close" => {
                self.app.send(ClientVerb::CloseBuffer {
                    network_id: key.network_id,
                    target,
                    reason: None,
                });
            }
            _ => {}
        }
    }

    /// Open message search. `everywhere` searches all conversations; otherwise
    /// it is scoped to the active buffer.
    fn open_search(self: &Rc<Self>, everywhere: bool) {
        let scope = if everywhere { None } else { self.active.borrow().clone() };
        let this = self.clone();
        crate::finder::open_search(&self.app, scope, move |key, message_id| {
            // Open the buffer, then fetch the window around the match.
            this.activate(key);
            this.app.jump_to(key, message_id);
        });
    }

    /// Open the buffer quick-switcher.
    fn open_switcher(self: &Rc<Self>) {
        let this = self.clone();
        crate::finder::open_switcher(&self.app, move |key| this.activate(key));
    }

    /// If the active buffer is detached by a jump, return it to the live
    /// present. Returns whether anything was detached.
    fn return_to_present(&self) -> bool {
        let Some(key) = self.active.borrow().clone() else { return false };
        let detached = self
            .app
            .store
            .borrow()
            .buffer(&key)
            .is_some_and(|b| b.detached);
        if detached {
            self.app.reattach(&key);
            self.status_label.set_text("returned to the present");
        }
        detached
    }

    /// Record a pending whois for the active buffer, if the "show in active
    /// buffer" setting is on. Called from both the nick menu and `/whois`.
    fn note_whois(&self, nick: &str) {
        let dest = self.active.borrow().clone();
        match dest {
            Some(key) => self.note_whois_for(nick, key),
            None => self.status_label.set_text(&format!("whois {nick}…")),
        }
    }

    /// Route a pending whois reply to an explicit buffer — the DM-row menu's
    /// case, where the buffer the user asked FROM is not the one on screen.
    fn note_whois_for(&self, nick: &str, key: BufferKey) {
        if self.app.device.borrow().whois_in_active_buffer {
            self.pending_whois.borrow_mut().insert(lurker_proto::fold(nick), key);
            self.status_label.set_text(&format!("whois {nick}…"));
        } else {
            self.status_label.set_text("whois sent — reply lands in the server log");
        }
    }

    /// Render a whois result in the buffer this window recorded for it, if any.
    fn show_whois(self: &Rc<Self>, event: &lurker_proto::MessageEvent) {
        let Some(whois) = &event.whois else { return };
        // The payload's own nick; only act if THIS window asked for it.
        let nick = whois.get("nick").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let folded = lurker_proto::fold(&nick);
        let Some(key) = self.pending_whois.borrow_mut().remove(&folded) else { return };
        let lines = crate::whois::format(whois);
        self.app.store.borrow_mut().inject_lines(&key, "whois", lines);
        // Redraw if the target buffer is the one on screen.
        if self.active.borrow().as_ref() == Some(&key) {
            *self.drawn.borrow_mut() = Drawn::default();
            self.render_active();
        }
    }

    /// Execute one nicklist-menu command against the menu's nick.
    fn run_nick_command(self: &Rc<Self>, id: &str) {
        let nick = self.menu_nick.borrow().clone();
        if nick.is_empty() {
            return;
        }
        let Some(key) = self.active.borrow().clone() else { return };
        let Some(network_id) = key.network_id else { return };

        // Call moderation isn't an IRC verb — it's a REST action against the
        // SFU (Lurker #680), so it is handled here rather than in nickmenu's
        // pure verb table. The menu only offers these when a call is live and
        // the clicker is halfop+; the server re-checks regardless.
        if id == "call-mute" || id == "call-remove" {
            let action = if id == "call-mute" { "mute" } else { "remove" };
            let status = self.status_label.clone();
            let who = nick.clone();
            self.app.moderate_call(&key, nick, action, move |res| match res {
                Ok(()) => status.set_text(&format!("{action}: {who}")),
                Err(e) => status.set_text(&format!("call moderation failed — {e}")),
            });
            return;
        }

        // Friends are account state, not an IRC command — handled here rather
        // than in nickmenu's pure verb table.
        if id == "friend" {
            let (model, faved) = {
                let store = self.app.store.borrow();
                let key = BufferKey::new(Some(network_id), &nick);
                (store.favorites_model(), store.is_favorite(&key))
            };
            if model {
                let key = BufferKey::new(Some(network_id), &nick);
                if faved {
                    self.app.send(ClientVerb::UnfavoriteBuffer {
                        network_id,
                        target: self.app.wire_target(&key),
                    });
                } else {
                    // The member may have no DM yet, and the server refuses to
                    // favorite a closed/absent buffer. open-buffer mints the
                    // row, and the same socket delivers it before the
                    // favorite, so the favorite always lands (the web's exact
                    // recipe).
                    self.app.open_buffer(&key);
                    self.app.send(ClientVerb::FavoriteBuffer {
                        network_id,
                        target: self.app.wire_target(&key),
                    });
                }
            } else {
                crate::frienddialog::FriendDialog::add_for_nick(&self.app, network_id, &nick);
            }
            return;
        }

        let Some(cmd) = crate::nickmenu::Cmd::from_id(id) else { return };

        // Query changes window state, so it lives here rather than in the
        // pure module: open-buffer (explicit intent — the reserved case, §4.3)
        // then focus the DM.
        if cmd == crate::nickmenu::Cmd::Query {
            let dm = BufferKey::new(Some(network_id), &nick);
            self.app.open_buffer(&dm);
            self.activate(&dm);
            return;
        }

        let channel = self.app.wire_target(&key);
        let host = self
            .app
            .store
            .borrow()
            .buffer(&key)
            .and_then(|b| b.members.get(&lurker_proto::fold(&nick)))
            .and_then(|m| m.host.clone());

        for verb in crate::nickmenu::verbs_for(cmd, network_id, &channel, &nick, host.as_deref())
        {
            self.app.send(verb);
        }
        if matches!(cmd, crate::nickmenu::Cmd::Whois) {
            self.note_whois(&nick);
        }
    }

    /// Inline media below a message, honouring the per-device toggles.
    ///
    /// Images render from the texture cache (fetched once, redraws free);
    /// video and audio are GTK media widgets streaming the URL on demand —
    /// nothing downloads until the user hits play.
    fn embed_media(&self, key: &BufferKey, text: &str) {
        let device = self.app.device.borrow().clone();
        for (url, kind) in crate::media::media_urls(text) {
            let widget: Option<gtk::Widget> = match kind {
                crate::media::MediaKind::Image if device.inline_images => {
                    match self.app.images.get(&url) {
                        Some(texture) => {
                            let picture = gtk::Picture::for_paintable(&texture);
                            picture.set_can_shrink(true);
                            // Cap the inline footprint, keep aspect.
                            let (w, h) =
                                (texture.width() as f64, texture.height() as f64);
                            let scale = (420.0 / w).min(280.0 / h).min(1.0);
                            picture.set_size_request(
                                (w * scale) as i32,
                                (h * scale) as i32,
                            );
                            picture.add_css_class("embed");
                            picture.set_cursor_from_name(Some("zoom-in"));
                            picture.set_tooltip_text(Some("Click to view full size"));
                            // Click opens the full-resolution viewer.
                            let click = gtk::GestureClick::builder().button(1).build();
                            let tex = texture.clone();
                            let title = url.clone();
                            let weak = self.clone_handle();
                            click.connect_released(move |_, _, _, _| {
                                if let Some(this) = weak.upgrade() {
                                    this.view_image(&tex, &title);
                                }
                            });
                            picture.add_controller(click);
                            Some(picture.upcast())
                        }
                        None => {
                            // No texture yet: say why. Silence here is what
                            // made a slow host (11s is normal for some image
                            // hosts) indistinguishable from a broken link.
                            let failed = self.app.images.is_failed(&url);
                            if !failed {
                                self.app.fetch_image(url.clone(), key.clone());
                            }
                            let note = gtk::Label::builder()
                                .xalign(0.0)
                                .label(if failed {
                                    "\u{26A0} image could not be loaded — click to retry"
                                } else {
                                    "\u{2026} loading image"
                                })
                                .css_classes(["embed-note"])
                                .build();
                            if failed {
                                // Retry on click: a failure is usually the host
                                // being slow or briefly unreachable, and being
                                // stuck with it until restart is worse.
                                note.set_cursor_from_name(Some("pointer"));
                                let click = gtk::GestureClick::builder().button(1).build();
                                let weak = self.clone_handle();
                                let retry_url = url.clone();
                                let wake = key.clone();
                                click.connect_released(move |_, _, _, _| {
                                    let Some(this) = weak.upgrade() else { return };
                                    this.app.images.clear_failure(&retry_url);
                                    this.app.fetch_image(retry_url.clone(), wake.clone());
                                    *this.drawn.borrow_mut() = Drawn::default();
                                    this.render_active();
                                });
                                note.add_controller(click);
                            }
                            Some(note.upcast())
                        }
                    }
                }
                // Video and audio are CLICK-TO-PLAY, never auto-instantiated.
                // Building a gtk::MediaFile during render spins up a native
                // media pipeline (gstreamer) for every A/V link in the
                // scrollback — and on a machine where that backend is broken
                // or absent (the Windows bundle has no gstreamer at all), the
                // pipeline dies in NATIVE code: no panic, no backtrace, the
                // log just stops. Field-reported as an unopenable channel —
                // one .mp4 in the backlog crashed the app on every visit.
                // A poster button turns that poisoned-buffer crash into, at
                // very worst, a crash on a deliberate press of play.
                crate::media::MediaKind::Video if device.inline_videos => {
                    Some(Self::deferred_player(&url, false))
                }
                crate::media::MediaKind::Audio if device.inline_audio => {
                    Some(Self::deferred_player(&url, true))
                }
                _ => None,
            };
            if let Some(widget) = widget {
                let mut end = self.text.end_iter();
                self.text.insert(&mut end, "\n");
                // Indent the embed to the text column — which now follows the
                // timestamp format, not the old fixed nick column.
                self.insert_with_tags(&" ".repeat(self.text_column()), &["time"]);
                let mut end = self.text.end_iter();
                let anchor = self.text.create_child_anchor(&mut end);
                self.text_view.add_child_at_anchor(&widget, &anchor);
                self.embeds.borrow_mut().push(widget);
            }
        }
    }

    /// Render a compact YouTube link preview (title + description) below a
    /// message, when previews are enabled. Text-only — no child widgets — so
    /// it stays clear of the image child-anchor teardown path.
    fn embed_previews(&self, key: &BufferKey, text: &str) {
        if !self.app.device.borrow().link_previews {
            return;
        }
        for (url, _kind_ignored) in preview_urls(text) {
            // Clone the cache entry and DROP the borrow before doing anything
            // that might re-borrow `previews` (fetch_preview borrows it mut).
            let cached = self.app.previews.borrow().get(&url).cloned();
            match cached.map(|(_, p)| p) {
                Some(Some(preview)) => {
                    // One CARD, not floating text lines: bordered, accent-
                    // edged, thumbnail beside title — visually separate from
                    // the conversation (field report: the text-line version
                    // was "kinda hard to parse") and clickable as one object.
                    let card = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(10)
                        .css_classes(["preview-card"])
                        .build();

                    // Thumbnail (og:image), through the same cache and fetch
                    // path inline images use. A card without its picture is
                    // still a card; when the fetch lands, the rerender adds it.
                    if !preview.image.is_empty() {
                        match self.app.images.get(&preview.image) {
                            Some(texture) => {
                                let picture = gtk::Picture::for_paintable(&texture);
                                picture.set_can_shrink(true);
                                let (w, h) =
                                    (texture.width() as f64, texture.height() as f64);
                                let scale = (96.0 / w).min(72.0 / h).min(1.0);
                                picture.set_size_request(
                                    (w * scale) as i32,
                                    (h * scale) as i32,
                                );
                                picture.set_valign(gtk::Align::Center);
                                card.append(&picture);
                            }
                            None if !self.app.images.is_failed(&preview.image) => {
                                self.app.fetch_image(preview.image.clone(), key.clone());
                            }
                            None => {}
                        }
                    }

                    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
                    col.set_valign(gtk::Align::Center);
                    col.set_hexpand(true);
                    // Collapse the metadata's own whitespace runs: GitHub
                    // (among others) puts raw newlines in og:description, and
                    // explicit breaks defeat the two-line ellipsize clamp —
                    // the card in the field report was ten lines tall.
                    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
                    let title = flat(&preview.title);
                    let description = flat(&preview.description);
                    if !title.is_empty() {
                        col.append(
                            &gtk::Label::builder()
                                .xalign(0.0)
                                .label(&title)
                                .wrap(true)
                                .wrap_mode(gtk::pango::WrapMode::WordChar)
                                .lines(2)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                // width_chars is the MINIMUM. Without it a
                                // wrapping label's minimum is ~one char, and
                                // a TextView anchor allocates minimum — the
                                // card crushed into a one-letter-wide column
                                // (field screenshot: "R- e… S- u…").
                                .width_chars(36)
                                .hexpand(true)
                                .css_classes(["preview-card-title"])
                                .build(),
                        );
                    }
                    if !description.is_empty() {
                        let desc: String = description.chars().take(220).collect();
                        col.append(
                            &gtk::Label::builder()
                                .xalign(0.0)
                                .label(desc)
                                .wrap(true)
                                .wrap_mode(gtk::pango::WrapMode::WordChar)
                                .lines(2)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .width_chars(36)
                                .hexpand(true)
                                .css_classes(["preview-card-desc"])
                                .build(),
                        );
                    }
                    card.append(&col);

                    // The whole card opens the link — it IS the link,
                    // restated legibly.
                    card.set_cursor_from_name(Some("pointer"));
                    card.set_tooltip_text(Some(&url));
                    let click = gtk::GestureClick::builder().button(1).build();
                    let weak = self.clone_handle();
                    let target = url.clone();
                    click.connect_released(move |_, _, _, _| {
                        if let Some(this) = weak.upgrade() {
                            this.open_url(&target);
                        }
                    });
                    card.add_controller(click);

                    // Full-width (field request, superseding centred): the
                    // card spans the buffer so it reads as a section of the
                    // UI rather than an island breaking it up, and long
                    // titles get the room. A TextView anchor grants only
                    // natural size, so the span is an explicit width request
                    // from the view's current allocation; renders are
                    // frequent enough that resize staleness self-heals.
                    let view_w = self.text_view.width();
                    if view_w > 60 {
                        card.set_size_request(view_w - 24, -1);
                    }
                    let mut end = self.text.end_iter();
                    self.text.insert(&mut end, "\n");
                    let mut end = self.text.end_iter();
                    let anchor = self.text.create_child_anchor(&mut end);
                    self.text_view.add_child_at_anchor(&card, &anchor);
                    self.embeds.borrow_mut().push(card.upcast());
                }
                Some(None) => {} // fetched, nothing usable
                None => self.app.fetch_preview(url.clone(), key.clone()),
            }
        }
    }

    /// Upload pasted or dropped content and paste the resulting link at the
    /// cursor. This is the Lurker upload pipeline (§10) — the server
    /// optimises, hosts, and records it in the account's upload history.
    /// Choose a file and upload it, inserting the resulting link.
    ///
    /// The same destination as paste and drag-and-drop; only the way you name
    /// the file differs. On a phone this is the ONLY way in — there is nothing
    /// to drag from, and the clipboard only carries what you already copied.
    fn pick_and_upload(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title("Attach a file").modal(true).build();
        let this = self.clone();
        dialog.open(Some(&self.window), gtk::gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return }; // cancelled
            let name = file
                .basename()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "upload".to_string());
            // Check the size BEFORE reading. The whole file goes into memory
            // to be uploaded, so picking a large video on a phone can exhaust
            // it and get the process killed — which reads to the user as the
            // app crashing on upload, with nothing to explain it.
            let path = match file.path() {
                Some(p) => p,
                None => {
                    this.status_label.set_text("could not read that file");
                    return;
                }
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let cap = this.upload_cap();
            if size > cap {
                this.status_label.set_text(&format!(
                    "{name} is {} — this server accepts up to {}",
                    human_bytes(size),
                    human_bytes(cap)
                ));
                return;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    this.status_label.set_text(&format!("could not read that file: {e}"));
                    return;
                }
            };
            // Content type from the name, since the portal does not always
            // give one. The server validates the real type regardless.
            let mime = gtk::gio::content_type_guess(Some(&name), Some(bytes.as_slice()))
                .0
                .to_string();
            this.upload_and_insert(name, mime, bytes);
        });
    }

    /// The effective upload ceiling: the smaller of what the server reports
    /// and the user's own `uploads.image.max_upload_mb` — the account-wide
    /// self-imposed cap the website honours, so a limit set there binds here.
    fn upload_cap(&self) -> u64 {
        let server = self.app.store.borrow().max_upload_bytes.unwrap_or(MAX_UPLOAD_FALLBACK);
        let user_mb = self.app.setting("uploads.image.max_upload_mb").as_u64().unwrap_or(0);
        if user_mb > 0 { server.min(user_mb.saturating_mul(1024 * 1024)) } else { server }
    }

    /// Re-encode a static image per the `uploads.image.*` settings, exactly
    /// as the website does before uploading: scale the longest edge down to
    /// max_dimension and re-encode at the chosen quality. Returns the
    /// replacement (filename, mime, bytes), or `None` to upload the original
    /// (animated formats bypass verbatim; a failed decode falls through
    /// rather than blocking the upload).
    ///
    /// One honest divergence: the web's default format is webp, but
    /// gdk-pixbuf has no webp encoder on most systems. When webp is asked for
    /// and unavailable, images that need work are saved as png — lossless and
    /// alpha-preserving, which honours the *reason* the web default is webp
    /// (transparency survives) at the cost of size.
    fn recompress_image(
        &self,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Option<(String, String, Vec<u8>)> {
        if !mime.starts_with("image/") || mime == "image/gif" {
            return None; // animated or not an image: verbatim, like the web
        }
        let format = self
            .app
            .setting("uploads.image.format")
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| "webp".to_string());
        let max_dim = self.app.setting("uploads.image.max_dimension").as_i64().unwrap_or(2048);
        let quality =
            self.app.setting("uploads.image.quality").as_i64().unwrap_or(85).clamp(30, 100);

        let loader = gtk::gdk_pixbuf::PixbufLoader::new();
        loader.write(bytes).ok()?;
        loader.close().ok()?;
        let pixbuf = loader.pixbuf()?;

        let (w, h) = (pixbuf.width(), pixbuf.height());
        let longest = w.max(h) as i64;
        let scaled = if max_dim > 0 && longest > max_dim {
            let f = max_dim as f64 / longest as f64;
            pixbuf.scale_simple(
                ((w as f64 * f).round() as i32).max(1),
                ((h as f64 * f).round() as i32).max(1),
                gtk::gdk_pixbuf::InterpType::Bilinear,
            )?
        } else if format == "jpeg" && mime != "image/jpeg" {
            pixbuf // format conversion still wanted at original size
        } else if pixbuf_has_writer("webp") && format == "webp" && mime != "image/webp" {
            pixbuf
        } else {
            return None; // nothing to do
        };

        let stem = filename.rsplit_once('.').map(|(s, _)| s).unwrap_or(filename);
        let try_save = |kind: &str, opts: &[(&str, &str)]| -> Option<Vec<u8>> {
            scaled.save_to_bufferv(kind, opts).ok().map(|b| b.to_vec())
        };
        let q = quality.to_string();
        let (out, ext, out_mime) = if format == "jpeg" {
            (try_save("jpeg", &[("quality", &q)])?, "jpg", "image/jpeg")
        } else if pixbuf_has_writer("webp") {
            (try_save("webp", &[("quality", &q)])?, "webp", "image/webp")
        } else {
            (try_save("png", &[])?, "png", "image/png")
        };
        // Never "optimize" a file into a bigger one unless we also shrank it.
        if out.len() >= bytes.len() && (w.max(h) as i64) <= max_dim.max(0) {
            return None;
        }
        Some((format!("{stem}.{ext}"), out_mime.to_string(), out))
    }

    fn upload_and_insert(self: &Rc<Self>, filename: String, mime: String, bytes: Vec<u8>) {
        let (filename, mime, bytes) = match self.recompress_image(&filename, &mime, &bytes) {
            Some(replacement) => replacement,
            None => (filename, mime, bytes),
        };
        // Belt for the paths that reach here without the pre-read metadata
        // check (clipboard pastes hand us bytes directly).
        let cap = self.upload_cap();
        if bytes.len() as u64 > cap {
            self.status_label.set_text(&format!(
                "{filename} is {} — the limit is {}",
                human_bytes(bytes.len() as u64),
                human_bytes(cap)
            ));
            return;
        }
        // Upload indicator (#9): the attach button becomes a busy marker for
        // the duration — visible right where the action started, unlike the
        // status line a phone user may never look at.
        self.btn_attach.set_sensitive(false);
        self.btn_attach.set_icon_name("content-loading-symbolic");
        self.status_label
            .set_text(&format!("uploading {filename} ({} KB)…", bytes.len() / 1024));
        let this = self.clone();
        self.app.upload(filename, mime, bytes, move |result| {
            // ONE reset path, before the arms split: no outcome — and no
            // future early return inside an arm — can strand the disabled
            // spinner button.
            this.btn_attach.set_sensitive(true);
            this.btn_attach.set_icon_name("mail-attachment-symbolic");
            match result {
                Ok(url) => {
                    // Insert at the cursor, space-padded so it can't fuse
                    // with adjacent words.
                    let mut pos = this.entry.position();
                    let text = this.entry.text();
                    let pad_before = pos > 0
                        && !text
                            .chars()
                            .nth(pos as usize - 1)
                            .is_none_or(char::is_whitespace);
                    let insert = format!("{}{url} ", if pad_before { " " } else { "" });
                    this.entry.insert_text(&insert, &mut pos);
                    this.entry.set_position(pos);
                    // NOT grab_focus(): on a GtkEntry that selects the whole
                    // contents, so the freshly inserted link sat marked and
                    // the next keystroke replaced it (#9).
                    this.entry.grab_focus_without_selecting();
                    this.update_status();
                }
                Err(e) => this.status_label.set_text(&format!("upload failed: {e}")),
            }
        });
    }

    /// Handle a paste that contains files or an image rather than text.
    /// Returns false when the clipboard holds neither (normal paste proceeds).
    fn paste_media(self: &Rc<Self>) -> bool {
        // `uploads.paste.enabled`: pasting media uploads it. Off means a
        // paste is just a paste, everywhere — same switch as the website.
        if !self.app.setting("uploads.paste.enabled").as_bool().unwrap_or(true) {
            return false;
        }
        let clipboard = self.entry.clipboard();
        let formats = clipboard.formats();

        if formats.contains_type(gtk::gdk::FileList::static_type()) {
            let this = self.clone();
            clipboard.read_value_async(
                gtk::gdk::FileList::static_type(),
                glib::Priority::DEFAULT,
                gtk::gio::Cancellable::NONE,
                move |result| {
                    let Ok(value) = result else { return };
                    let Ok(files) = value.get::<gtk::gdk::FileList>() else { return };
                    for file in files.files() {
                        let Some(path) = file.path() else { continue };
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                let name = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "file".into());
                                let mime = mime_for(&name);
                                this.upload_and_insert(name, mime, bytes);
                            }
                            Err(e) => this
                                .status_label
                                .set_text(&format!("could not read {}: {e}", path.display())),
                        }
                    }
                },
            );
            return true;
        }

        if formats.contains_type(gtk::gdk::Texture::static_type()) {
            let this = self.clone();
            clipboard.read_texture_async(gtk::gio::Cancellable::NONE, move |result| {
                let Ok(Some(texture)) = result else { return };
                let bytes = texture.save_to_png_bytes();
                this.upload_and_insert(
                    "clipboard.png".into(),
                    "image/png".into(),
                    bytes.to_vec(),
                );
            });
            return true;
        }
        false
    }

    // ── Nicklist ──────────────────────────────────────────────────────────

    fn rebuild_member_list(&self) {
        clear_list(&self.member_list);
        let Some(key) = self.active.borrow().clone() else {
            self.member_count.set_text("");
            return;
        };
        let store = self.app.store.borrow();
        let Some(buf) = store.buffer(&key) else { return };

        let mut members: Vec<_> = buf.members.values().collect();
        // Ops first, then by nick — the conventional IRC nicklist order. The
        // rank comes from nickmenu::Rank (the one table shared with menu
        // gating), not a private copy that could drift; cached_key because
        // the key allocates and sort_by_key would recompute it per
        // comparison — ~11x the work during a netsplit-rejoin churn.
        members.sort_by_cached_key(|m| {
            (
                std::cmp::Reverse(crate::nickmenu::Rank::from_mode(m.highest_mode())),
                m.nick.to_ascii_lowercase(),
            )
        });
        self.member_count.set_text(&format!("{} members", members.len()));

        *self.member_nicks.borrow_mut() = members.iter().map(|m| m.nick.clone()).collect();

        // The nicklist wears the same colours as the buffer: the colour is a
        // pure function of the folded nick and palette size, so computing it
        // here with the same hash keeps the two views in agreement without any
        // shared state. Your own nick takes the self colour, as it does in the
        // scrollback.
        let palette = crate::theme::nick_palette(&self.app);
        let self_colour =
            crate::theme::self_color(&self.app).unwrap_or_else(|| "#9aa0b0".to_string());
        let my_nick = key
            .network_id
            .and_then(|id| store.networks.get(&id))
            .and_then(|n| n.nick.as_deref().map(lurker_proto::fold));

        for m in members {
            let sigil = m.sigil().map(|c| c.to_string()).unwrap_or_default();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            label.add_css_class("member");
            if m.away {
                // Away wins over the palette: the grey IS the away signal, and
                // a coloured-but-away nick would read as present.
                label.add_css_class("away");
                label.set_text(&format!("{sigil}{}", m.nick));
            } else {
                let colour = if my_nick.as_deref() == Some(lurker_proto::fold(&m.nick).as_str()) {
                    self_colour.clone()
                } else {
                    let idx = crate::format::nick_color_index(
                        &m.nick,
                        palette.len(),
                        &self.render_opts.borrow().stop_chars,
                    );
                    palette.get(idx).cloned().unwrap_or_else(|| "#939293".to_string())
                };
                label.set_markup(&format!(
                    "<span foreground=\"{colour}\">{}</span>",
                    glib::markup_escape_text(&format!("{sigil}{}", m.nick)),
                ));
            }
            let row = gtk::ListBoxRow::builder().child(&label).selectable(false).build();
            self.member_list.append(&row);
        }
    }

    // ── Status line ───────────────────────────────────────────────────────

    fn update_status(&self) {
        let conn = self.app.conn.borrow().to_string();
        let store = self.app.store.borrow();
        let key = self.active.borrow().clone();

        let nick = key
            .as_ref()
            .and_then(|k| k.network_id)
            .and_then(|id| store.networks.get(&id))
            .and_then(|n| n.nick.clone())
            .unwrap_or_else(|| "—".into());

        let where_ = key
            .as_ref()
            .map(|k| {
                let net = k
                    .network_id
                    .and_then(|id| store.networks.get(&id))
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                format!("{net}/{}", store.wire_target(k))
            })
            .unwrap_or_default();

        let typing = key
            .as_ref()
            .and_then(|k| store.buffer(k))
            .map(|b| b.typing_nicks())
            .filter(|nicks| !nicks.is_empty())
            .map(|nicks| match nicks.len() {
                1 => format!("  │  {} is typing…", nicks[0]),
                2 => format!("  │  {} and {} are typing…", nicks[0], nicks[1]),
                n => format!("  │  {n} people are typing…"),
            })
            .unwrap_or_default();

        let detached = key
            .as_ref()
            .and_then(|k| store.buffer(k))
            .is_some_and(|b| b.detached);
        let jumped = if detached {
            "  │  viewing older history — Esc returns to the present"
        } else {
            ""
        };

        let paused = if store.paused { "  [account paused — read only]" } else { "" };
        let highlights = store.total_highlights();
        let badge = if highlights > 0 { format!("  ✱{highlights}") } else { String::new() };

        // `look.bar.*`: the lag readout appears at min_show, turns alarming at
        // the alarm threshold, and can be pinned on permanently.
        let lag = key
            .as_ref()
            .and_then(|k| k.network_id)
            .and_then(|id| store.networks.get(&id))
            .and_then(|n| n.lag_ms);
        let lag_part = match lag {
            Some(ms) if ms >= self.lag_min_show_ms.get().max(0) || self.lag_always_show.get() => {
                let level = if ms >= self.lag_alarm_ms.get().max(0) { " (!)" } else { "" };
                format!("  │  lag {ms}ms{level}")
            }
            _ => String::new(),
        };

        // `look.bar.time_format`: the status-bar clock; empty hides it.
        let clock = {
            let fmt = self.bar_time_fmt.borrow();
            if fmt.is_empty() {
                String::new()
            } else {
                glib::DateTime::now_local()
                    .ok()
                    .and_then(|d| d.format(&fmt).ok())
                    .map(|t| format!("  │  {t}"))
                    .unwrap_or_default()
            }
        };

        self.status_label
            .set_text(&format!(
                "{nick}  │  {where_}  │  {conn}{badge}{lag_part}{typing}{jumped}{paused}{clock}"
            ));
    }

    /// Persist the composer's current text as the active buffer's draft.
    ///
    /// Empty text clears the draft — otherwise a deleted draft resurrects on
    /// every buffer switch.
    fn save_draft(&self) {
        let Some(key) = self.active.borrow().clone() else { return };
        let text = self.entry.text().to_string();
        let stored = self
            .app
            .store
            .borrow()
            .buffer(&key)
            .and_then(|b| b.draft.clone())
            .unwrap_or_default();
        if text == stored {
            return;
        }
        {
            let mut store = self.app.store.borrow_mut();
            if let Some(buf) = store.buffers.get_mut(&key) {
                buf.draft = if text.is_empty() { None } else { Some(text.clone()) };
            }
        }
        if text.is_empty() {
            self.app.send(ClientVerb::DraftClear {
                network_id: key.network_id,
                target: self.app.wire_target(&key),
            });
        } else {
            self.app.send(ClientVerb::DraftSet {
                network_id: key.network_id,
                target: self.app.wire_target(&key),
                body: text,
            });
        }
    }

    /// Send a `typing` TAGMSG, throttled to one every 3 s while composing.
    ///
    /// Gated on `chat.send_typing_notifications` — the first behaviour setting
    /// a privacy-minded user looks for.
    fn signal_typing(&self, state: &str) {
        if !self.app.setting("chat.send_typing_notifications").as_bool().unwrap_or(true) {
            return;
        }
        let Some(key) = self.active.borrow().clone() else { return };
        let Some(network_id) = key.network_id else { return };
        if key.is_server_log() || key.is_system() {
            return;
        }
        if state == "active" {
            let now = std::time::Instant::now();
            if let Some(last) = self.typing_sent.get() {
                if now.duration_since(last) < std::time::Duration::from_secs(3) {
                    return;
                }
            }
            self.typing_sent.set(Some(now));
        } else {
            self.typing_sent.set(None);
        }
        self.app.send(ClientVerb::Typing {
            network_id,
            target: self.app.wire_target(&key),
            state: state.to_string(),
        });
    }

    /// Recall a line from the buffer's input history (Up/Down in the entry).
    fn recall(&self, direction: i32) {
        let Some(key) = self.active.borrow().clone() else { return };
        let store = self.app.store.borrow();
        let Some(buf) = store.buffer(&key) else { return };
        let len = buf.input_history.len();
        let pos = self.history_pos.get();
        let next = crate::input::recall_step(pos, len, direction);
        if pos.is_none() && next.is_some() {
            *self.history_stash.borrow_mut() = self.entry.text().to_string();
        }
        let line = next.and_then(|i| buf.input_history.get(i).cloned());
        drop(store);
        self.history_pos.set(next);
        match line {
            Some(text) => self.entry.set_text(&text),
            None => self.entry.set_text(&self.history_stash.borrow()),
        }
        self.entry.set_position(-1);
    }

    /// Tab completion over recent speakers then the nicklist
    /// (`crate::input::candidates` — §9.3: self-echoes are skipped when
    /// ranking speakers). Repeated Tab cycles in place.
    /// Tab completion. `direction` is +1 for Tab and -1 for Shift+Tab, so a
    /// cycle overshot by one keypress costs one keypress to undo rather than a
    /// full lap through the candidates.
    ///
    /// What gets completed depends on where the cursor is (see input::classify):
    /// a slash command in the first column, a channel by its sigil, otherwise a
    /// nick.
    fn complete_nick(&self, direction: i32) {
        let text = self.entry.text().to_string();
        let cursor = self.entry.position().max(0) as usize;

        // Continue an existing cycle. The stored kind matters: cycling must keep
        // completing the same category the cycle started in.
        let cycling = self.completion.borrow_mut().as_mut().map(|(s, c, i)| {
            let len = c.len();
            *i = if direction < 0 { (*i + len - 1) % len } else { (*i + 1) % len };
            (*s, c[*i].clone())
        });
        if let Some((anchor, value)) = cycling {
            let kind = crate::input::classify(&text, cursor).2;
            let (new_text, new_cursor) =
                crate::input::complete(&text, cursor, anchor as usize, &value, kind);
            self.entry.set_text(&new_text);
            self.entry.set_position(new_cursor as i32);
            return;
        }

        let Some((anchor, kind, candidates)) = self.completion_candidates(&text, cursor)
        else {
            return;
        };

        if candidates.is_empty() {
            return;
        }
        // Shift+Tab with no cycle in progress starts from the last candidate.
        let first = if direction < 0 { candidates.len() - 1 } else { 0 };
        let nick = candidates[first].clone();
        *self.completion.borrow_mut() = Some((anchor as i32, candidates, first));
        let (new_text, new_cursor) = crate::input::complete(&text, cursor, anchor, &nick, kind);
        self.entry.set_text(&new_text);
        self.entry.set_position(new_cursor as i32);
    }

    /// The completion candidates at a cursor position — the source both Tab
    /// and the suggestion strip draw from, so the two can never disagree.
    fn completion_candidates(
        &self,
        text: &str,
        cursor: usize,
    ) -> Option<(usize, crate::input::Token, Vec<String>)> {
        let (anchor, prefix, kind) = crate::input::classify(text, cursor);
        // A bare "/" is a legitimate "what can I type?" prompt, so commands may
        // complete from an empty prefix; nicks and channels may not — Tab on
        // nothing would dump the whole nicklist into the composer.
        if prefix.is_empty() && kind != crate::input::Token::Command {
            return None;
        }
        let key = self.active.borrow().clone()?;

        let candidates = match kind {
            crate::input::Token::Command => {
                crate::input::command_candidates(&prefix, crate::commands::KNOWN)
            }
            crate::input::Token::Channel => {
                // Channels you already have open on this network, so completion
                // reflects where you actually are.
                let store = self.app.store.borrow();
                let mut names: Vec<String> = store
                    .buffers
                    .iter()
                    .filter(|(k, _)| k.network_id == key.network_id && k.is_channel())
                    .map(|(_, b)| b.display_name.clone())
                    .filter(|n| n.to_ascii_lowercase().starts_with(&prefix))
                    .collect();
                names.sort();
                names.dedup();
                names
            }
            crate::input::Token::Nick => {
                let store = self.app.store.borrow();
                let buf = store.buffer(&key)?;
                let own = key
                    .network_id
                    .and_then(|id| store.networks.get(&id))
                    .and_then(|n| n.nick.clone());
                let recent = buf
                    .events
                    .iter()
                    .rev()
                    .filter(|e| !e.is_self && e.event_type.is_chat())
                    .filter_map(|e| e.nick.as_deref());
                let members = buf.members.values().map(|m| m.nick.as_str());
                crate::input::candidates(&prefix, recent, members, own.as_deref())
            }
        };
        Some((anchor, kind, candidates))
    }

    /// Apply a colour pick from the palette: merge into any colour code
    /// already at the cursor, replacing the merged span.
    /// Deliberately does NOT focus the entry: picks come from the palette
    /// popover, and stealing focus out of an autohide popover dismisses it —
    /// which would kill the left-then-right fg/bg flow on the first click.
    /// The popover's closed handler refocuses the entry instead.
    fn pick_color(&self, index: u8, background: bool) {
        let text = self.entry.text().to_string();
        let cursor = self.entry.position().max(0) as usize;
        let before: String = text.chars().take(cursor).collect();
        let (span, code) = crate::input::merge_color_code(&before, index, background);
        let start = (cursor - span) as i32;
        if span > 0 {
            self.entry.delete_text(start, cursor as i32);
        }
        let mut pos = start;
        self.entry.insert_text(&code, &mut pos);
        self.entry.set_position(pos);
    }

    /// Insert an IRC formatting code at the cursor (composer keyboard
    /// shortcuts and the palette popover both land here).
    fn insert_format(&self, code: &str) {
        let mut pos = self.entry.position();
        self.entry.insert_text(code, &mut pos);
        self.entry.set_position(pos);
        self.entry.grab_focus_without_selecting();
    }

    /// The mIRC palette popover (`input.show_format_button`): the 16 colour
    /// slots, the style toggles, and clear-formatting — the same surface as
    /// the web's picker, inserting the same control codes.
    fn open_format_popover(self: &Rc<Self>) {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_top(8);
        root.set_margin_bottom(8);
        root.set_margin_start(8);
        root.set_margin_end(8);

        let styles = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        for (label, code, tip) in [
            ("B", "\u{02}", "Bold (Ctrl+B)"),
            ("I", "\u{1D}", "Italic (Ctrl+I)"),
            ("U", "\u{1F}", "Underline (Ctrl+U)"),
            ("S", "\u{1E}", "Strikethrough"),
            ("✕", "\u{0F}", "Clear formatting from here"),
        ] {
            let b = gtk::Button::builder()
                .label(label)
                .tooltip_text(tip)
                .css_classes(["toolbtn"])
                .build();
            let this = self.clone();
            let code = code.to_string();
            // Insert without focusing — see pick_color: focus-out dismisses
            // the autohide popover mid-flow.
            b.connect_clicked(move |_| {
                let mut pos = this.entry.position();
                this.entry.insert_text(&code, &mut pos);
                this.entry.set_position(pos);
            });
            styles.append(&b);
        }
        root.append(&styles);

        // Colour swatches: user palette slots where set, defaults otherwise.
        // A swatch inserts \x03NN — two digits always, so typing a digit
        // right after can't mutate the colour.
        let grid = gtk::Grid::builder().row_spacing(4).column_spacing(4).build();
        let user = self.mirc_palette.borrow().clone();
        for i in 0..16u8 {
            let colour = user
                .get(i as usize)
                .cloned()
                .flatten()
                .or_else(|| mirc::color_hex(i).map(str::to_string))
                .unwrap_or_else(|| "#888888".to_string());
            let swatch = gtk::Button::builder()
                .tooltip_text(format!("Colour {i} — left: text, right: background"))
                .css_classes(["mirc-swatch"])
                .build();
            swatch.set_child(Some(
                &gtk::Label::builder()
                    .use_markup(true)
                    .label(format!("<span foreground=\"{colour}\">⬤</span>"))
                    .build(),
            ));
            // Left picks the foreground, right the background; picking both
            // merges into ONE \x03fg,bg code at the cursor (see
            // input::merge_color_code) instead of stacking codes that fight.
            let this = self.clone();
            swatch.connect_clicked(move |_| this.pick_color(i, false));
            let right = gtk::GestureClick::builder().button(3).build();
            let this = self.clone();
            right.connect_pressed(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                this.pick_color(i, true);
            });
            swatch.add_controller(right);
            grid.attach(&swatch, (i % 8) as i32, (i / 8) as i32, 1, 1);
        }
        root.append(&grid);

        let popover = gtk::Popover::builder().child(&root).build();
        popover.set_parent(&self.btn_format);
        let weak = self.clone_handle();
        popover.connect_closed(move |p| {
            p.unparent();
            // Hand focus back so typing continues right after the picks.
            if let Some(this) = weak.upgrade() {
                this.entry.grab_focus_without_selecting();
            }
        });
        popover.popup();
    }

    /// Rebuild the suggestion strip for the current composer state. Chips are
    /// the same candidates Tab cycles through; tapping one applies it exactly
    /// as Tab would.
    fn refresh_suggestion_strip(self: &Rc<Self>) {
        // Cached in apply_display_settings: this runs per keystroke, and the
        // settings fallback path is a linear scan of the whole registry.
        let on = self.narrow.get() || self.strip_on_desktop.get();
        if !on {
            self.strip_scroll.set_visible(false);
            return;
        }
        let text = self.entry.text().to_string();
        let cursor = self.entry.position().max(0) as usize;
        let found = self.completion_candidates(&text, cursor);
        let candidates = match found {
            Some((_, _, c)) if !c.is_empty() => c,
            _ => {
                self.strip_scroll.set_visible(false);
                return;
            }
        };

        // A fixed pool of chips, built once: this runs per keystroke, and
        // destroying + recreating 12 buttons (CSS nodes, signal closures,
        // relayout) each press is the expensive part of the strip. Chips are
        // relabelled and shown/hidden; each reads its value by index and
        // re-derives against the LIVE entry, exactly as Tab would — never a
        // snapshot (buffer switches with identical text emit no changed
        // signal to rebuild us).
        const POOL: usize = 12;
        if self.suggestion_strip.first_child().is_none() {
            for i in 0..POOL {
                let chip = gtk::Button::builder().css_classes(["suggestion-chip"]).build();
                let this = self.clone_handle();
                chip.connect_clicked(move |_| {
                    let Some(this) = this.upgrade() else { return };
                    let Some(value) = this.chip_values.borrow().get(i).cloned() else {
                        return;
                    };
                    let text = this.entry.text().to_string();
                    let cursor = this.entry.position().max(0) as usize;
                    let Some((anchor, kind, _)) = this.completion_candidates(&text, cursor)
                    else {
                        return;
                    };
                    let (new_text, new_cursor) =
                        crate::input::complete(&text, cursor, anchor, &value, kind);
                    this.entry.set_text(&new_text);
                    this.entry.set_position(new_cursor as i32);
                    this.entry.grab_focus_without_selecting();
                });
                self.suggestion_strip.append(&chip);
            }
        }

        let mut values = self.chip_values.borrow_mut();
        values.clear();
        values.extend(candidates.into_iter().take(POOL));
        let mut child = self.suggestion_strip.first_child();
        let mut i = 0;
        while let Some(widget) = child {
            child = widget.next_sibling();
            if let Some(chip) = widget.downcast_ref::<gtk::Button>() {
                match values.get(i) {
                    Some(v) => {
                        chip.set_label(v);
                        chip.set_visible(true);
                    }
                    None => chip.set_visible(false),
                }
            }
            i += 1;
        }
        self.strip_scroll.set_visible(true);
    }

    // ── Input ─────────────────────────────────────────────────────────────

    /// Handle a submitted line. Returns whether the entry should be cleared.
    fn submit(self: &Rc<Self>, raw: &str) -> bool {
        let Some(key) = self.active.borrow().clone() else { return false };

        // §9.3: a dead socket at send time means nothing was sent — keep the
        // input text and say so, rather than clearing it and losing the user's
        // message.
        if !self.app.is_connected() {
            self.status_label.set_text("not connected — message not sent");
            return false;
        }

        // The split gate (`chat.allow_split_messages`), mirroring the web:
        // the server splits long PRIVMSGs into consecutive wire lines itself,
        // so what's gated here is *surprise* — a /me over one line is a hard
        // block (CTCP ACTION cannot split), and a plain message that will
        // split needs either the setting on or a second Send press.
        let (split_body, is_action) = match raw.strip_prefix('/') {
            Some(rest) if !rest.starts_with('/') => match rest.split_once(' ') {
                Some((c, a)) if c.eq_ignore_ascii_case("me") => (Some(a.to_string()), true),
                _ => (None, false), // other commands don't ride PRIVMSG
            },
            _ => (Some(raw.strip_prefix('/').unwrap_or(raw).to_string()), false),
        };
        if let Some(body) = &split_body {
            let budget =
                if is_action { crate::split::ACTION_MAX_BYTES } else { crate::split::MESSAGE_MAX_BYTES };
            let chunks = crate::split::chunk_count(body, budget);
            if chunks > 1 {
                if is_action {
                    self.status_label.set_text(
                        "action too long for one IRC line — actions can't split; shorten it",
                    );
                    return false;
                }
                let allow =
                    self.app.setting("chat.allow_split_messages").as_bool().unwrap_or(false);
                if !allow && !self.pending_split_confirm.get() {
                    self.pending_split_confirm.set(true);
                    self.status_label.set_text(&format!(
                        "will split into {chunks} IRC lines — press Enter again to send"
                    ));
                    return false;
                }
            }
        }
        self.pending_split_confirm.set(false);

        if let Some(rest) = raw.strip_prefix('/') {
            if !rest.starts_with('/') {
                return self.run_command(&key, rest);
            }
        }
        // A leading `//` escapes to a literal slash.
        let text = raw.strip_prefix('/').unwrap_or(raw).to_string();

        let Some(network_id) = key.network_id else {
            self.status_label.set_text("this buffer cannot be sent to");
            return false;
        };
        // §9.3: no optimistic rendering. The authoritative row echoes back as
        // an `irc` event with self:true and its real id — that is when it
        // renders. `clientId` only tells us accepted/failed.
        self.app.send(ClientVerb::Send {
            network_id,
            target: self.app.wire_target(&key),
            text,
            client_id: Some(new_client_id()),
        })
    }

    /// Slash commands are parsed **client-side** — the server does not
    /// interpret `/` in `send` text (§12). Known verbs map to typed messages;
    /// everything else falls through to `raw`, which is the documented escape
    /// hatch.
    fn run_command(self: &Rc<Self>, key: &BufferKey, rest: &str) -> bool {
        let (cmd, args) = match rest.split_once(' ') {
            Some((c, a)) => (c.to_ascii_lowercase(), a.trim().to_string()),
            None => (rest.to_ascii_lowercase(), String::new()),
        };
        let network_id = key.network_id;

        let verb = match (cmd.as_str(), network_id) {
            ("me", Some(net)) => Some(ClientVerb::Action {
                network_id: net,
                target: self.app.wire_target(key),
                text: args,
                client_id: Some(new_client_id()),
            }),
            ("notice", Some(net)) => {
                let (target, text) = split_target(&args);
                Some(ClientVerb::Notice {
                    network_id: net,
                    target,
                    text,
                    client_id: Some(new_client_id()),
                })
            }
            ("msg" | "query", Some(net)) => {
                let (target, text) = split_target(&args);
                if text.is_empty() {
                    // Opening a conversation is explicit user intent, which is
                    // exactly when `open-buffer` is the right verb.
                    let dm = BufferKey::new(Some(net), &target);
                    self.app.open_buffer(&dm);
                    self.activate(&dm);
                    return true;
                }
                Some(ClientVerb::Send {
                    network_id: net,
                    target,
                    text,
                    client_id: Some(new_client_id()),
                })
            }
            ("join", Some(net)) => {
                let mut parts = args.split_whitespace();
                let channel = parts.next().unwrap_or_default().to_string();
                let chan_key = BufferKey::new(Some(net), &channel);
                self.app.store.borrow_mut().note_pending_join(chan_key);
                Some(ClientVerb::Join {
                    network_id: net,
                    channel,
                    key: parts.next().map(str::to_string),
                })
            }
            ("part" | "leave", Some(net)) => Some(ClientVerb::Part {
                network_id: net,
                channel: if args.is_empty() { self.app.wire_target(key) } else { args.clone() },
                reason: None,
            }),
            ("close", _) => Some(ClientVerb::CloseBuffer {
                network_id,
                target: self.app.wire_target(key),
                reason: None,
            }),
            ("away", _) => Some(ClientVerb::Away {
                message: (!args.is_empty()).then_some(args),
            }),
            ("back", _) => Some(ClientVerb::Back),
            ("clear", _) => {
                Some(ClientVerb::ClearBuffer { network_id, target: self.app.wire_target(key) })
            }
            // Start/join a voice call in this buffer. Always recognised — even
            // in a build without the `voice` feature — because falling through
            // to the raw fallback sent "CALL" to the IRC server and came back
            // "Unknown command", which explains nothing.
            // The channel browser. Server-cached, so it opens instantly and
            // only issues a real LIST when explicitly refreshed.
            ("list" | "channels", Some(net)) => {
                crate::chanlist::open(&self.app, net, self.window.clone().upcast_ref());
                return true;
            }
            ("call", _) => {
                self.try_start_call();
                return true;
            }
            // CTCP is NOT an IRC command — it is a PRIVMSG whose body is
            // wrapped in \x01 — so it has to go out through the typed verb.
            // Without these arms both fell through to the raw passthrough,
            // which sent a literal "CTCP …" line and got back the server's
            // "Unknown command". The nicklist menu always used the verb; only
            // the typed commands were missing.
            ("ctcp", Some(net)) => {
                let mut it = args.split_whitespace();
                let (Some(target), Some(kind)) = (it.next(), it.next()) else {
                    self.status_label.set_text("usage: /ctcp <nick> <TYPE> [args]");
                    return true;
                };
                Some(ClientVerb::Ctcp {
                    network_id: net,
                    target: target.to_string(),
                    ctcp_type: kind.to_ascii_uppercase(),
                    args: it.collect::<Vec<_>>().join(" "),
                    // Replies surface in the buffer the request was issued from.
                    issuing_target: self.app.wire_target(key),
                })
            }
            ("ping", Some(net)) => {
                let target = args.split_whitespace().next().unwrap_or("");
                if target.is_empty() {
                    self.status_label.set_text("usage: /ping <nick>");
                    return true;
                }
                // Convention: carry a timestamp so the reply can be timed.
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs().to_string())
                    .unwrap_or_default();
                Some(ClientVerb::Ctcp {
                    network_id: net,
                    target: target.to_string(),
                    ctcp_type: "PING".to_string(),
                    args: stamp,
                    issuing_target: self.app.wire_target(key),
                })
            }
            ("whois" | "wi", Some(net)) => {
                let target = args.split_whitespace().next().unwrap_or("").to_string();
                if !target.is_empty() {
                    self.note_whois(&target);
                    Some(ClientVerb::Raw {
                        network_id: net,
                        line: format!("WHOIS {target} {target}"),
                    })
                } else {
                    None
                }
            }
            // `/friend` opens the editor: with no friends yet the sidebar has
            // no FRIENDS section to click, so this is the way in from cold.
            // With a nick, it pre-fills for that person on this network.
            ("friend" | "friends", net) => {
                let who = args.split_whitespace().next().unwrap_or("");
                match (net, who.is_empty()) {
                    (Some(net), false) => {
                        crate::frienddialog::FriendDialog::add_for_nick(&self.app, net, who)
                    }
                    _ => crate::frienddialog::FriendDialog::add(&self.app),
                }
                return true;
            }
            // Where uploads go — the uploader picker/manager (#514).
            ("uploads" | "uploaders", _) => {
                crate::uploadersdialog::open(&self.app);
                return true;
            }
            ("search" | "find", _) => {
                if !args.is_empty() {
                    self.app.search(&args, self.active.borrow().as_ref());
                }
                self.open_search(false);
                return true;
            }
            // Rendered into the buffer, not the status line: 75 command names
            // do not fit on one line, so the status version simply truncated
            // and told you nothing.
            ("help", _) => {
                let mut names: Vec<&str> = crate::commands::KNOWN.to_vec();
                names.sort_unstable();
                let mut lines = vec![format!("{} commands:", names.len())];
                // Six to a line, padded into columns so it reads as a table
                // rather than a paragraph.
                for chunk in names.chunks(6) {
                    lines.push(
                        chunk
                            .iter()
                            .map(|n| format!("/{n:<14}"))
                            .collect::<String>()
                            .trim_end()
                            .to_string(),
                    );
                }
                lines.push(String::new());
                lines.push("anything else is sent as a raw IRC line".to_string());
                self.app.store.borrow_mut().inject_lines(key, "help", lines);
                *self.drawn.borrow_mut() = Drawn::default();
                self.render_active();
                return true;
            }
            // The command table: mode shortcuts, kick/ban, services, text
            // expansions. Anything it does not claim falls through to the raw
            // passthrough below, so an unknown command still reaches the
            // network rather than being rejected (§12).
            (name, Some(net)) => {
                let channel = self.app.wire_target(key);
                // The network's PREFIX letters disambiguate `+q`
                // (owner vs quiet) — see commands::to_raw.
                let prefix_modes = self
                    .app
                    .store
                    .borrow()
                    .networks
                    .get(&net)
                    .map(|n| n.isupport.prefix_modes.clone())
                    .unwrap_or_default();

                if let Some(text) = crate::commands::to_message(name, &args) {
                    Some(ClientVerb::Send {
                        network_id: net,
                        target: channel,
                        text,
                        client_id: Some(new_client_id()),
                    })
                } else if let Some(text) = crate::commands::to_action(name, &args) {
                    Some(ClientVerb::Action {
                        network_id: net,
                        target: channel,
                        text,
                        client_id: Some(new_client_id()),
                    })
                } else if let Some(line) = crate::commands::to_raw(name, &args, &channel, &prefix_modes) {
                    // `/kickban` needs the kick as well; the table gives the ban.
                    if matches!(name, "kickban" | "kb") {
                        let who = args.split_whitespace().next().unwrap_or("").to_string();
                        self.app.send(ClientVerb::Raw { network_id: net, line });
                        Some(ClientVerb::Raw {
                            network_id: net,
                            line: format!("KICK {channel} {who}"),
                        })
                    } else {
                        Some(ClientVerb::Raw { network_id: net, line })
                    }
                } else {
                    let line = if args.is_empty() {
                        name.to_uppercase()
                    } else {
                        format!("{} {args}", name.to_uppercase())
                    };
                    Some(ClientVerb::Raw { network_id: net, line })
                }
            }
            (_, None) => None,
        };

        match verb {
            Some(v) => self.app.send(v),
            None => {
                self.status_label.set_text("that command needs a network buffer");
                false
            }
        }
    }
}

fn split_target(args: &str) -> (String, String) {
    match args.split_once(' ') {
        Some((t, rest)) => (t.to_string(), rest.trim().to_string()),
        None => (args.to_string(), String::new()),
    }
}

/// A correlation id for `send`/`action`/`notice` (§6). Only needs to be unique
/// within this socket's lifetime.
fn new_client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("c{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Content type from a filename, for the upload's multipart part.
fn mime_for(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "txt" | "log" => "text/plain",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Remove every ROW from a list box, skipping any non-row child (e.g. a
/// `set_parent`'d popover), which `GtkListBox::remove` rejects. Removing a row
/// unparents it, so `first_child` advances; a non-row is stepped over with
/// `next_sibling` rather than retried, which is what stops the infinite
/// "Tried to remove non-child" loop that once wrote gigabytes of warnings.
/// Whether gdk-pixbuf on this system can *write* the named format. Most
/// installs read webp but cannot write it, which is what decides the png
/// fallback in the upload recompressor.
fn pixbuf_has_writer(name: &str) -> bool {
    gtk::gdk_pixbuf::Pixbuf::formats()
        .iter()
        .any(|f| f.is_writable() && f.name().as_deref() == Some(name))
}

/// An event's moment as unix seconds, when parseable. Hand-rolled parser —
/// glib::DateTime dragged locale/timezone machinery into a fixed-format
/// ASCII string, at ~500 calls per redraw before the last_spoke map moved
/// into the store.
fn event_unix(e: &lurker_proto::MessageEvent) -> Option<i64> {
    e.time.as_deref().and_then(lurker_proto::timeparse::rfc3339_to_unix)
}

/// Pause and detach the media stream from any player inside `root`, so the
/// widget tree tears down as plain widgets. See clear_embeds (#11).
fn defuse_media(root: &gtk::Widget) {
    if let Some(video) = root.downcast_ref::<gtk::Video>() {
        if let Some(stream) = video.media_stream() {
            if stream.is_playing() {
                stream.pause();
            }
        }
        video.set_media_stream(gtk::MediaStream::NONE);
        return;
    }
    if let Some(controls) = root.downcast_ref::<gtk::MediaControls>() {
        if let Some(stream) = controls.media_stream() {
            if stream.is_playing() {
                stream.pause();
            }
        }
        controls.set_media_stream(gtk::MediaStream::NONE);
        return;
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        defuse_media(&widget);
        child = widget.next_sibling();
    }
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn clear_list(list: &gtk::ListBox) {
    let mut child = list.first_child();
    while let Some(widget) = child {
        let next = widget.next_sibling();
        if let Ok(row) = widget.clone().downcast::<gtk::ListBoxRow>() {
            list.remove(&row);
        }
        child = next;
    }
}

/// A full-resolution image viewer. Opens fit-to-window (scaled down to fit,
/// never up); clicking toggles 1:1 actual size inside a scroller so large

/// YouTube URLs in a message, for link previews. Returns (url, ()) to mirror
/// the media_urls shape the caller iterates.
fn preview_urls(text: &str) -> Vec<(String, ())> {
    // Any non-media http(s) link earns a card, not just YouTube (field
    // request). Capped at two per message: a pasted list of ten links must
    // not become ten fetches and ten cards.
    crate::media::find_links(text)
        .into_iter()
        .filter(|(_, _, url)| crate::media::is_previewable(url))
        .map(|(_, _, url)| (url, ()))
        .take(2)
        .collect()
}
