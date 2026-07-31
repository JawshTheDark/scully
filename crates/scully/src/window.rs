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
use crate::format::{self, TEXT_COLUMN};

/// Scroll within this many pixels of the top to trigger loading an older page.
const PAGE_TRIGGER_PX: f64 = 150.0;
/// Treat the view as "at the bottom" within this many pixels, for autoscroll.
const BOTTOM_EPSILON: f64 = 40.0;
/// Baseline top padding, before any bottom-anchoring pad is added.
const BASE_TOP_MARGIN: i32 = 6;

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
        left.append(&buffer_scroll);
        left.add_css_class("sidebar");
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
        let tool = |glyph: &str, tip: &str| {
            gtk::Button::builder()
                .label(glyph)
                .tooltip_text(tip)
                .css_classes(["toolbtn"])
                .valign(gtk::Align::Center)
                .build()
        };
        let btn_search = tool("⌕", "Search messages (Ctrl+F)");
        let btn_popout = tool("⬈", "Pop this channel out into its own window");
        let btn_read = tool("✓", "Mark everything read");
        let btn_settings = tool("⚙", "Settings");
        // Hidden until `voiceEnabled` arrives — see refresh_voice_ui.
        let btn_call = tool("\u{1F4DE}", "Start or join a voice call (/call)");
        btn_call.set_visible(false);
        header.append(&btn_search);
        header.append(&btn_call);
        header.append(&btn_popout);
        header.append(&btn_read);
        header.append(&btn_settings);

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
            .build();

        let centre = gtk::Box::new(gtk::Orientation::Vertical, 0);
        centre.append(&header);
        centre.append(&scroller);
        centre.append(&status_label);
        centre.append(&entry);

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
            .position(230)
            .resize_start_child(false)
            .build();
        window.set_child(Some(&outer));

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
            btn_search,
            btn_popout,
            btn_read,
            btn_settings,
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
        add(gtk::TextTag::builder().name("preview-title").foreground("#c3e88d").weight(600).build());
        add(gtk::TextTag::builder().name("preview-desc").foreground("#8f8f9f").build());

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

        let sample = " ".repeat(TEXT_COLUMN);
        let layout = self.text_view.create_pango_layout(Some(&sample));
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
                    this.open_url(&url);
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
        self.entry.connect_activate(move |entry| {
            let text = entry.text().to_string();
            if text.trim().is_empty() {
                return;
            }
            if this.submit(&text) {
                entry.set_text("");
                this.history_pos.set(None);
                this.signal_typing("done");
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
            if !entry.text().is_empty() {
                this.signal_typing("active");
            }
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

        // Voice call: toolbar button and the nicklist panel's join button both
        // route through try_start_call, which reports precisely why nothing
        // happened when it can't proceed.
        let this = self.clone();
        self.btn_call.connect_clicked(move |_| this.try_start_call());
        let this = self.clone();
        self.call_button.connect_clicked(move |_| this.try_start_call());

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
                    // Only meaningful when the buffer is detached by a jump.
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

    /// Pull display-relevant settings into this window's cached state and
    /// layout: sidebar visibility, timestamp format, nick palette.
    fn apply_display_settings(&self) {
        self.retint_nick_tags();
        let is_popout = self.pinned.is_some();
        let fmt = self
            .app
            .setting("look.buffer.time_format")
            .as_str()
            .map(format::time_format_to_strftime)
            .unwrap_or_else(|| "%H:%M:%S".to_string());
        *self.time_fmt.borrow_mut() = fmt;

        // Layout toggles govern full windows; a popout's whole point is the
        // missing sidebar, so the setting must not resurrect it.
        if !is_popout {
            if let Some(show) = self.app.setting("look.layout.show_channel_list").as_bool() {
                self.buffer_pane.set_visible(show);
            }
        }
        if let Some(show) = self.app.setting("look.layout.show_member_list").as_bool() {
            self.member_pane.set_visible(show);
        }
    }

    fn raise_notification(&self, key: &BufferKey, event: &lurker_proto::MessageEvent) {
        // The store has already applied the server's notify verdict and the
        // freshness check (§5.3, §9.6). What remains is per-CATEGORY user
        // preference: the raw signals stay on the wire beside `notify`
        // precisely so the client can pick the alert kind per signal type.
        let enabled = if event.matched {
            self.app.setting("notifications.highlight.enabled").as_bool().unwrap_or(true)
        } else if event.dm {
            self.app.setting("notifications.dm.enabled").as_bool().unwrap_or(true)
        } else if event.notify_always {
            self.app.setting("notifications.always_notify.enabled").as_bool().unwrap_or(true)
        } else {
            true
        };
        if !enabled {
            return;
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
        struct Section {
            header: Option<String>,
            offline: bool,
            keys: Vec<BufferKey>,
            /// Section that mixes buffers from several networks (pinned, DMs,
            /// DCC). Its rows carry the network name, because pulling a buffer
            /// out of its network otherwise loses the only thing that says
            /// which `amiantos` this is.
            cross_network: bool,
        }
        let mut sections: Vec<Section> = Vec::new();
        let sort_key = |k: &BufferKey| (k.is_dm(), k.is_dcc(), k.target.clone());

        // System log (app-scoped), no header.
        let system: Vec<BufferKey> =
            store.buffers.keys().filter(|k| k.network_id.is_none()).cloned().collect();
        if !system.is_empty() {
            sections.push(Section { header: None, offline: false, keys: system, cross_network: false });
        }

        // Pinned — across all networks, any buffer kind.
        let mut pinned: Vec<BufferKey> = store
            .buffers
            .iter()
            .filter(|(_, b)| b.pinned)
            .map(|(k, _)| k.clone())
            .collect();
        pinned.sort_by_key(&sort_key);
        if !pinned.is_empty() {
            sections.push(Section {
                header: Some("★ PINNED".to_string()),
                offline: false,
                keys: pinned,
                cross_network: true,
            });
        }

        // Direct messages sit directly under the pinned channels: they are
        // people, not places, and a conversation you are actually having
        // outranks the list of rooms you happen to be in.
        let mut dms: Vec<BufferKey> = store
            .buffers
            .iter()
            .filter(|(k, b)| k.is_dm() && !b.pinned)
            .map(|(k, _)| k.clone())
            .collect();
        dms.sort_by_key(|k| k.target.clone());
        if !dms.is_empty() {
            sections.push(Section {
                header: Some("✉ DIRECT MESSAGES".to_string()),
                offline: false,
                keys: dms,
                cross_network: true,
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
                })
                .map(|(k, _)| k.clone())
                .collect();
            keys.sort_by_key(|k| (!k.is_server_log(), k.target.clone()));
            let name = if net.name.is_empty() {
                format!("network {id}")
            } else {
                net.name.to_uppercase()
            };
            sections.push(Section {
                header: Some(name),
                offline: net.state != lurker_proto::NetworkState::Connected,
                keys,
                cross_network: false,
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
                keys: dcc,
                cross_network: true,
            });
        }

        self.rebuilding.set(true);
        clear_list(&self.buffer_list);
        let mut rows = Vec::new();
        let mut select_index: Option<i32> = None;

        for section in sections {
            // A section with a header can be folded away by clicking it.
            let folded = section
                .header
                .as_ref()
                .is_some_and(|t| self.collapsed.borrow().contains(t));

            if let Some(title) = &section.header {
                // Collapsing must not hide the fact that something wants
                // attention, so a folded section carries its contents' unread
                // and highlight totals on the header itself.
                let (unread, highlights) = section.keys.iter().fold((0, 0), |(u, h), k| {
                    store.buffer(k).map_or((u, h), |b| (u + b.unread, h + b.highlights))
                });

                let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                let caret = gtk::Label::builder()
                    .label(if folded { "▸" } else { "▾" })
                    .css_classes(["network-header"])
                    .build();
                let header = gtk::Label::builder()
                    .xalign(0.0)
                    .label(title)
                    .hexpand(true)
                    .css_classes(["network-header"])
                    .build();
                if section.offline {
                    header.add_css_class("offline");
                }
                header_box.append(&caret);
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

                self.buffer_list.append(&row);
                // A non-selectable header still occupies a ListBox index, so
                // `rows` needs a placeholder to stay index-aligned. A sentinel
                // server key is harmless — header rows are never activated.
                rows.push(BufferKey::system());
            }

            if folded {
                continue;
            }

            for key in section.keys {
                let buf = store.buffer(&key);
                let label = buf.map(|b| b.display_name.clone()).unwrap_or_else(|| key.target.clone());
                let display = if key.is_server_log() {
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
                        .label(format!("\u{1F4DE} {call_n}"))
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
                                .css_classes(["buffer-net"])
                                .build(),
                        );
                    }
                }

                let (unread, highlights) =
                    buf.map(|b| (b.unread, b.highlights)).unwrap_or((0, 0));
                if highlights > 0 {
                    let badge = gtk::Label::builder()
                        .label(highlights.to_string())
                        .css_classes(["badge", "badge-highlight"])
                        .build();
                    row_box.append(&badge);
                } else if unread > 0 {
                    let badge = gtk::Label::builder()
                        .label(unread.to_string())
                        .css_classes(["badge"])
                        .build();
                    row_box.append(&badge);
                }

                let row = gtk::ListBoxRow::builder().child(&row_box).build();
                row.add_css_class("buffer-row");
                if buf.is_some_and(|b| !b.joined) && key.is_channel() {
                    row.add_css_class("parted");
                }
                if key.is_dcc() {
                    row.add_css_class("dcc");
                }
                if unread > 0 || highlights > 0 {
                    row.add_css_class("has-unread");
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
        if self.window.is_active() {
            self.mark_read_to_tail();
        }
    }

    // ── Message rendering ─────────────────────────────────────────────────

    fn render_active(&self) {
        let Some(key) = self.active.borrow().clone() else {
            self.clear_embeds();
            self.text.set_text("");
            return;
        };
        let store = self.app.store.borrow();
        let Some(buf) = store.buffer(&key) else {
            self.clear_embeds();
            self.text.set_text("");
            return;
        };

        self.title_label.set_text(&buf.display_name);
        self.topic_label.set_text(&mirc::strip(buf.topic.as_deref().unwrap_or("")));
        if self.pinned.is_some() {
            self.window.set_title(Some(&format!("{} — Scully", buf.display_name)));
        }

        // §9.3: skip self-events when building "recent speakers".
        let recent: std::collections::HashSet<String> = buf
            .events
            .iter()
            .filter(|e| !e.is_self && e.event_type.is_chat())
            .filter_map(|e| e.nick.as_ref().map(|n| n.to_ascii_lowercase()))
            .collect();

        // The event-noise tier, resolved client-side (`shared/eventFilter.ts`):
        // `none` hides the noise set entirely; `smart` keeps events only for
        // nicks who have recently spoken. Both are display choices, which is
        // why they live here and not in the store.
        let tier = self.app.event_mode.get();
        let events: Vec<_> = buf
            .events
            .iter()
            .filter(|e| match tier {
                lurker_proto::EventMode::None => !e.event_type.is_noise(),
                lurker_proto::EventMode::Smart => {
                    !e.event_type.is_consolidatable()
                        || e.nick
                            .as_ref()
                            .is_some_and(|n| recent.contains(&n.to_ascii_lowercase()))
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
            recent_speakers: Some(recent.into_iter().collect()),
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
            self.text.set_text("");
            0
        };

        let strftime = self.time_fmt.borrow().clone();
        let palette_len = self.palette_len.get();
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
                    if let Some(line) = format::line_for(e, &strftime, palette_len) {
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
        if let Some(fg) = style.fg_color() {
            let name = format!("mfg{fg}");
            self.ensure_tag(&name, |b| b.foreground(&fg).build());
            names.push(name);
        }
        if let Some(bg) = style.bg_color() {
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

    /// Remove every embedded child widget from the TextView while the buffer
    /// is still valid. Call this immediately before `set_text("")` so GTK
    /// never unmaps a child against a half-cleared buffer (the SIGSEGV in
    /// `gtk_text_view_remove` the coredump pinned).
    fn clear_embeds(&self) {
        for widget in self.embeds.borrow_mut().drain(..) {
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

        self.insert_with_tags(&line.time, &["time"]);
        self.insert_with_tags("  ", &["time"]);
        self.insert_with_tags(&line.nick, &[line.nick_tag.as_deref().unwrap_or("nick-plain")]);
        self.insert_with_tags("  ", &["time"]);

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
        if count > 0 {
            self.call_status.set_text(&format!("\u{1F4DE} Call in progress — {count} in call"));
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
            .map(|m| crate::nickmenu::Rank::from_mode(m.modes.first().map(String::as_str)))
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

        let this = self.clone();
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, param| {
            if let Some(id) = param.and_then(|p| p.get::<String>()) {
                this.run_nick_command(&id);
            }
        });
        group.add_action(&action);

        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.insert_action_group("nick", Some(&group));
        popover.set_parent(&self.member_list);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x, y, 1, 1)));

        // Header showing who and the clicker's own status, so the gating is
        // legible ("you are op here").
        let _ = nick;
        popover.popup();
        *self.nick_menu.borrow_mut() = Some(popover);
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
        let cx = crate::buffermenu::BufContext {
            is_channel: key.is_channel(),
            is_dm: key.is_dm(),
            in_store,
            joined,
            pinned,
        };
        let model = crate::buffermenu::menu_model(&cx);

        let this = self.clone();
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("cmd", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, param| {
            if let Some(id) = param.and_then(|p| p.get::<String>()) {
                this.run_buffer_command(&id);
            }
        });
        group.add_action(&action);

        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.insert_action_group("buf", Some(&group));
        popover.set_parent(&self.buffer_list);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x, y, 1, 1)));

        *self.menu_buffer.borrow_mut() = Some(key);
        popover.popup();
        *self.buffer_menu.borrow_mut() = Some(popover);
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
        if self.app.device.borrow().whois_in_active_buffer {
            if let Some(key) = self.active.borrow().clone() {
                self.pending_whois.borrow_mut().insert(lurker_proto::fold(nick), key);
            }
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
                            click.connect_released(move |_, _, _, _| {
                                open_image_viewer(&tex, &title);
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
                crate::media::MediaKind::Video if device.inline_videos => {
                    let media = gtk::MediaFile::for_file(&gtk::gio::File::for_uri(&url));
                    let video = gtk::Video::builder().media_stream(&media).build();
                    video.set_size_request(420, 260);
                    video.add_css_class("embed");
                    Some(video.upcast())
                }
                crate::media::MediaKind::Audio if device.inline_audio => {
                    let media = gtk::MediaFile::for_file(&gtk::gio::File::for_uri(&url));
                    let controls = gtk::MediaControls::new(Some(&media));
                    controls.set_size_request(360, -1);
                    controls.add_css_class("embed");
                    Some(controls.upcast())
                }
                _ => None,
            };
            if let Some(widget) = widget {
                let mut end = self.text.end_iter();
                self.text.insert(&mut end, "\n");
                // Indent the embed to the text column.
                self.insert_with_tags(&" ".repeat(TEXT_COLUMN), &["time"]);
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
            match cached {
                Some(Some(preview)) => {
                    let indent = " ".repeat(TEXT_COLUMN);
                    if !preview.title.is_empty() {
                        self.text.insert(&mut self.text.end_iter(), "\n");
                        self.insert_with_tags(&indent, &["time"]);
                        self.insert_with_tags(&format!("▶ {}", preview.title), &["preview-title"]);
                    }
                    if !preview.description.is_empty() {
                        // One trimmed line of description.
                        let desc: String = preview.description.chars().take(200).collect();
                        self.text.insert(&mut self.text.end_iter(), "\n");
                        self.insert_with_tags(&indent, &["time"]);
                        self.insert_with_tags(&desc, &["preview-desc"]);
                    }
                }
                Some(None) => {} // fetched, nothing usable
                None => self.app.fetch_preview(url.clone(), key.clone()),
            }
        }
    }

    /// Upload pasted or dropped content and paste the resulting link at the
    /// cursor. This is the Lurker upload pipeline (§10) — the server
    /// optimises, hosts, and records it in the account's upload history.
    fn upload_and_insert(self: &Rc<Self>, filename: String, mime: String, bytes: Vec<u8>) {
        self.status_label
            .set_text(&format!("uploading {filename} ({} KB)…", bytes.len() / 1024));
        let this = self.clone();
        self.app.upload(filename, mime, bytes, move |result| match result {
            Ok(url) => {
                // Insert at the cursor, space-padded so it can't fuse with
                // adjacent words.
                let mut pos = this.entry.position();
                let text = this.entry.text();
                let pad_before = pos > 0
                    && !text
                        .chars()
                        .nth(pos as usize - 1)
                        .is_none_or(char::is_whitespace);
                let insert =
                    format!("{}{url} ", if pad_before { " " } else { "" });
                this.entry.insert_text(&insert, &mut pos);
                this.entry.set_position(pos);
                this.entry.grab_focus();
                this.update_status();
            }
            Err(e) => this.status_label.set_text(&format!("upload failed: {e}")),
        });
    }

    /// Handle a paste that contains files or an image rather than text.
    /// Returns false when the clipboard holds neither (normal paste proceeds).
    fn paste_media(self: &Rc<Self>) -> bool {
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
        // Ops first, then by nick — the conventional IRC nicklist order.
        members.sort_by_key(|m| {
            let rank = match m.modes.first().map(String::as_str) {
                Some("q") => 0,
                Some("a") => 1,
                Some("o") => 2,
                Some("h") => 3,
                Some("v") => 4,
                _ => 5,
            };
            (rank, m.nick.to_ascii_lowercase())
        });
        self.member_count.set_text(&format!("{} members", members.len()));

        *self.member_nicks.borrow_mut() = members.iter().map(|m| m.nick.clone()).collect();
        for m in members {
            let sigil = m.sigil().map(|c| c.to_string()).unwrap_or_default();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .label(format!("{sigil}{}", m.nick))
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            label.add_css_class("member");
            if m.away {
                label.add_css_class("away");
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

        self.status_label
            .set_text(&format!(
                "{nick}  │  {where_}  │  {conn}{badge}{typing}{jumped}{paused}"
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

        let (anchor, prefix, kind) = crate::input::classify(&text, cursor);
        // A bare "/" is a legitimate "what can I type?" prompt, so commands may
        // complete from an empty prefix; nicks and channels may not — Tab on
        // nothing would dump the whole nicklist into the composer.
        if prefix.is_empty() && kind != crate::input::Token::Command {
            return;
        }
        let Some(key) = self.active.borrow().clone() else { return };

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
                let Some(buf) = store.buffer(&key) else { return };
                let own = key
                    .network_id
                    .and_then(|id| store.networks.get(&id))
                    .and_then(|n| n.nick.clone());
                let recent = buf
                    .events
                    .iter()
                    .rev()
                    .filter(|e| !e.is_self && e.event_type.is_chat())
                    .filter_map(|e| e.nick.clone());
                let members = buf.members.values().map(|m| m.nick.clone());
                crate::input::candidates(&prefix, recent, members, own.as_deref())
            }
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
/// images can be panned. Escape or closing the window dismisses it.
fn open_image_viewer(texture: &gtk::gdk::Texture, title: &str) {
    let mut builder = gtk::Window::builder()
        .title(title)
        .default_width(texture.width().clamp(320, 1200))
        .default_height(texture.height().clamp(240, 900))
        .destroy_with_parent(true);
    // Anchor to the running app's active window so it opens in front.
    let parent = gtk::gio::Application::default()
        .and_then(|a| a.downcast::<gtk::Application>().ok())
        .and_then(|a| a.active_window());
    if let Some(parent) = &parent {
        builder = builder.transient_for(parent);
    }
    let window = builder.build();
    window.add_css_class("image-viewer");

    let picture = gtk::Picture::for_paintable(texture);
    picture.set_can_shrink(true);
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);

    let scroller = gtk::ScrolledWindow::builder().child(&picture).build();
    window.set_child(Some(&scroller));

    // Toggle fit / actual size on click.
    let actual = std::rc::Rc::new(std::cell::Cell::new(false));
    let tex = texture.clone();
    let pic = picture.clone();
    let click = gtk::GestureClick::builder().button(1).build();
    click.connect_released(move |_, _, _, _| {
        let now = !actual.get();
        actual.set(now);
        if now {
            pic.set_can_shrink(false);
            pic.set_size_request(tex.width(), tex.height());
            pic.set_cursor_from_name(Some("zoom-out"));
        } else {
            pic.set_can_shrink(true);
            pic.set_size_request(-1, -1);
            pic.set_cursor_from_name(Some("zoom-in"));
        }
    });
    picture.add_controller(click);
    picture.set_cursor_from_name(Some("zoom-in"));

    // Escape closes.
    let keys = gtk::EventControllerKey::new();
    let win = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            win.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);

    window.present();
}

/// YouTube URLs in a message, for link previews. Returns (url, ()) to mirror
/// the media_urls shape the caller iterates.
fn preview_urls(text: &str) -> Vec<(String, ())> {
    crate::media::find_links(text)
        .into_iter()
        .filter(|(_, _, url)| crate::media::is_youtube(url))
        .map(|(_, _, url)| (url, ()))
        .collect()
}
