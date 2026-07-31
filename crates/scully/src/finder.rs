// Two keyboard-first finders: message search and the buffer quick-switcher.
//
// Both are deliberately native-desktop rather than web-modal: a real top-level
// window, arrow keys to move, Enter to act, Escape to dismiss, and no scrim.
// The switcher is pure client-side filtering over the store; search goes to
// the server's `search` verb and jumps to a result via `history`/`around`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use lurker_proto::BufferKey;

use crate::app::AppRef;

/// Fuzzy-ish subsequence match used by the quick switcher: every character of
/// `needle` must appear in `haystack` in order. Returns a score where lower is
/// better (a prefix match beats a scattered one), or `None` for no match.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut score = 0;
    let mut pos = 0usize;
    let mut last_hit: Option<usize> = None;
    for ch in needle.to_lowercase().chars() {
        let found = hay[pos..].iter().position(|c| *c == ch)? + pos;
        // Penalise gaps, so contiguous and early matches rank first.
        score += match last_hit {
            Some(prev) => (found - prev) as i32,
            None => found as i32 * 2,
        };
        last_hit = Some(found);
        pos = found + 1;
    }
    Some(score)
}

/// Rank buffers for the quick switcher: fuzzy match on the display name, with
/// unread conversations floated up on an empty query so the switcher opens on
/// something useful.
pub fn rank_buffers(
    query: &str,
    buffers: impl Iterator<Item = (BufferKey, String, i64)>,
) -> Vec<(BufferKey, String)> {
    let mut scored: Vec<(i32, i64, BufferKey, String)> = buffers
        .filter_map(|(key, name, unread)| {
            fuzzy_score(query, &name).map(|s| (s, unread, key, name))
        })
        .collect();
    scored.sort_by(|a, b| {
        if query.is_empty() {
            // No query: most-unread first, then name.
            b.1.cmp(&a.1).then_with(|| a.3.to_lowercase().cmp(&b.3.to_lowercase()))
        } else {
            a.0.cmp(&b.0).then_with(|| a.3.len().cmp(&b.3.len()))
        }
    });
    scored.into_iter().map(|(_, _, k, n)| (k, n)).collect()
}

/// Build a keyboard-driven finder window: an entry on top, a list below.
///
/// `on_activate` receives the selected row index. Escape closes; Up/Down move
/// the selection without leaving the entry, which is what makes it usable
/// without touching the mouse.
fn finder_window(
    app: &AppRef,
    title: &str,
    placeholder: &str,
    width: i32,
) -> (gtk::Window, gtk::Entry, gtk::ListBox) {
    let mut builder = gtk::Window::builder()
        .title(title)
        .default_width(width)
        .default_height(460)
        .destroy_with_parent(true);
    if let Some(parent) = app.gtk_app.active_window() {
        builder = builder.transient_for(&parent);
    }
    let window = builder.build();
    crate::fit_to_screen(&window);
    window.add_css_class("finder");

    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .css_classes(["finder-entry"])
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["finder-list"])
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .vexpand(true)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&entry);
    outer.append(&scroll);
    window.set_child(Some(&outer));

    // Escape closes from anywhere in the window.
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

    (window, entry, list)
}

/// Move the list selection by `delta`, wrapping at the ends.
fn move_selection(list: &gtk::ListBox, delta: i32) {
    let count = {
        let mut n = 0;
        let mut child = list.first_child();
        while let Some(w) = child {
            child = w.next_sibling();
            n += 1;
        }
        n
    };
    if count == 0 {
        return;
    }
    let current = list.selected_row().map(|r| r.index()).unwrap_or(-1);
    let next = (current + delta).rem_euclid(count);
    if let Some(row) = list.row_at_index(next) {
        list.select_row(Some(&row));
        row.grab_focus();
        // Keep the entry focused for typing; scrolling is enough feedback.
        list.set_focusable(false);
    }
}

/// The quick switcher: type to filter buffers, Enter to jump. Ctrl+K.
pub fn open_switcher(app: &AppRef, on_pick: impl Fn(&BufferKey) + 'static) {
    let (window, entry, list) =
        finder_window(app, "Go to buffer", "Jump to a channel or conversation…", 460);

    let rows: Rc<RefCell<Vec<BufferKey>>> = Rc::new(RefCell::new(Vec::new()));
    let on_pick = Rc::new(on_pick);

    let refresh = {
        let app = app.clone();
        let list = list.clone();
        let rows = rows.clone();
        move |query: &str| {
            while let Some(child) = list.first_child() {
                if let Ok(row) = child.clone().downcast::<gtk::ListBoxRow>() {
                    list.remove(&row);
                } else {
                    break;
                }
            }
            let store = app.store.borrow();
            let ranked = rank_buffers(
                query,
                store.buffers.values().map(|b| {
                    (b.key.clone(), b.display_name.clone(), b.unread)
                }),
            );
            drop(store);

            let mut keys = Vec::new();
            for (key, name) in ranked.into_iter().take(50) {
                let store = app.store.borrow();
                let net = key
                    .network_id
                    .and_then(|id| store.networks.get(&id))
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                let unread = store.buffer(&key).map(|b| b.unread).unwrap_or(0);
                drop(store);

                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let label = gtk::Label::builder()
                    .xalign(0.0)
                    .label(&name)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let net_label = gtk::Label::builder()
                    .label(&net)
                    .css_classes(["finder-dim"])
                    .build();
                row_box.append(&label);
                if unread > 0 {
                    row_box.append(
                        &gtk::Label::builder()
                            .label(unread.to_string())
                            .css_classes(["badge"])
                            .build(),
                    );
                }
                row_box.append(&net_label);
                list.append(&gtk::ListBoxRow::builder().child(&row_box).build());
                keys.push(key);
            }
            *rows.borrow_mut() = keys;
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };
    refresh("");

    {
        let refresh = refresh.clone();
        entry.connect_changed(move |e| refresh(&e.text()));
    }

    // Up/Down navigate the list while the entry keeps focus.
    {
        let list = list.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gtk::gdk::Key::Down => {
                move_selection(&list, 1);
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Up => {
                move_selection(&list, -1);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        entry.add_controller(keys);
    }

    let activate = {
        let list = list.clone();
        let rows = rows.clone();
        let window = window.clone();
        let on_pick = on_pick.clone();
        move || {
            let idx = list.selected_row().map(|r| r.index()).unwrap_or(0);
            if let Some(key) = rows.borrow().get(idx as usize) {
                on_pick(key);
            }
            window.close();
        }
    };
    {
        let activate = activate.clone();
        entry.connect_activate(move |_| activate());
    }
    {
        let activate = activate.clone();
        list.connect_row_activated(move |_, _| activate());
    }

    window.present();
    entry.grab_focus();
}

/// Message search: query the server, list matches, Enter jumps to the message
/// in its buffer (a `history`/`around` fetch, which detaches until the reader
/// returns to the present).
pub fn open_search(
    app: &AppRef,
    scope: Option<BufferKey>,
    on_jump: impl Fn(&BufferKey, i64) + 'static,
) {
    let scope_label = scope
        .as_ref()
        .map(|k| format!("Search in {}", k.target))
        .unwrap_or_else(|| "Search all conversations".to_string());
    let (window, entry, list) =
        finder_window(app, "Search messages", &scope_label, 720);

    let rows: Rc<RefCell<Vec<(BufferKey, i64)>>> = Rc::new(RefCell::new(Vec::new()));
    let on_jump = Rc::new(on_jump);

    // Submit on Enter rather than per-keystroke: search is a server round trip
    // and the flood limiter is real (§4.2).
    {
        let app = app.clone();
        let scope = scope.clone();
        entry.connect_activate(move |e| {
            let q = e.text().to_string();
            if q.trim().len() >= 2 {
                app.search(&q, scope.as_ref());
            }
        });
    }

    // Render results when they land.
    let render = {
        let app = app.clone();
        let list = list.clone();
        let rows = rows.clone();
        move || {
            while let Some(child) = list.first_child() {
                if let Ok(row) = child.clone().downcast::<gtk::ListBoxRow>() {
                    list.remove(&row);
                } else {
                    break;
                }
            }
            let results = app.search_results.borrow().clone();
            let store = app.store.borrow();
            let mut keys = Vec::new();
            for event in results.iter() {
                let Some(id) = event.id else { continue };
                let key = event.buffer_key();
                let where_ = store
                    .buffer(&key)
                    .map(|b| b.display_name.clone())
                    .unwrap_or_else(|| key.target.clone());
                let who = event.nick.clone().unwrap_or_default();
                let text = lurker_proto::mirc::strip(event.text.as_deref().unwrap_or(""));

                let row_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
                let head = gtk::Label::builder()
                    .xalign(0.0)
                    .label(format!("{where_} — {who}"))
                    .css_classes(["finder-dim"])
                    .build();
                let body = gtk::Label::builder()
                    .xalign(0.0)
                    .label(text)
                    .wrap(true)
                    .lines(2)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                row_box.append(&head);
                row_box.append(&body);
                list.append(&gtk::ListBoxRow::builder().child(&row_box).build());
                keys.push((key, id));
            }
            drop(store);
            *rows.borrow_mut() = keys;
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };

    // Subscribe for result frames while the window is open.
    {
        let render = render.clone();
        let id = app.subscribe(Rc::new(move |events: &[lurker_client::StoreEvent]| {
            if events.iter().any(|e| matches!(e, lurker_client::StoreEvent::SearchResults)) {
                render();
            }
        }));
        let app = app.clone();
        window.connect_close_request(move |_| {
            app.unsubscribe(id);
            glib::Propagation::Proceed
        });
    }

    {
        let list = list.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gtk::gdk::Key::Down => {
                move_selection(&list, 1);
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Up => {
                move_selection(&list, -1);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        entry.add_controller(keys);
    }

    {
        let rows = rows.clone();
        let window = window.clone();
        let on_jump = on_jump.clone();
        list.connect_row_activated(move |list, _| {
            let idx = list.selected_row().map(|r| r.index()).unwrap_or(0);
            if let Some((key, id)) = rows.borrow().get(idx as usize) {
                on_jump(key, *id);
            }
            window.close();
        });
    }

    window.present();
    entry.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequences_and_ranks_prefixes_first() {
        assert!(fuzzy_score("lur", "#lurker").is_some());
        assert!(fuzzy_score("lkr", "#lurker").is_some(), "scattered still matches");
        assert!(fuzzy_score("xyz", "#lurker").is_none());
        // A tighter, earlier match scores lower (better).
        let tight = fuzzy_score("lur", "lurker").unwrap();
        let loose = fuzzy_score("lur", "a-l-u-r").unwrap();
        assert!(tight < loose, "tight {tight} should beat loose {loose}");
    }

    #[test]
    fn fuzzy_is_case_insensitive_and_empty_matches_all() {
        assert!(fuzzy_score("LUR", "#lurker").is_some());
        assert_eq!(fuzzy_score("", "#anything"), Some(0));
    }

    #[test]
    fn empty_query_ranks_by_unread_then_name() {
        let buffers = vec![
            (BufferKey::new(Some(1), "#quiet"), "#quiet".to_string(), 0),
            (BufferKey::new(Some(1), "#busy"), "#busy".to_string(), 42),
            (BufferKey::new(Some(1), "#some"), "#some".to_string(), 5),
        ];
        let ranked = rank_buffers("", buffers.into_iter());
        assert_eq!(ranked[0].1, "#busy", "most unread first");
        assert_eq!(ranked[1].1, "#some");
    }

    #[test]
    fn a_query_ranks_by_match_quality_not_unread() {
        let buffers = vec![
            (BufferKey::new(Some(1), "#other"), "#other".to_string(), 99),
            (BufferKey::new(Some(1), "#lurker"), "#lurker".to_string(), 0),
        ];
        let ranked = rank_buffers("lurk", buffers.into_iter());
        assert_eq!(ranked.len(), 1, "non-matching buffers are filtered out");
        assert_eq!(ranked[0].1, "#lurker");
    }
}
