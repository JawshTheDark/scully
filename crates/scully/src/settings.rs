// The settings window.
//
// Generated entirely from the server's self-describing registry
// (`GET /api/settings/bootstrap`), per §10: types, bounds, enum choices,
// labels, grouping and defaults all come from the wire, so a server that adds
// a setting shows it here with no client change. Hardcoding keys is exactly
// what the doc warns against.
//
// Layout: category sidebar on the left (first-seen order from the registry),
// grouped rows on the right. Each row: label, dotted key as a power-user
// subtitle, description, the type-appropriate control, and a reset button.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use lurker_client::SettingOption;

use crate::app::AppRef;

/// Display labels for category ids (cosmetic; unknown ids are prettified).
fn category_label(id: &str) -> String {
    match id {
        "appearance" => "Appearance".into(),
        "chat" => "Chat".into(),
        "events" => "Events".into(),
        "input" => "Input bar".into(),
        "uploads" => "Uploads".into(),
        "notifications" => "Notifications".into(),
        "away" => "Away".into(),
        DEVICE_CATEGORY => device_label(),
        ABOUT_CATEGORY => "About".into(),
        other => prettify(other),
    }
}

fn device_label() -> String {
    "This device".to_string()
}

fn prettify(id: &str) -> String {
    let mut out = String::new();
    for (i, part) in id.split(['-', '_', '.']).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.extend(first.to_uppercase());
            } else {
                out.push(first);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

/// The last `max` bytes of the log, aligned to the first complete line — the
/// end is where the crash is. Missing or unreadable log reads as a note, not
/// an error: a debug report from a machine with no log yet is still a report.
fn read_log_tail(path: &std::path::Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return "(no log file)".to_string();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max {
        let _ = f.seek(SeekFrom::Start(len - max));
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        // A seek into the middle of a multibyte char makes read_to_string
        // fail; re-read lossily rather than give up.
        let _ = f.seek(SeekFrom::Start(if len > max { len - max } else { 0 }));
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut f, &mut bytes);
        buf = String::from_utf8_lossy(&bytes).into_owned();
    }
    match buf.find('\n') {
        Some(i) if len > max => buf[i + 1..].to_string(),
        _ => buf,
    }
}

/// Synthetic category id for device-local settings.
const DEVICE_CATEGORY: &str = "__device";
/// Synthetic category id for the About/debug page.
const ABOUT_CATEGORY: &str = "__about";

pub struct SettingsWindow {
    app: AppRef,
    window: gtk::Window,
    sidebar: gtk::ListBox,
    pane: gtk::Box,
    categories: RefCell<Vec<String>>,
}

impl SettingsWindow {
    pub fn open(app: &AppRef) {
        // The window the WM should place this in front of.
        let parent = app.gtk_app.active_window();

        // Single instance: re-present rather than stacking windows.
        if let Some(existing) = app.settings_window.borrow().as_ref() {
            if let Some(p) = &parent {
                existing.window.set_transient_for(Some(p));
            }
            existing.window.present();
            return;
        }

        let mut builder = gtk::Window::builder()
            .title("Scully — settings")
            .default_width(860)
            .default_height(640)
            // Anchor to the active window so the WM opens it in front and
            // centered on the parent, not off-screen or behind (a top-level
            // with no transient parent is placed wherever the WM likes).
            .destroy_with_parent(true);
        if let Some(p) = &parent {
            builder = builder.transient_for(p);
        }
        let window = builder.build();
        crate::fit_to_screen(&window);
        window.add_css_class("settings");

        let sidebar = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["settings-sidebar"])
            .build();
        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&sidebar)
            .build();
        sidebar_scroll.set_size_request(180, -1);

        let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pane.add_css_class("settings-pane");
        let pane_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&pane)
            .hexpand(true)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&sidebar_scroll);
        root.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        root.append(&pane_scroll);
        window.set_child(Some(&root));

        let this = Rc::new(SettingsWindow {
            app: app.clone(),
            window,
            sidebar,
            pane,
            categories: RefCell::new(Vec::new()),
        });

        this.build_sidebar();

        let handler = this.clone();
        this.sidebar.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let idx = row.index() as usize;
            let cat = handler.categories.borrow().get(idx).cloned();
            if let Some(cat) = cat {
                handler.populate(&cat);
            }
        });
        if let Some(first) = this.sidebar.row_at_index(0) {
            this.sidebar.select_row(Some(&first));
        }

        let owner = app.clone();
        this.window.connect_close_request(move |_| {
            owner.settings_window.replace(None);
            glib::Propagation::Proceed
        });

        this.window.present();
        *app.settings_window.borrow_mut() = Some(this);
    }

    fn build_sidebar(&self) {
        // Categories in first-seen registry order; the registry itself is
        // ordered sensibly, so no client-side taxonomy needed. One synthetic
        // category leads: device-local preferences the server doesn't sync.
        let registry = self.app.settings_registry.borrow();
        let mut seen: Vec<String> = vec![DEVICE_CATEGORY.to_string()];
        for opt in registry.iter() {
            if !opt.category.is_empty() && !seen.contains(&opt.category) {
                seen.push(opt.category.clone());
            }
        }
        // About trails everything: it is information, not preference.
        seen.push(ABOUT_CATEGORY.to_string());
        for cat in &seen {
            let label = gtk::Label::builder()
                .xalign(0.0)
                .label(category_label(cat))
                .css_classes(["settings-category"])
                .build();
            self.sidebar.append(&gtk::ListBoxRow::builder().child(&label).build());
        }
        *self.categories.borrow_mut() = seen;

        if self.categories.borrow().is_empty() {
            let msg = gtk::Label::builder()
                .label("Settings have not loaded yet — is the connection up?")
                .css_classes(["settings-empty"])
                .wrap(true)
                .build();
            self.pane.append(&msg);
        }
    }

    fn populate(&self, category: &str) {
        let mut child = self.pane.first_child();
        while let Some(widget) = child {
            let next = widget.next_sibling();
            self.pane.remove(&widget);
            child = next;
        }
        if category == DEVICE_CATEGORY {
            self.populate_device();
            return;
        }
        if category == ABOUT_CATEGORY {
            self.populate_about();
            return;
        }

        // The uploads category gets a real management surface at the top:
        // the uploader picker (#514) is an API, not a registry setting, so
        // the generated rows alone can't reach it.
        if category == "uploads" {
            let btn = gtk::Button::builder()
                .label("Manage uploaders…")
                .halign(gtk::Align::Start)
                .css_classes(["toolbtn"])
                .build();
            let app = self.app.clone();
            btn.connect_clicked(move |_| crate::uploadersdialog::open(&app));
            self.pane.append(&btn);
        }

        let registry = self.app.settings_registry.borrow();
        let mut last_group: Option<&str> = None;

        for opt in registry.iter().filter(|o| o.category == category) {
            if last_group != Some(opt.group.as_str()) {
                last_group = Some(opt.group.as_str());
                let heading = gtk::Label::builder()
                    .xalign(0.0)
                    .label(prettify(&opt.group))
                    .css_classes(["settings-group"])
                    .build();
                self.pane.append(&heading);
            }
            self.pane.append(&self.build_row(opt));
        }
    }

    /// The About page: version, build and environment facts, and a one-click
    /// copy of the whole block — the answer to "send me your debug info" in
    /// a bug report, without asking anyone to transcribe from a screenshot.
    fn populate_about(&self) {
        let heading = gtk::Label::builder()
            .xalign(0.0)
            .label("Scully")
            .css_classes(["settings-group"])
            .build();
        self.pane.append(&heading);

        let conn = self.app.conn.borrow().to_string();
        let server = self
            .app
            .rest
            .borrow()
            .as_ref()
            .map(|r| r.base().to_string())
            .unwrap_or_else(|| "(not connected)".into());
        let log_path = crate::paths::data_dir().join("scully.log");

        let rows: Vec<(&str, String)> = vec![
            ("Version", env!("CARGO_PKG_VERSION").to_string()),
            (
                "Build",
                format!(
                    "{}{}",
                    if cfg!(debug_assertions) { "debug" } else { "release" },
                    if cfg!(feature = "voice") { ", voice" } else { ", no voice" },
                ),
            ),
            (
                "GTK",
                format!("{}.{}.{}", gtk::major_version(), gtk::minor_version(), gtk::micro_version()),
            ),
            ("Platform", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)),
            (
                "Device class",
                if crate::is_mobile_class() { "phone".into() } else { "desktop".into() },
            ),
            ("Server", server),
            ("Connection", conn),
            ("Log file", log_path.display().to_string()),
            ("Config dir", crate::paths::config_dir().display().to_string()),
        ];

        for (label, value) in &rows {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.add_css_class("settings-row");
            row.append(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .label(*label)
                    .width_chars(12)
                    .css_classes(["settings-label"])
                    .build(),
            );
            row.append(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .label(value)
                    .hexpand(true)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .selectable(true)
                    .css_classes(["settings-desc"])
                    .build(),
            );
            self.pane.append(&row);
        }

        let block: String = rows
            .iter()
            .map(|(l, v)| format!("{l}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);

        let copy = gtk::Button::builder()
            .label("Copy debug info")
            .halign(gtk::Align::Start)
            .css_classes(["toolbtn"])
            .build();
        {
            let block = block.clone();
            copy.connect_clicked(move |btn| {
                btn.clipboard().set_text(&block);
                btn.set_label("Copied");
            });
        }
        buttons.append(&copy);

        // One press: debug block + the log tail, through the account's own
        // uploader (whatever /uploads selected — their Zipline, catbox, the
        // instance default), URL straight to the clipboard. "Send me your
        // debug info" becomes pasting one link.
        let upload = gtk::Button::builder()
            .label("Upload debug report")
            .tooltip_text(
                "Bundles this info plus the recent log and uploads it via your \
                 configured uploader; the link lands on your clipboard.",
            )
            .css_classes(["toolbtn"])
            .build();
        let result_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["settings-desc"])
            .build();
        {
            let app = self.app.clone();
            let log_path = log_path.clone();
            let result_label = result_label.clone();
            upload.connect_clicked(move |btn| {
                // Log tail read at CLICK time, not page-build time — the
                // interesting lines are usually the most recent ones.
                let tail = read_log_tail(&log_path, 256 * 1024);
                let report = format!(
                    "== Scully debug report ==\n{block}\n\n== log tail ==\n{tail}"
                );
                btn.set_sensitive(false);
                btn.set_label("Uploading…");
                let btn = btn.clone();
                let result_label = result_label.clone();
                app.upload(
                    format!("scully-debug-{}.txt", env!("CARGO_PKG_VERSION")),
                    "text/plain".to_string(),
                    report.into_bytes(),
                    move |result| {
                        btn.set_sensitive(true);
                        match result {
                            Ok(url) => {
                                btn.set_label("Uploaded — link copied");
                                btn.clipboard().set_text(&url);
                                result_label.set_text(&url);
                            }
                            Err(e) => {
                                btn.set_label("Upload debug report");
                                result_label.set_text(&format!("upload failed: {e}"));
                            }
                        }
                    },
                );
            });
        }
        buttons.append(&upload);

        self.pane.append(&buttons);
        self.pane.append(&result_label);
    }

    /// Device-local preferences: not server-synced, stored in
    /// `XDG_CONFIG_HOME/scully/device.json`. Inline media is per-device
    /// because it is a bandwidth/screen decision, like mobile font size.
    fn populate_device(&self) {
        let heading = gtk::Label::builder()
            .xalign(0.0)
            .label("Display")
            .css_classes(["settings-group"])
            .build();
        self.pane.append(&heading);

        let toggles: [(&str, &str, fn(&crate::app::DeviceSettings) -> bool,
                       fn(&mut crate::app::DeviceSettings, bool)); 5] = [
            (
                "Show images inline",
                "Image links render below the message. Fetched once, capped at 15 MB.",
                |d| d.inline_images,
                |d, v| d.inline_images = v,
            ),
            (
                "Show videos inline",
                "Video links get an embedded player. Nothing downloads until you press play.",
                |d| d.inline_videos,
                |d, v| d.inline_videos = v,
            ),
            (
                "Show audio inline",
                "Audio links get playback controls.",
                |d| d.inline_audio,
                |d, v| d.inline_audio = v,
            ),
            (
                "Show whois in the active buffer",
                "Whois replies appear in the channel or DM you ran them from,                  instead of only the server log.",
                |d| d.whois_in_active_buffer,
                |d, v| d.whois_in_active_buffer = v,
            ),
            (
                "Fetch link previews",
                "Links get a card with the page's title, description and thumbnail. Off by default: it fetches pages other people linked in chat.",
                |d| d.link_previews,
                |d, v| d.link_previews = v,
            ),
        ];

        for (label, description, get, set) in toggles {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.add_css_class("settings-row");
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            text.append(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .label(label)
                    .css_classes(["settings-label"])
                    .build(),
            );
            text.append(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .label(description)
                    .wrap(true)
                    .css_classes(["settings-desc"])
                    .build(),
            );
            row.append(&text);

            let switch = gtk::Switch::builder()
                .active(get(&self.app.device.borrow()))
                .valign(gtk::Align::Center)
                .build();
            let app = self.app.clone();
            switch.connect_state_set(move |_, state| {
                {
                    let mut device = app.device.borrow_mut();
                    set(&mut device, state);
                    device.save();
                }
                // Windows re-render so embeds appear/disappear immediately.
                app.notify(&[lurker_client::StoreEvent::SettingsChanged]);
                glib::Propagation::Proceed
            });
            row.append(&switch);
            self.pane.append(&row);
        }

        let note = gtk::Label::builder()
            .xalign(0.0)
            .label(
                "Stored on this machine only — every other setting in this window                  syncs through your Lurker server.",
            )
            .wrap(true)
            .css_classes(["settings-desc"])
            .build();
        self.pane.append(&note);
    }

    fn build_row(&self, opt: &SettingOption) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("settings-row");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let label = gtk::Label::builder()
            .xalign(0.0)
            .label(&opt.label)
            .css_classes(["settings-label"])
            .build();
        let key = gtk::Label::builder()
            .xalign(0.0)
            .label(&opt.key)
            .css_classes(["settings-key"])
            .build();
        text.append(&label);
        text.append(&key);
        if !opt.description.is_empty() {
            let desc = gtk::Label::builder()
                .xalign(0.0)
                .label(&opt.description)
                .wrap(true)
                .css_classes(["settings-desc"])
                .build();
            text.append(&desc);
        }
        row.append(&text);

        let control = self.build_control(opt);
        control.set_valign(gtk::Align::Center);
        row.append(&control);

        // Reset to default. DELETE /api/settings/:key is the authoritative
        // reset; the reply's values repaint the pane via SettingsChanged.
        let reset = gtk::Button::builder()
            .label("↺")
            .tooltip_text("Reset to default")
            .css_classes(["settings-reset"])
            .valign(gtk::Align::Center)
            .build();
        let app = self.app.clone();
        let reset_key = opt.key.clone();
        let category = opt.category.clone();
        reset.connect_clicked(move |_| {
            app.reset_setting(reset_key.clone());
            // Repaint this pane once the reply lands. The window is read
            // through the App at fire time, not captured at build time.
            let app = app.clone();
            let category = category.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                let win = app.settings_window.borrow().as_ref().cloned();
                if let Some(w) = win {
                    w.populate(&category);
                }
            });
        });
        row.append(&reset);

        row
    }

    /// The type-appropriate editor for one option, pre-filled with the
    /// effective value.
    fn build_control(&self, opt: &SettingOption) -> gtk::Widget {
        let app = self.app.clone();
        let key = opt.key.clone();
        let value = self.app.setting(&opt.key);

        match opt.setting_type.as_str() {
            "bool" => {
                let switch = gtk::Switch::builder()
                    .active(value.as_bool().unwrap_or(false))
                    .build();
                switch.connect_state_set(move |_, state| {
                    app.set_setting(key.clone(), serde_json::Value::Bool(state));
                    glib::Propagation::Proceed
                });
                switch.upcast()
            }
            "int" => {
                let min = opt.min.unwrap_or(0.0);
                let max = opt.max.unwrap_or(1_000_000.0);
                let current = value.as_f64().unwrap_or(min);
                let spin = gtk::SpinButton::with_range(min, max, 1.0);
                spin.set_value(current);
                spin.connect_value_changed(move |s| {
                    app.set_setting(
                        key.clone(),
                        serde_json::Value::Number(serde_json::Number::from(s.value_as_int())),
                    );
                });
                spin.upcast()
            }
            "enum" => {
                let choices = opt.choices.clone().unwrap_or_default();
                let labels: Vec<String> = choices
                    .iter()
                    .map(|c| {
                        opt.choice_labels
                            .as_ref()
                            .and_then(|m| m.get(c))
                            .cloned()
                            .unwrap_or_else(|| c.clone())
                    })
                    .collect();
                let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let dropdown = gtk::DropDown::from_strings(&label_refs);
                if let Some(current) = value.as_str() {
                    if let Some(pos) = choices.iter().position(|c| c == current) {
                        dropdown.set_selected(pos as u32);
                    }
                }
                dropdown.connect_selected_notify(move |d| {
                    if let Some(choice) = choices.get(d.selected() as usize) {
                        app.set_setting(key.clone(), serde_json::Value::String(choice.clone()));
                    }
                });
                dropdown.upcast()
            }
            "secret" => {
                // Write-only: the server never returns the stored value, so an
                // empty field means "unchanged", not "unset".
                let entry = gtk::PasswordEntry::builder()
                    .placeholder_text("(unchanged — type to replace)")
                    .show_peek_icon(true)
                    .build();
                entry.connect_activate(move |e| {
                    let text = e.text().to_string();
                    if !text.is_empty() {
                        app.set_setting(key.clone(), serde_json::Value::String(text));
                        e.set_text("");
                    }
                });
                entry.upcast()
            }
            "string-list" => {
                // Comma-separated editing; committed on Enter.
                let joined = value
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let entry = gtk::Entry::builder().text(joined).hexpand(false).build();
                entry.set_width_chars(24);
                entry.connect_activate(move |e| {
                    let list: Vec<serde_json::Value> = e
                        .text()
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| serde_json::Value::String(s.to_string()))
                        .collect();
                    app.set_setting(key.clone(), serde_json::Value::Array(list));
                });
                entry.upcast()
            }
            // `string` and `color`. A colour well would be nicer for `color`,
            // but a hex-text entry is faithful to what the server stores.
            _ => {
                let entry = gtk::Entry::builder()
                    .text(value.as_str().unwrap_or_default())
                    .build();
                entry.set_width_chars(18);
                entry.connect_activate(move |e| {
                    app.set_setting(
                        key.clone(),
                        serde_json::Value::String(e.text().to_string()),
                    );
                });
                entry.upcast()
            }
        }
    }
}
