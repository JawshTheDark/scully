// The buffer-list context menu: pure item selection plus a gio::Menu builder.
//
// Which items appear depends on the buffer's kind and state — you can leave a
// channel but not a DM, rejoin a parted channel, unpin one that's pinned — so
// the selection logic is plain data and unit-tested without a display, the same
// split as nickmenu.rs. The window renders the model and dispatches the ids.

use gtk::gio;
use gtk::prelude::*;

/// The buffer the menu was opened on, reduced to just what changes the items.
#[derive(Clone, Copy, Debug)]
pub struct BufContext {
    pub is_channel: bool,
    pub is_dm: bool,
    /// Only a buffer that exists in the store can be popped out.
    pub in_store: bool,
    /// Channels only: joined now, vs parted-but-still-open (offer Rejoin).
    pub joined: bool,
    pub pinned: bool,
}

/// A menu entry. `Separator` becomes a gio menu section boundary, which GTK
/// renders as a divider. Ids double as GAction string targets and must
/// round-trip through the window's dispatch (see `id_is_known`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Item { id: &'static str, label: &'static str },
    Separator,
}

const fn item(id: &'static str, label: &'static str) -> Entry {
    Entry::Item { id, label }
}

/// The items for a buffer, tailored to kind + state. Pure.
///
/// Order: the safe, common actions first (pop out, mark read, pin), then a
/// divider, then the lifecycle actions that change membership (leave / rejoin /
/// close) so a destructive click is never adjacent to an everyday one.
pub fn entries(cx: &BufContext) -> Vec<Entry> {
    let mut v = Vec::new();
    if cx.in_store {
        v.push(item("popout", "Pop out"));
    }
    v.push(item("read", "Mark as read"));
    if cx.is_channel || cx.is_dm {
        v.push(if cx.pinned { item("unpin", "Unpin") } else { item("pin", "Pin to top") });
    }
    if cx.is_channel {
        v.push(Entry::Separator);
        v.push(if cx.joined {
            item("part", "Leave channel")
        } else {
            item("join", "Rejoin channel")
        });
        v.push(item("close", "Close"));
    } else if cx.is_dm {
        v.push(Entry::Separator);
        v.push(item("close", "Close"));
    }
    v
}

/// Whether an id is one this menu can emit — the dispatch side asserts on this
/// so a renamed id can't silently become a dead click.
pub fn id_is_known(id: &str) -> bool {
    matches!(id, "popout" | "read" | "pin" | "unpin" | "part" | "join" | "close")
}

/// Build the gio menu model for `cx`. Separators split items into sections.
pub fn menu_model(cx: &BufContext) -> gio::Menu {
    let root = gio::Menu::new();
    let mut section = gio::Menu::new();
    for entry in entries(cx) {
        match entry {
            Entry::Separator => {
                if section.n_items() > 0 {
                    root.append_section(None, &section);
                    section = gio::Menu::new();
                }
            }
            Entry::Item { id, label } => {
                section.append_item(&gio::MenuItem::new(
                    Some(label),
                    Some(&format!("buf.cmd::{id}")),
                ));
            }
        }
    }
    if section.n_items() > 0 {
        root.append_section(None, &section);
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(cx: &BufContext) -> Vec<&'static str> {
        entries(cx)
            .into_iter()
            .filter_map(|e| match e {
                Entry::Item { id, .. } => Some(id),
                Entry::Separator => None,
            })
            .collect()
    }

    const CHAN: BufContext =
        BufContext { is_channel: true, is_dm: false, in_store: true, joined: true, pinned: false };

    #[test]
    fn joined_channel_offers_leave_and_close_not_rejoin() {
        let got = ids(&CHAN);
        assert!(got.contains(&"popout"));
        assert!(got.contains(&"read"));
        assert!(got.contains(&"part"), "joined channel can leave");
        assert!(got.contains(&"close"));
        assert!(!got.contains(&"join"), "already joined — no rejoin");
    }

    #[test]
    fn parted_channel_offers_rejoin_not_leave() {
        let cx = BufContext { joined: false, ..CHAN };
        let got = ids(&cx);
        assert!(got.contains(&"join"), "parted channel can rejoin");
        assert!(!got.contains(&"part"));
        assert!(got.contains(&"close"));
    }

    #[test]
    fn pin_toggles_with_state() {
        assert!(ids(&CHAN).contains(&"pin"));
        assert!(ids(&BufContext { pinned: true, ..CHAN }).contains(&"unpin"));
        assert!(!ids(&BufContext { pinned: true, ..CHAN }).contains(&"pin"));
    }

    #[test]
    fn dm_can_close_and_pin_but_not_leave() {
        let dm =
            BufContext { is_channel: false, is_dm: true, in_store: true, joined: false, pinned: false };
        let got = ids(&dm);
        assert!(got.contains(&"close"));
        assert!(got.contains(&"pin"));
        assert!(!got.contains(&"part"), "a DM is not left");
        assert!(!got.contains(&"join"));
    }

    #[test]
    fn server_and_system_buffers_have_no_lifecycle_actions() {
        // Not a channel, not a DM: popout + read only — nothing to leave, close,
        // or pin.
        let srv = BufContext {
            is_channel: false,
            is_dm: false,
            in_store: true,
            joined: false,
            pinned: false,
        };
        let got = ids(&srv);
        assert_eq!(got, ["popout", "read"]);
        for gone in ["close", "part", "join", "pin", "unpin"] {
            assert!(!got.contains(&gone), "{gone} should not appear on a server/system buffer");
        }
    }

    #[test]
    fn a_shell_buffer_not_in_store_cannot_pop_out() {
        let cx = BufContext { in_store: false, ..CHAN };
        assert!(!ids(&cx).contains(&"popout"));
        // …but still offers the rest.
        assert!(ids(&cx).contains(&"read"));
    }

    #[test]
    fn every_emitted_id_is_known_to_dispatch() {
        for cx in [
            CHAN,
            BufContext { joined: false, ..CHAN },
            BufContext { pinned: true, ..CHAN },
            BufContext { is_channel: false, is_dm: true, ..CHAN },
        ] {
            for id in ids(&cx) {
                assert!(id_is_known(id), "menu emits unknown id {id}");
            }
        }
    }
}
