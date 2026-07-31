// The channel control dialog (mIRC's "channel central").
//
// Opened by double-clicking in a channel. Shows the topic rendered with mIRC
// colours, an editor for it, and the channel's mode switches — built from what
// THIS network's server actually advertised in RPL_ISUPPORT (parsed out of
// the `:server:` log; `lurker_proto::isupport`), so an ergo network and a
// UnrealIRCd network get different dialogs. Prefix modes (qaohv) are per-user
// and live in the nicklist menu, not here.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use lurker_proto::{isupport, mirc, BufferKey, ClientVerb};

use crate::app::AppRef;

pub fn open(app: &AppRef, key: &BufferKey) {
    let Some(network_id) = key.network_id else { return };
    if !key.is_channel() {
        return;
    }
    let store = app.store.borrow();
    let Some(buf) = store.buffer(key) else { return };
    let channel = buf.display_name.clone();
    let topic = buf.topic.clone().unwrap_or_default();
    let modes = buf.modes.clone().unwrap_or_default();

    // ISUPPORT for this network — from the per-network cache the store fills as
    // the 005 burst arrives, so it is complete even after the server-buffer
    // ring evicted the original lines. Fall back to re-parsing the log (and
    // then the RFC baseline) only if the cache never saw a 005.
    let cached = store.networks.get(&network_id).map(|n| n.isupport.clone());
    let mut support = match cached {
        Some(s) if s.advertised() => s,
        _ => isupport::parse(
            store
                .buffer(&BufferKey::server(network_id))
                .map(|log| log.events.iter().filter_map(|e| e.text.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter(),
        ),
    };
    drop(store);

    let (set_letters, _params) = isupport::parse_current(&modes);

    // Safety net: any currently-SET letter that ISUPPORT didn't classify as a
    // list/param/prefix mode is a flag mode we should still show (so the dialog
    // can never hide an enabled mode). Append it to the flag set.
    for &letter in &set_letters {
        let known = support.list_modes.contains(letter)
            || support.param_modes.contains(letter)
            || support.set_param_modes.contains(letter)
            || support.flag_modes.contains(letter)
            || support.prefix_modes.contains(letter);
        if !known {
            support.flag_modes.push(letter);
        }
    }

    let mut builder = gtk::Window::builder()
        .title(format!("{channel} — channel control"))
        .default_width(560)
        .default_height(620)
        .modal(false)
        .destroy_with_parent(true);
    // Anchor to the active window so the WM opens it in front, not off-screen.
    if let Some(parent) = app.gtk_app.active_window() {
        builder = builder.transient_for(&parent);
    }
    let window = builder.build();
    crate::fit_to_screen(&window);
    window.add_css_class("chanctl");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(14);
    outer.set_margin_bottom(14);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    // ── Topic ──
    let topic_heading = gtk::Label::builder()
        .xalign(0.0)
        .label("Topic")
        .css_classes(["chanctl-heading"])
        .build();
    outer.append(&topic_heading);

    // Rendered with colour support; updates live as the editor changes.
    let topic_view = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .use_markup(true)
        .css_classes(["chanctl-topic"])
        .build();
    topic_view.set_markup(&mirc::to_pango_markup(&topic));
    outer.append(&topic_view);

    let topic_edit = gtk::Entry::builder().text(&topic).hexpand(true).build();
    if let Some(max) = support.topic_len {
        topic_edit.set_max_length(max as i32);
    }
    let set_topic = gtk::Button::builder().label("Set topic").css_classes(["chanctl-set"]).build();
    let topic_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    topic_row.append(&topic_edit);
    topic_row.append(&set_topic);
    outer.append(&topic_row);

    {
        // Live preview: the editor's raw text (mIRC codes and all) renders
        // above as it will appear.
        let preview = topic_view.clone();
        topic_edit.connect_changed(move |e| {
            preview.set_markup(&mirc::to_pango_markup(&e.text()));
        });
    }
    {
        let app = app.clone();
        let topic_edit = topic_edit.clone();
        let channel = channel.clone();
        set_topic.connect_clicked(move |_| {
            // No typed WS verb for TOPIC; raw is the documented escape hatch.
            app.send(ClientVerb::Raw {
                network_id,
                line: format!("TOPIC {channel} :{}", topic_edit.text()),
            });
        });
    }

    // ── Flag modes (class D): plain switches ──
    let flags_heading = gtk::Label::builder()
        .xalign(0.0)
        .label(if support.advertised() {
            "Modes (as advertised by this server)".to_string()
        } else {
            "Modes (server did not advertise; showing the RFC set)".to_string()
        })
        .css_classes(["chanctl-heading"])
        .build();
    outer.append(&flags_heading);

    // Every advertised flag mode, laid out in a wrapping multi-column grid so
    // a server like Libera (~20 flag modes) shows them all at once rather than
    // four at a time in a cramped scroller. A FlowBox reflows to the width.
    let flags = gtk::FlowBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .selection_mode(gtk::SelectionMode::None)
        // One column minimum, not two: a phone is ~720px and these labels run
        // to "No external messages (+n)". Forcing two columns made the row
        // wider than the window, and the dialog scrolls vertically only — so
        // the overflow was simply clipped rather than reachable. At one
        // minimum the box reflows to whatever actually fits.
        .min_children_per_line(1)
        .max_children_per_line(3)
        .column_spacing(10)
        .row_spacing(2)
        .homogeneous(true)
        .build();
    let mut flag_letters: Vec<char> = support.flag_modes.chars().collect();
    flag_letters.sort_unstable();
    for letter in flag_letters {
        // Prefix modes are per-user; never show them as channel switches even
        // if a server lists them oddly.
        if support.prefix_modes.contains(letter) {
            continue;
        }
        let label = match isupport::mode_label(letter) {
            Some(text) => format!("{text}  (+{letter})"),
            None => format!("Mode +{letter}"),
        };
        let check = gtk::CheckButton::builder()
            .label(&label)
            .active(set_letters.contains(&letter))
            .build();
        let app = app.clone();
        let channel = channel.clone();
        check.connect_toggled(move |c| {
            let sign = if c.is_active() { '+' } else { '-' };
            app.send(ClientVerb::Raw {
                network_id,
                line: format!("MODE {channel} {sign}{letter}"),
            });
        });
        flags.append(&check);
    }
    outer.append(&flags);

    // ── Parameter modes (classes B and C): entry + set/clear ──
    let param_heading = gtk::Label::builder()
        .xalign(0.0)
        .label("Modes with a value")
        .css_classes(["chanctl-heading"])
        .build();
    outer.append(&param_heading);
    let params_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let param_letters: Vec<char> = support
        .param_modes
        .chars()
        .chain(support.set_param_modes.chars())
        .filter(|c| !support.prefix_modes.contains(*c))
        .collect();
    for letter in param_letters {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label_text = match isupport::mode_label(letter) {
            Some(text) => format!("{text}  (+{letter})"),
            None => format!("mode +{letter}"),
        };
        // Ellipsize and a smaller entry, so the row shrinks to a narrow screen
        // instead of pushing the buttons off the edge.
        let label = gtk::Label::builder()
            .xalign(0.0)
            .label(&label_text)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let value = gtk::Entry::builder().width_chars(6).hexpand(false).build();
        let set = gtk::Button::builder().label("Set").build();
        let clear = gtk::Button::builder().label("Clear").build();
        row.append(&label);
        row.append(&value);
        row.append(&set);
        row.append(&clear);
        {
            let app = app.clone();
            let channel = channel.clone();
            let value = value.clone();
            set.connect_clicked(move |_| {
                let v = value.text();
                if v.is_empty() {
                    return;
                }
                app.send(ClientVerb::Raw {
                    network_id,
                    line: format!("MODE {channel} +{letter} {v}"),
                });
            });
        }
        {
            let app = app.clone();
            let channel = channel.clone();
            let value = value.clone();
            clear.connect_clicked(move |_| {
                // `-k` historically requires the key as its argument; `*` is
                // the accepted wildcard where the key isn't known.
                let arg = if letter == 'k' {
                    let v = value.text();
                    if v.is_empty() { " *".to_string() } else { format!(" {v}") }
                } else {
                    String::new()
                };
                app.send(ClientVerb::Raw {
                    network_id,
                    line: format!("MODE {channel} -{letter}{arg}"),
                });
            });
        }
        params_box.append(&row);
    }
    outer.append(&params_box);

    // ── List modes (class A): request and DISPLAY the list ──
    //
    // The reply numerics (367/368 ban, 348/349 except, 346/347 invite) are
    // relayed into the `:server:` log as text. Showing them only there made
    // these buttons look broken — you clicked and nothing happened anywhere
    // you were looking. Now the dialog collects and renders them in place.
    let lists_heading = gtk::Label::builder()
        .xalign(0.0)
        .label("Lists")
        .css_classes(["chanctl-heading"])
        .build();
    outer.append(&lists_heading);

    let lists_row = gtk::FlowBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(1)
        .max_children_per_line(3)
        .column_spacing(8)
        .row_spacing(4)
        .build();
    let list_status = gtk::Label::builder()
        .xalign(0.0)
        .label("Pick a list to load.")
        .css_classes(["chanctl-current"])
        .build();
    let list_box = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["chanctl-list"])
        .build();
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list_box)
        .min_content_height(120)
        .build();

    // Collection state, shared with the store observer below.
    #[derive(Clone)]
    struct Collecting {
        /// Only lines newer than this server-log id belong to this request.
        after_id: i64,
        count: usize,
    }
    let collecting: Rc<RefCell<Option<Collecting>>> = Rc::new(RefCell::new(None));

    let mut list_letters: Vec<char> = support.list_modes.chars().collect();
    if let Some(e) = support.except_mode {
        if !list_letters.contains(&e) {
            list_letters.push(e);
        }
    }
    if let Some(i) = support.invex_mode {
        if !list_letters.contains(&i) {
            list_letters.push(i);
        }
    }
    for letter in list_letters {
        let label = match isupport::mode_label(letter) {
            Some(text) => format!("{text} (+{letter})"),
            None => format!("+{letter} list"),
        };
        let button = gtk::Button::builder().label(&label).build();
        let app = app.clone();
        let channel = channel.clone();
        let collecting = collecting.clone();
        let list_box = list_box.clone();
        let list_status = list_status.clone();
        button.connect_clicked(move |_| {
            // Clear previous results.
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            // Only count replies that arrive AFTER this request, so a second
            // click doesn't re-read the previous list's lines.
            let after_id = app
                .store
                .borrow()
                .buffer(&BufferKey::server(network_id))
                .and_then(|b| b.newest_id)
                .unwrap_or(0);
            *collecting.borrow_mut() = Some(Collecting { after_id, count: 0 });
            list_status.set_label(&format!("loading +{letter} list…"));
            app.send(ClientVerb::Raw {
                network_id,
                line: format!("MODE {channel} +{letter}"),
            });
        });
        #[cfg(debug_assertions)]
        if std::env::var("SCULLY_TEST_LIST").is_ok_and(|v| v == letter.to_string()) {
            // Dev-build hook: auto-click this list button once the dialog is
            // mapped, so list collection can be exercised headlessly (GTK4
            // rejects synthetic clicks under the nested test display).
            let button = button.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
                button.emit_clicked();
            });
        }
        lists_row.append(&button);
    }
    outer.append(&lists_row);
    outer.append(&list_status);
    outer.append(&list_scroll);

    // ── Voice-call join policy (Lurker #680) ──
    // Only rendered when the server offers voice at all. Reading the policy is
    // open to any member; changing it is ops-only, mirrored from the server so
    // a non-op sees the current setting but cannot edit it.
    if app.voice_enabled.get() {
        let my_rank = {
            let store = app.store.borrow();
            let my_nick = store.networks.get(&network_id).and_then(|n| n.nick.clone());
            my_nick
                .and_then(|nick| {
                    store
                        .buffer(key)
                        .and_then(|b| b.members.get(&lurker_proto::fold(&nick)))
                        .map(|m| crate::nickmenu::Rank::from_mode(m.modes.first().map(String::as_str)))
                })
                .unwrap_or(crate::nickmenu::Rank::None)
        };
        let may_set = crate::callmod::can_set_policy(my_rank);

        let call_heading = gtk::Label::builder()
            .xalign(0.0)
            .label("Voice call — who may join")
            .css_classes(["chanctl-heading"])
            .build();
        outer.append(&call_heading);

        let call_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let labels: Vec<&str> = crate::callmod::MinJoin::ALL.iter().map(|m| m.label()).collect();
        let policy_drop = gtk::DropDown::from_strings(&labels);
        policy_drop.set_hexpand(true);
        policy_drop.set_sensitive(false); // until the current value loads
        let policy_status = gtk::Label::builder()
            .xalign(0.0)
            .label(if may_set { "loading…" } else { "loading… (operators may change this)" })
            .css_classes(["chanctl-current"])
            .build();
        call_row.append(&policy_drop);
        outer.append(&call_row);
        outer.append(&policy_status);

        // Load the current policy, then arm the control. `applying` suppresses
        // the change handler while we set the initial selection, so loading a
        // value never writes it straight back to the server.
        let applying = Rc::new(std::cell::Cell::new(true));
        {
            let drop_w = policy_drop.clone();
            let status = policy_status.clone();
            let applying = applying.clone();
            app.fetch_call_policy(key, move |res| match res {
                Ok(mode) => {
                    let idx = crate::callmod::MinJoin::ALL
                        .iter()
                        .position(|m| *m == mode)
                        .unwrap_or(0) as u32;
                    applying.set(true);
                    drop_w.set_selected(idx);
                    applying.set(false);
                    drop_w.set_sensitive(may_set);
                    status.set_text(if may_set {
                        "operators can change this"
                    } else {
                        "only operators can change this"
                    });
                }
                Err(e) => status.set_text(&format!("could not read policy — {e}")),
            });
        }

        if may_set {
            let app = app.clone();
            let key = key.clone();
            let status = policy_status.clone();
            policy_drop.connect_selected_notify(move |d| {
                if applying.get() {
                    return;
                }
                let Some(mode) = crate::callmod::MinJoin::ALL.get(d.selected() as usize).copied()
                else {
                    return;
                };
                let status = status.clone();
                app.set_call_policy(&key, mode, move |res| match res {
                    Ok(applied) => status.set_text(&format!("join policy: {}", applied.label())),
                    Err(e) => status.set_text(&format!("could not set policy — {e}")),
                });
            });
        }
    }

    // ── Footer: current mode string + refresh ──
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let current = gtk::Label::builder()
        .xalign(0.0)
        .label(if modes.is_empty() {
            "current modes: (unknown — refresh)".to_string()
        } else {
            format!("current modes: {modes}")
        })
        .hexpand(true)
        .css_classes(["chanctl-current"])
        .build();
    let refresh = gtk::Button::builder().label("Refresh").build();
    {
        let app = app.clone();
        let channel = channel.clone();
        refresh.connect_clicked(move |_| {
            app.send(ClientVerb::Raw { network_id, line: format!("MODE {channel}") });
        });
    }
    footer.append(&current);
    footer.append(&refresh);
    outer.append(&footer);

    // Track live mode updates, and collect list-mode replies, while the
    // dialog is open.
    {
        let current = current.clone();
        let app_ref = app.clone();
        let key = key.clone();
        let server_key = BufferKey::server(network_id);
        let channel_name = channel.clone();
        let collecting = collecting.clone();
        let list_box = list_box.clone();
        let list_status = list_status.clone();
        let id = app.subscribe(Rc::new(move |events: &[lurker_client::StoreEvent]| {
            for ev in events {
                let lurker_client::StoreEvent::BufferChanged(k) = ev else { continue };

                // Live channel-mode refresh.
                if *k == key {
                    if let Some(modes) =
                        app_ref.store.borrow().buffer(&key).and_then(|b| b.modes.clone())
                    {
                        current.set_label(&format!("current modes: {modes}"));
                    }
                }

                // List replies arrive in the network's server log.
                if *k != server_key {
                    continue;
                }
                let Some(state) = collecting.borrow().clone() else { continue };

                // Scan only rows newer than the request, parse each as a list
                // line, and render entries as they stream in.
                let mut newest_seen = state.after_id;
                let mut added = 0usize;
                let mut finished = false;
                {
                    let store = app_ref.store.borrow();
                    let Some(log) = store.buffer(&server_key) else { continue };
                    for event in log.events.iter() {
                        let Some(id) = event.id else { continue };
                        if id <= state.after_id {
                            continue;
                        }
                        newest_seen = newest_seen.max(id);
                        let Some(text) = &event.text else { continue };
                        match crate::modelist::parse_line(&channel_name, text) {
                            crate::modelist::ListLine::Entry { mask, setter, when } => {
                                let label = gtk::Label::builder()
                                    .xalign(0.0)
                                    .label(crate::modelist::describe(
                                        &mask,
                                        setter.as_deref(),
                                        when.as_deref(),
                                    ))
                                    .selectable(true)
                                    .build();
                                list_box.append(
                                    &gtk::ListBoxRow::builder().child(&label).build(),
                                );
                                added += 1;
                            }
                            crate::modelist::ListLine::End => finished = true,
                            crate::modelist::ListLine::Other => {}
                        }
                    }
                }

                let total = state.count + added;
                if finished {
                    list_status.set_label(&if total == 0 {
                        "list is empty".to_string()
                    } else {
                        format!("{total} entr{}", if total == 1 { "y" } else { "ies" })
                    });
                    *collecting.borrow_mut() = None;
                } else {
                    // Advance the watermark so already-rendered rows are not
                    // added again on the next change.
                    *collecting.borrow_mut() =
                        Some(Collecting { after_id: newest_seen, count: total });
                    if total > 0 {
                        list_status.set_label(&format!("{total}…"));
                    }
                }
            }
        }));
        let app = app.clone();
        window.connect_close_request(move |_| {
            app.unsubscribe(id);
            glib::Propagation::Proceed
        });
    }

    // One scroll for the whole dialog: no section can starve another of
    // vertical space, so all advertised modes are reachable.
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&outer)
        .build();
    window.set_child(Some(&scroll));
    window.present();
}
