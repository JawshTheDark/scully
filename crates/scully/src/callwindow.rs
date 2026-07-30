//! The call window: a lightweight top-level window for one voice call, in the
//! same spirit as a channel popout — no scrim modal, just a real window with
//! the participants, a mute toggle, and hang up. Feature-gated with `voice`.
//!
//! It owns the [`Call`] for its lifetime: closing the window (or hang up) drops
//! the call, which stops the microphone and speakers and closes the room.
#![cfg(feature = "voice")]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::app::AppRef;
use crate::voice::{Call, CallEvent};

/// Open a call window bound to an already-connected [`Call`]. `title` is the
/// human label for what is being called (e.g. `#dev on Libera`).
pub fn open(app: &AppRef, call: Call, title: &str) {
    let window = gtk::ApplicationWindow::builder()
        .application(&app.gtk_app)
        .title(format!("Call — {title}"))
        .default_width(320)
        .default_height(360)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);

    let heading = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["title-3"])
        .build();
    let status = gtk::Label::builder()
        .label("Connecting…")
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();

    let participants = gtk::ListBox::new();
    participants.set_selection_mode(gtk::SelectionMode::None);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&participants)
        .build();

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let mute = gtk::ToggleButton::builder().label("Mute").build();
    let hangup = gtk::Button::builder()
        .label("Leave")
        .css_classes(["destructive-action"])
        .build();
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    controls.append(&mute);
    controls.append(&spacer);
    controls.append(&hangup);

    root.append(&heading);
    root.append(&status);
    root.append(&scroller);
    root.append(&controls);
    window.set_child(Some(&root));

    // The call is shared so the control handlers and the close handler can reach
    // it; the last of these to drop takes the call (and its audio) down with it.
    let call = Rc::new(RefCell::new(Some(call)));

    // Mute toggle → feed silence / resume.
    {
        let call = call.clone();
        mute.connect_toggled(move |btn| {
            if let Some(c) = call.borrow().as_ref() {
                c.set_muted(btn.is_active());
            }
            btn.set_label(if btn.is_active() { "Unmute" } else { "Mute" });
        });
    }

    // Leave → hang up and close. `close` triggers the window's close-request,
    // which drops the call below.
    {
        let window = window.clone();
        let call = call.clone();
        hangup.connect_clicked(move |_| {
            if let Some(c) = call.borrow().as_ref() {
                c.hangup();
            }
            window.close();
        });
    }

    // Closing the window ends the call: drop it to stop mic/speakers and room.
    {
        let call = call.clone();
        window.connect_close_request(move |_| {
            if let Some(c) = call.borrow().as_ref() {
                c.hangup();
            }
            *call.borrow_mut() = None; // drops the Call → cpal streams stop
            glib::Propagation::Proceed
        });
    }

    // Drain call events into the UI. The receiver is cloned out of the call
    // before we hand it to the window; the future runs on the glib main loop.
    let events = call.borrow().as_ref().unwrap().events.clone();
    let roster: Rc<RefCell<BTreeSet<String>>> = Rc::new(RefCell::new(BTreeSet::new()));
    // Our own identity, so the list can show you as well as the other people —
    // "who is in this call" has to include yourself to make sense.
    let me: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    // Who is talking right now; kept so a join/leave redraw doesn't wipe it.
    let speaking: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let status = status.clone();
        let participants = participants.clone();
        let roster = roster.clone();
        let me = me.clone();
        let speaking = speaking.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            let draw = |list: &gtk::ListBox| {
                redraw_roster(list, &me.borrow(), &roster.borrow(), &speaking.borrow());
            };
            while let Ok(ev) = events.recv().await {
                match ev {
                    CallEvent::Connected { you, participants: already } => {
                        status.set_text("Connected");
                        *me.borrow_mut() = you;
                        roster.borrow_mut().extend(already);
                        draw(&participants);
                    }
                    CallEvent::ParticipantJoined(id) => {
                        roster.borrow_mut().insert(id);
                        draw(&participants);
                    }
                    CallEvent::ParticipantLeft(id) => {
                        roster.borrow_mut().remove(&id);
                        speaking.borrow_mut().retain(|s| s != &id);
                        draw(&participants);
                    }
                    CallEvent::Speaking(active) => {
                        *speaking.borrow_mut() = active;
                        draw(&participants);
                    }
                    CallEvent::Ended(why) => {
                        status.set_text(&format!("Call ended — {why}"));
                        window.close();
                        break;
                    }
                    CallEvent::Failed(err) => {
                        status.set_text(&format!("Failed — {err}"));
                    }
                }
            }
        });
    }

    window.present();
}

/// Rebuild the participant rows: you first (marked), then everyone else in
/// name order. Whoever currently has the floor gets a speaker icon, so the list
/// answers both "who is here" and "who is talking".
fn redraw_roster(
    list: &gtk::ListBox,
    me: &str,
    roster: &BTreeSet<String>,
    speaking: &[String],
) {
    while let Some(row) = list.first_child() {
        list.remove(&row);
    }

    let mut rows: Vec<(String, bool)> = Vec::with_capacity(roster.len() + 1);
    if !me.is_empty() {
        rows.push((format!("{me} (you)"), speaking.iter().any(|s| s == me)));
    }
    for id in roster {
        // The local participant can also appear in the remote list on some
        // servers; don't render them twice.
        if id == me {
            continue;
        }
        rows.push((id.clone(), speaking.iter().any(|s| s == id)));
    }

    if rows.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label("Connecting…")
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
        );
        return;
    }

    for (name, talking) in rows {
        // U+2007 (figure space) keeps non-speaking names aligned under the icon.
        let label = if talking { format!("\u{1F50A} {name}") } else { format!("\u{2007}\u{2007} {name}") };
        list.append(&gtk::Label::builder().label(label).xalign(0.0).build());
    }
}
