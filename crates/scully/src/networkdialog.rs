// "Add a network" — the same form Lurker's web client offers, as a real window.
//
// The field names are the server's (`POST /api/networks`), which is snake_case
// because that route predates the camelCase WS frames. Only name, host and nick
// are required; everything else is optional and omitted when blank rather than
// sent empty, so the server's own defaults apply.
//
// Passwords are typed by the user into a masked entry and posted straight to
// their own Lurker instance. Scully never stores them: the server keeps them
// encrypted and only ever reports *whether* one is set.

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::app::AppRef;

/// Open the add-network window, transient for `parent`.
pub fn open(app: &AppRef, parent: &gtk::Window) {
    build(app, parent, None);
}

/// Open the same form pre-filled, to edit an existing network.
pub fn open_edit(app: &AppRef, parent: &gtk::Window, row: lurker_client::NetworkRow) {
    build(app, parent, Some(row));
}

fn build(app: &AppRef, parent: &gtk::Window, existing: Option<lurker_client::NetworkRow>) {
    let editing = existing.as_ref().map(|r| r.id);
    let window = gtk::Window::builder()
        .title(match &existing {
            Some(r) => format!("Edit {}", r.name),
            None => "Add a network".to_string(),
        })
        .default_width(460)
        .modal(false)
        .transient_for(parent)
        .destroy_with_parent(true)
        .build();
    crate::fit_to_screen(&window);
    window.add_css_class("chanctl");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(14);
    outer.set_margin_bottom(14);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(10);

    // (key, label, placeholder, secret)
    let fields: [(&str, &str, &str, bool); 9] = [
        ("name", "Name", "Libera.Chat", false),
        ("host", "Server", "irc.libera.chat", false),
        ("port", "Port", "6697", false),
        ("nick", "Nick", "yournick", false),
        ("username", "Username", "(defaults to nick)", false),
        ("realname", "Real name", "(optional)", false),
        ("sasl_account", "SASL account", "(optional)", false),
        ("sasl_password", "SASL password", "", true),
        ("server_password", "Server password", "", true),
    ];

    let mut entries: Vec<(&str, gtk::Entry)> = Vec::new();
    for (row, (key, label, placeholder, secret)) in fields.iter().enumerate() {
        let lbl = gtk::Label::builder().label(*label).xalign(0.0).build();
        let entry = gtk::Entry::builder().placeholder_text(*placeholder).hexpand(true).build();
        if *secret {
            entry.set_visibility(false);
            entry.set_input_purpose(gtk::InputPurpose::Password);
        }
        grid.attach(&lbl, 0, row as i32, 1, 1);
        grid.attach(&entry, 1, row as i32, 1, 1);
        entries.push((key, entry));
    }

    // Prefill when editing; otherwise defaults for the common case.
    let set = |key: &str, value: &str| {
        if let Some((_, e)) = entries.iter().find(|(k, _)| *k == key) {
            e.set_text(value);
        }
    };
    match &existing {
        Some(r) => {
            set("name", &r.name);
            set("host", &r.host);
            set("port", &r.port.map(|p| p.to_string()).unwrap_or_default());
            set("nick", r.nick.as_deref().unwrap_or(""));
            set("username", r.username.as_deref().unwrap_or(""));
            set("realname", r.realname.as_deref().unwrap_or(""));
            set("sasl_account", r.sasl_account.as_deref().unwrap_or(""));
            // Passwords are never returned. A blank field means "leave it
            // alone", so say so rather than looking like the password is unset.
            for (key, has) in [("server_password", r.has_password), ("sasl_password", r.has_sasl_password)] {
                if has {
                    if let Some((_, e)) = entries.iter().find(|(k, _)| *k == key) {
                        e.set_placeholder_text(Some("(unchanged)"));
                    }
                }
            }
        }
        None => set("port", "6697"),
    }
    outer.append(&grid);

    let tls = gtk::CheckButton::with_label("Use TLS");
    tls.set_active(existing.as_ref().map(|r| r.tls).unwrap_or(true));
    let autoconnect = gtk::CheckButton::with_label("Connect automatically");
    autoconnect.set_active(existing.as_ref().map(|r| r.autoconnect).unwrap_or(true));
    outer.append(&tls);
    outer.append(&autoconnect);

    let channels = gtk::Entry::builder()
        .placeholder_text("#channels to join, space or comma separated (optional)")
        .build();
    // Autojoin channels are seeded at creation only; editing them afterwards is
    // done by joining/parting, not by rewriting a list.
    if editing.is_none() {
        outer.append(&channels);
    }

    let status = gtk::Label::builder().xalign(0.0).css_classes(["chanctl-current"]).build();
    outer.append(&status);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let add = gtk::Button::with_label(if editing.is_some() { "Save" } else { "Add network" });
    buttons.append(&spacer);
    buttons.append(&cancel);
    buttons.append(&add);
    outer.append(&buttons);

    window.set_child(Some(&outer));

    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }

    let entries = Rc::new(entries);
    {
        let app = app.clone();
        let window = window.clone();
        let entries = entries.clone();
        let tls = tls.clone();
        let autoconnect = autoconnect.clone();
        let channels = channels.clone();
        let status = status.clone();
        let add_btn = add.clone();
        add.connect_clicked(move |_| {
            let get = |key: &str| {
                entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, e)| e.text().trim().to_string())
                    .unwrap_or_default()
            };

            // The server enforces these too; checking here just gives a better
            // message than a 400.
            for required in ["name", "host", "nick"] {
                if get(required).is_empty() {
                    status.set_text(&format!("{required} is required"));
                    return;
                }
            }

            let mut payload = serde_json::Map::new();
            for (key, entry) in entries.iter() {
                let v = entry.text().trim().to_string();
                if v.is_empty() {
                    continue; // omit rather than send "", so server defaults apply
                }
                if *key == "port" {
                    match v.parse::<u16>() {
                        Ok(p) => {
                            payload.insert("port".into(), serde_json::json!(p));
                        }
                        Err(_) => {
                            status.set_text("port must be a number");
                            return;
                        }
                    }
                } else {
                    payload.insert((*key).to_string(), serde_json::json!(v));
                }
            }
            payload.insert("tls".into(), serde_json::json!(tls.is_active()));
            payload.insert("autoconnect".into(), serde_json::json!(autoconnect.is_active()));
            let chans = channels.text().trim().to_string();
            if !chans.is_empty() {
                payload.insert("default_channel".into(), serde_json::json!(chans));
            }

            status.set_text(if editing.is_some() { "saving…" } else { "adding…" });
            add_btn.set_sensitive(false);
            let window = window.clone();
            let status = status.clone();
            let add_btn = add_btn.clone();
            let done = move |res: Result<(), String>| match res {
                Ok(()) => window.close(),
                Err(e) => {
                    status.set_text(&e);
                    add_btn.set_sensitive(true);
                }
            };
            match editing {
                Some(id) => app.update_network(id, serde_json::Value::Object(payload), done),
                None => app.create_network(serde_json::Value::Object(payload), done),
            }
        });
    }

    // Enter anywhere in the form submits.
    for (_, entry) in entries.iter() {
        let add = add.clone();
        entry.connect_activate(move |_| add.emit_clicked());
    }

    let esc = gtk::EventControllerKey::new();
    let win = window.clone();
    esc.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            win.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(esc);

    window.present();
}
