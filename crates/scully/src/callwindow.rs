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
    {
        let status = status.clone();
        let participants = participants.clone();
        let roster = roster.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = events.recv().await {
                match ev {
                    CallEvent::Connected => status.set_text("Connected"),
                    CallEvent::ParticipantJoined(id) => {
                        roster.borrow_mut().insert(id);
                        redraw_roster(&participants, &roster.borrow(), &[]);
                    }
                    CallEvent::ParticipantLeft(id) => {
                        roster.borrow_mut().remove(&id);
                        redraw_roster(&participants, &roster.borrow(), &[]);
                    }
                    CallEvent::Speaking(active) => {
                        redraw_roster(&participants, &roster.borrow(), &active);
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

/// Rebuild the participant rows. `speaking` identities get a small marker so you
/// can see who has the floor.
fn redraw_roster(list: &gtk::ListBox, roster: &BTreeSet<String>, speaking: &[String]) {
    while let Some(row) = list.first_child() {
        list.remove(&row);
    }
    if roster.is_empty() {
        let row = gtk::Label::builder()
            .label("Just you so far…")
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        list.append(&row);
        return;
    }
    for id in roster {
        let talking = speaking.iter().any(|s| s == id);
        let label = if talking { format!("🔊 {id}") } else { format!("   {id}") };
        let row = gtk::Label::builder().label(label).xalign(0.0).build();
        list.append(&row);
    }
}
