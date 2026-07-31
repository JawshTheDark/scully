// The channel browser (`/list`).
//
// Paging, sorting and filtering all happen server-side: a large network's list
// runs to tens of thousands of channels, which is why Lurker caches it in the
// database and answers `chanlist-search` from there rather than shipping the
// whole thing. This window is a thin view over that — type to filter, Enter to
// join.
//
// Refreshing is deliberately a separate, explicit button: it makes the server
// issue a real `LIST` to the network, which is heavy and on some networks
// rate-limited or outright throttled. It should never happen just because a
// window opened.

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::app::AppRef;

/// Open the channel browser for `network_id`.
pub fn open(app: &AppRef, network_id: i64, parent: &gtk::Window) {
    let net_name = app
        .store
        .borrow()
        .networks
        .get(&network_id)
        .map(|n| n.name.clone())
        .unwrap_or_default();

    let window = gtk::Window::builder()
        .title(if net_name.is_empty() {
            "Channels".to_string()
        } else {
            format!("Channels on {net_name}")
        })
        .default_width(680)
        .default_height(520)
        .transient_for(parent)
        .destroy_with_parent(true)
        .build();
    window.add_css_class("chanctl");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let filter = gtk::Entry::builder()
        .placeholder_text("Filter by name or topic")
        .hexpand(true)
        .build();
    let refresh = gtk::Button::with_label("Refresh from server");
    refresh.set_tooltip_text(Some(
        "Ask the network for a fresh channel list. This can be slow and some \
         networks rate-limit it.",
    ));
    top.append(&filter);
    top.append(&refresh);
    outer.append(&top);

    let status = gtk::Label::builder().xalign(0.0).css_classes(["chanctl-current"]).build();
    outer.append(&status);

    let list = gtk::ListBox::builder().css_classes(["chanctl-list"]).build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .vexpand(true)
        .build();
    outer.append(&scroll);
    window.set_child(Some(&outer));

    // Rows currently displayed, so a click or Enter knows what it selected.
    let shown: Rc<std::cell::RefCell<Vec<String>>> = Rc::new(std::cell::RefCell::new(Vec::new()));

    let redraw = {
        let app = app.clone();
        let list = list.clone();
        let status = status.clone();
        let shown = shown.clone();
        move || {
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            let rows = app.chanlist.borrow();
            shown.borrow_mut().clear();
            for r in rows.iter() {
                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                row_box.append(
                    &gtk::Label::builder()
                        .label(&r.channel)
                        .xalign(0.0)
                        .width_chars(24)
                        .css_classes(["chanlist-name"])
                        .build(),
                );
                row_box.append(
                    &gtk::Label::builder()
                        .label(r.num_users.to_string())
                        .xalign(1.0)
                        .width_chars(6)
                        .css_classes(["badge"])
                        .build(),
                );
                row_box.append(
                    &gtk::Label::builder()
                        .label(lurker_proto::mirc::strip(&r.topic))
                        .xalign(0.0)
                        .hexpand(true)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .css_classes(["chanctl-current"])
                        .build(),
                );
                list.append(&gtk::ListBoxRow::builder().child(&row_box).build());
                shown.borrow_mut().push(r.channel.clone());
            }

            let total = app.chanlist_total.get();
            let loading = app.chanlist_loading.get();
            status.set_text(&match (rows.len(), total, loading) {
                (0, _, true) => "fetching the channel list…".to_string(),
                (0, 0, false) => {
                    "no channel list yet — press Refresh to ask the network".to_string()
                }
                (0, _, false) => "nothing matched".to_string(),
                (n, t, false) => format!("showing {n} of {t}"),
                (n, t, true) => format!("showing {n} of {t} — still fetching…"),
            });
        }
    };
    redraw();

    // Live results arrive as frames; redraw when they do.
    let observer_id = {
        let redraw = redraw.clone();
        app.subscribe(Rc::new(move |events: &[lurker_client::StoreEvent]| {
            if events.iter().any(|e| {
                matches!(
                    e,
                    lurker_client::StoreEvent::ChanlistResult
                        | lurker_client::StoreEvent::ChanlistState
                )
            }) {
                redraw();
            }
        }))
    };
    {
        let app = app.clone();
        window.connect_close_request(move |_| {
            app.unsubscribe(observer_id);
            glib::Propagation::Proceed
        });
    }

    // Filtering is a server query, not a local pass: we only ever hold the page
    // the server sent, so filtering client-side would hide matches that exist.
    {
        let app = app.clone();
        filter.connect_changed(move |e| {
            app.chanlist_search(network_id, e.text().trim());
        });
    }
    {
        let app = app.clone();
        refresh.connect_clicked(move |_| app.list_channels(network_id));
    }

    let join = {
        let app = app.clone();
        let shown = shown.clone();
        let window = window.clone();
        move |index: usize| {
            let Some(channel) = shown.borrow().get(index).cloned() else { return };
            let key = lurker_proto::BufferKey::new(Some(network_id), &channel);
            app.store.borrow_mut().note_pending_join(key);
            app.send(lurker_proto::ClientVerb::Join { network_id, channel, key: None });
            window.close();
        }
    };
    {
        let join = join.clone();
        list.connect_row_activated(move |_, row| join(row.index() as usize));
    }

    // Enter in the filter joins the first result — the common case is typing
    // enough of a name to bring it to the top.
    {
        let join = join.clone();
        filter.connect_activate(move |_| join(0));
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

    // Show whatever the server already has cached, without forcing a LIST.
    app.chanlist_search(network_id, "");
    window.present();
    filter.grab_focus();
}
