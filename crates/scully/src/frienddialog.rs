// "Configure friend" — add or edit a contact.
//
// A contact is one *person* who may be several nicks on several networks; that
// is the whole reason the feature exists, and it is why this form is a list of
// targets rather than a single nick field. Exactly one target is primary: the
// DM that opens when the friend is clicked in the sidebar.
//
// The server owns contacts (`set-contact` / `delete-contact`), so this window
// sends and closes — the echoed `contact-updated` frame is what actually
// updates the UI, here and on every other device.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{glib, pango};

use crate::app::AppRef;

/// One editable row of the targets list.
struct TargetRow {
    network: gtk::DropDown,
    nick: gtk::Entry,
    primary: gtk::CheckButton,
    row: gtk::Box,
}

pub struct FriendDialog;

impl FriendDialog {
    /// Add a friend from scratch.
    pub fn add(app: &AppRef) {
        build(app, None, None);
    }

    /// Add a friend pre-filled from a nick the user right-clicked. If that
    /// (network, nick) is already watched, edit that contact instead — the same
    /// resolution the web client's nick menu does, so the menu item can be
    /// labelled honestly.
    pub fn add_for_nick(app: &AppRef, network_id: i64, nick: &str) {
        let existing = app.store.borrow().contact_for(network_id, nick).map(|c| c.id);
        match existing {
            Some(id) => build(app, Some(id), None),
            None => build(app, None, Some((network_id, nick.to_string()))),
        }
    }

    /// Edit an existing contact.
    pub fn edit(app: &AppRef, contact_id: i64) {
        build(app, Some(contact_id), None);
    }
}

fn build(app: &AppRef, contact_id: Option<i64>, prefill: Option<(i64, String)>) {
    // Networks, in a stable order, for every target row's dropdown.
    let networks: Vec<(i64, String)> = {
        let store = app.store.borrow();
        store
            .networks
            .values()
            .map(|n| {
                let name = if n.name.is_empty() { format!("network {}", n.id) } else { n.name.clone() };
                (n.id, name)
            })
            .collect()
    };
    if networks.is_empty() {
        return; // No networks, no targets to watch.
    }

    // The contact being edited, copied out so the store borrow ends here.
    let existing = contact_id.and_then(|id| app.store.borrow().contact(id).cloned());

    let window = gtk::Window::builder()
        .title(match &existing {
            Some(c) => format!("Edit {}", c.display_name),
            None => "Add a friend".to_string(),
        })
        .default_width(460)
        .modal(false)
        .transient_for(&app.gtk_app.windows()[0])
        .destroy_with_parent(true)
        .build();
    crate::fit_to_screen(&window);
    window.add_css_class("chanctl");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(14);
    outer.set_margin_bottom(14);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    // ── Display name ──────────────────────────────────────────────────────
    outer.append(
        &gtk::Label::builder().label("DISPLAY NAME").xalign(0.0).css_classes(["chanctl-heading"]).build(),
    );
    let name_entry = gtk::Entry::builder()
        .placeholder_text("e.g. amiantos")
        .max_length(128)
        .hexpand(true)
        .build();
    if let Some(c) = &existing {
        name_entry.set_text(&c.display_name);
    } else if let Some((_, nick)) = &prefill {
        // Seed from the nick: the common case is that a person's display name
        // *is* the nick you already know them by.
        name_entry.set_text(nick);
    }
    outer.append(&name_entry);

    // ── Targets ───────────────────────────────────────────────────────────
    outer.append(
        &gtk::Label::builder().label("WATCH NICKS").xalign(0.0).css_classes(["chanctl-heading"]).build(),
    );
    outer.append(
        &gtk::Label::builder()
            .label("The primary nick is the DM that opens when you click the friend.")
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .css_classes(["settings-desc"])
            .build(),
    );

    let targets_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let rows: Rc<RefCell<Vec<Rc<TargetRow>>>> = Rc::new(RefCell::new(Vec::new()));

    // All the primary radios share one group, so choosing a primary
    // deselects the previous one without any bookkeeping here.
    let radio_group = gtk::CheckButton::new();

    let net_names: Vec<&str> = networks.iter().map(|(_, n)| n.as_str()).collect();
    let add_row = {
        let networks = networks.clone();
        let net_names: Vec<String> = net_names.iter().map(|s| s.to_string()).collect();
        let rows = rows.clone();
        let targets_box = targets_box.clone();
        let radio_group = radio_group.clone();
        move |network_id: Option<i64>, nick: &str, is_primary: bool| {
            let strs: Vec<&str> = net_names.iter().map(|s| s.as_str()).collect();
            let network = gtk::DropDown::from_strings(&strs);
            if let Some(id) = network_id {
                if let Some(idx) = networks.iter().position(|(nid, _)| *nid == id) {
                    network.set_selected(idx as u32);
                }
            }
            let nick_entry = gtk::Entry::builder()
                .placeholder_text("nick")
                .text(nick)
                .hexpand(true)
                .build();
            let primary = gtk::CheckButton::builder().label("primary").build();
            primary.set_group(Some(&radio_group));
            primary.set_active(is_primary);
            let remove = gtk::Button::builder()
                .label("✕")
                .tooltip_text("Stop watching this nick")
                .build();

            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row_box.append(&network);
            row_box.append(&nick_entry);
            row_box.append(&primary);
            row_box.append(&remove);

            let entry = Rc::new(TargetRow {
                network,
                nick: nick_entry,
                primary,
                row: row_box.clone(),
            });
            rows.borrow_mut().push(entry.clone());
            targets_box.append(&row_box);

            let rows = rows.clone();
            let targets_box = targets_box.clone();
            remove.connect_clicked(move |_| {
                // Removing the primary leaves nobody flagged; promote whoever
                // is left, or the row the user picks next has no effect.
                let was_primary = entry.primary.is_active();
                targets_box.remove(&entry.row);
                rows.borrow_mut().retain(|r| !Rc::ptr_eq(r, &entry));
                if was_primary {
                    if let Some(first) = rows.borrow().first() {
                        first.primary.set_active(true);
                    }
                }
            });
        }
    };

    match (&existing, &prefill) {
        (Some(c), _) if !c.targets.is_empty() => {
            for t in &c.targets {
                add_row(Some(t.network_id), &t.nick, t.is_primary);
            }
            // A contact whose primary flag never arrived would otherwise open
            // nothing; the server's own rule is "primary, else first".
            if !c.targets.iter().any(|t| t.is_primary) {
                if let Some(first) = rows.borrow().first() {
                    first.primary.set_active(true);
                }
            }
        }
        (_, Some((net, nick))) => add_row(Some(*net), nick, true),
        _ => add_row(None, "", true),
    }

    outer.append(&targets_box);

    let add_target = gtk::Button::builder().label("+ Watch another nick").halign(gtk::Align::Start).build();
    {
        let add_row = add_row.clone();
        add_target.connect_clicked(move |_| add_row(None, "", false));
    }
    outer.append(&add_target);

    // ── Notify ────────────────────────────────────────────────────────────
    let notify = gtk::CheckButton::builder()
        .label("Notify me when they come online")
        .active(existing.as_ref().is_some_and(|c| c.notify_online))
        .build();
    outer.append(&notify);

    let status = gtk::Label::builder().xalign(0.0).css_classes(["login-status"]).build();
    outer.append(&status);

    // ── Buttons ───────────────────────────────────────────────────────────
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);

    // Remove sits on the left, away from Save, and asks first: it is the one
    // irreversible thing this window can do.
    if let Some(id) = contact_id {
        let remove = gtk::Button::builder()
            .label("Remove friend")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();
        let app = app.clone();
        let window_ref = window.clone();
        let name = existing.as_ref().map(|c| c.display_name.clone()).unwrap_or_default();
        remove.connect_clicked(move |_| {
            let confirm = gtk::AlertDialog::builder()
                .message(format!("Remove {name} from your friends?"))
                .detail("Their messages stay; only the friend entry goes away.")
                .buttons(["Cancel", "Remove"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let app = app.clone();
            let window_ref = window_ref.clone();
            confirm.choose(Some(&window_ref.clone()), gtk::gio::Cancellable::NONE, move |answer| {
                if answer == Ok(1) {
                    app.send(lurker_proto::ClientVerb::DeleteContact { contact_id: id });
                    window_ref.close();
                }
            });
        });
        buttons.append(&remove);
    }

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    buttons.append(&cancel);
    buttons.append(&save);
    outer.append(&buttons);

    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }

    {
        let app = app.clone();
        let window = window.clone();
        let rows = rows.clone();
        let name_entry = name_entry.clone();
        let notify = notify.clone();
        let status = status.clone();
        let networks = networks.clone();
        save.connect_clicked(move |_| {
            let display_name = name_entry.text().trim().to_string();
            if display_name.is_empty() {
                status.set_text("A friend needs a name.");
                return;
            }

            // Blank rows are dropped rather than rejected — an empty row the
            // user added and changed their mind about is not an error.
            let mut targets = Vec::new();
            let mut primary_seen = false;
            for r in rows.borrow().iter() {
                let nick = r.nick.text().trim().to_string();
                if nick.is_empty() {
                    continue;
                }
                let Some((network_id, _)) = networks.get(r.network.selected() as usize) else {
                    continue;
                };
                let is_primary = r.primary.is_active() && !primary_seen;
                primary_seen |= is_primary;
                targets.push(serde_json::json!({
                    "networkId": network_id,
                    "nick": nick,
                    "isPrimary": is_primary,
                }));
            }
            if targets.is_empty() {
                status.set_text("Watch at least one nick.");
                return;
            }
            // If the row that was flagged primary got dropped for being blank,
            // nothing carries the flag. Promote the first rather than sending a
            // contact whose sidebar row would open nowhere.
            if !primary_seen {
                targets[0]["isPrimary"] = serde_json::Value::Bool(true);
            }

            app.send(lurker_proto::ClientVerb::SetContact {
                contact_id,
                display_name,
                notify_online: notify.is_active(),
                targets,
            });
            window.close();
        });
    }

    // Enter in the name field saves, the way every other form here behaves.
    {
        let save = save.clone();
        name_entry.connect_activate(move |_| save.emit_clicked());
    }

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&outer)
        .build();
    window.set_child(Some(&scroller));

    let controller = gtk::EventControllerKey::new();
    let esc = window.clone();
    controller.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            esc.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(controller);

    window.present();
}
