// Tile channels across a display: pick buffers, pick a monitor, and each
// selected conversation opens as a popout in an equal-space grid.
//
// The honest physics: Wayland forbids clients from positioning windows — GTK4
// has no move API at all. So Scully does what a client CAN do (open the
// popouts, size each to its grid cell) and then asks the compositor to place
// them. On KDE that works today through KWin's scripting DBus interface: a
// tiny generated script matches our popouts by caption + app id and sets
// their frame geometry. On other compositors the popouts still open
// cell-sized and placement stays with the shell — degraded, stated, not
// broken.


use gtk::prelude::*;
use gtk::{gio, glib};

use crate::app::AppRef;
use lurker_proto::BufferKey;

pub fn open(app: &AppRef) {
    let Some(parent) = app.gtk_app.windows().into_iter().next() else { return };

    // Tileable buffers: everything with a conversation in it.
    let buffers: Vec<(BufferKey, String)> = {
        let store = app.store.borrow();
        store
            .buffers
            .iter()
            .filter(|(k, _)| k.is_channel() || k.is_dm())
            .map(|(k, b)| {
                let net = k
                    .network_id
                    .and_then(|id| store.networks.get(&id))
                    .map(|n| n.name.clone())
                    .filter(|n| !n.is_empty());
                let label = match net {
                    Some(n) => format!("{}  ({n})", b.display_name),
                    None => b.display_name.clone(),
                };
                (k.clone(), label)
            })
            .collect()
    };
    if buffers.is_empty() {
        return;
    }

    let monitors: Vec<(gtk::gdk::Monitor, String)> = gtk::gdk::Display::default()
        .map(|d| {
            let list = d.monitors();
            (0..list.n_items())
                .filter_map(|i| list.item(i).and_then(|o| o.downcast::<gtk::gdk::Monitor>().ok()))
                .map(|m| {
                    let geo = m.geometry();
                    let name = m
                        .model()
                        .or_else(|| m.connector())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "display".into());
                    let label = format!("{name}  ({}×{})", geo.width(), geo.height());
                    (m, label)
                })
                .collect()
        })
        .unwrap_or_default();
    if monitors.is_empty() {
        return;
    }

    let window = gtk::Window::builder()
        .title("Tile channels")
        .default_width(420)
        .modal(false)
        .transient_for(&parent)
        .destroy_with_parent(true)
        .build();
    crate::fit_to_screen(&window);
    window.add_css_class("chanctl");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(14);
    outer.set_margin_bottom(14);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    outer.append(
        &gtk::Label::builder()
            .label("CHANNELS TO TILE")
            .xalign(0.0)
            .css_classes(["chanctl-heading"])
            .build(),
    );
    let checks_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let mut checks: Vec<(BufferKey, gtk::CheckButton)> = Vec::new();
    for (key, label) in &buffers {
        let cb = gtk::CheckButton::builder().label(label).build();
        checks_box.append(&cb);
        checks.push((key.clone(), cb));
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(220)
        .child(&checks_box)
        .vexpand(true)
        .build();
    outer.append(&scroller);

    outer.append(
        &gtk::Label::builder()
            .label("ON DISPLAY")
            .xalign(0.0)
            .css_classes(["chanctl-heading"])
            .build(),
    );
    let mon_group = gtk::CheckButton::new();
    let mut mon_radios: Vec<gtk::CheckButton> = Vec::new();
    for (i, (_, label)) in monitors.iter().enumerate() {
        let rb = gtk::CheckButton::builder().label(label).active(i == 0).build();
        rb.set_group(Some(&mon_group));
        outer.append(&rb);
        mon_radios.push(rb);
    }

    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .width_chars(30)
        .css_classes(["settings-desc"])
        .build();
    outer.append(&status);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let tile = gtk::Button::with_label("Tile");
    buttons.append(&cancel);
    buttons.append(&tile);
    outer.append(&buttons);

    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let app = app.clone();
        let window = window.clone();
        tile.connect_clicked(move |_| {
            let selected: Vec<BufferKey> = checks
                .iter()
                .filter(|(_, cb)| cb.is_active())
                .map(|(k, _)| k.clone())
                .collect();
            if selected.is_empty() {
                status.set_text("Pick at least one channel.");
                return;
            }
            let mon_idx = mon_radios.iter().position(|r| r.is_active()).unwrap_or(0);
            let geo = monitors[mon_idx].0.geometry();
            tile_windows(&app, &selected, geo, &status);
            window.close();
        });
    }

    // The line whose absence shipped an empty window: the contents box was
    // built, filled, and never attached.
    window.set_child(Some(&outer));

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

/// Grid dimensions for n windows: as square as possible, columns first —
/// the shape every tiling convention converges on.
pub fn grid_for(n: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    (cols, rows)
}

fn tile_windows(
    app: &AppRef,
    keys: &[BufferKey],
    geo: gtk::gdk::Rectangle,
    _status: &gtk::Label,
) {
    let (cols, rows) = grid_for(keys.len());
    let cell_w = geo.width() / cols as i32;
    let cell_h = geo.height() / rows as i32;

    // Open (or re-present) a popout per selection, sized for its cell, and
    // collect (caption, rect) pairs for the compositor script.
    let mut placements: Vec<(String, i32, i32, i32, i32)> = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        crate::open_popout(app, key.clone());
        let title = {
            let store = app.store.borrow();
            let name = store
                .buffer(key)
                .map(|b| b.display_name.clone())
                .unwrap_or_else(|| key.target.clone());
            format!("{name} — Scully")
        };
        let x = geo.x() + (i % cols) as i32 * cell_w;
        let y = geo.y() + (i / cols) as i32 * cell_h;
        if let Some(win) = app.chat_windows.borrow().iter().find(|w| w.pinned_key() == Some(key))
        {
            win.gtk_window().set_default_size(cell_w, cell_h);
        }
        placements.push((title, x, y, cell_w, cell_h));
    }

    // Give the compositor a beat to map the fresh surfaces, then ask KWin to
    // place them. On non-KDE this quietly does nothing and the popouts stay
    // where the shell put them, at grid-cell size.
    glib::timeout_add_local_once(std::time::Duration::from_millis(450), move || {
        if let Err(e) = kwin_place(&placements) {
            tracing::info!(error = %e, "compositor placement unavailable (not KDE?)");
        }
    });
}

/// Ask KWin to place our popouts via its scripting interface. Generated
/// script matches by caption + app id, clears maximize/fullscreen, and sets
/// each frame geometry in the compositor's own logical coordinates (GDK's
/// monitor geometry speaks the same space).
fn kwin_place(placements: &[(String, i32, i32, i32, i32)]) -> Result<(), String> {
    let wants: Vec<String> = placements
        .iter()
        .map(|(t, x, y, w, h)| {
            format!("[{}, {x}, {y}, {w}, {h}]", serde_json::to_string(t).unwrap_or_default())
        })
        .collect();
    let script = format!(
        r#"
const wants = [{}];
const R = (x, y, w, h) =>
    (typeof Qt !== "undefined" && Qt.rect) ? Qt.rect(x, y, w, h)
                                           : {{ x: x, y: y, width: w, height: h }};
const wins = workspace.windowList ? workspace.windowList() : workspace.clientList();
for (const w of wins) {{
    const cls = (w.resourceClass || "").toString().toLowerCase();
    if (!cls.includes("scully")) continue;
    for (const t of wants) {{
        if (w.caption === t[0]) {{
            if (w.fullScreen) w.fullScreen = false;
            if (typeof w.setMaximize === "function") w.setMaximize(false, false);
            w.frameGeometry = R(t[1], t[2], t[3], t[4]);
        }}
    }}
}}
"#,
        wants.join(", ")
    );

    let dir = crate::paths::data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("tile.kwin.js");
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    let path_str = path.to_string_lossy().to_string();

    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|e| e.to_string())?;
    let call = |obj: &str, iface: &str, method: &str, args: Option<&glib::Variant>| {
        conn.call_sync(
            Some("org.kde.KWin"),
            obj,
            iface,
            method,
            args,
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
        )
    };

    // Unload any previous run first: KWin refuses to reload a plugin name
    // that is already registered, which would make the button single-use.
    let _ = call(
        "/Scripting",
        "org.kde.kwin.Scripting",
        "unloadScript",
        Some(&("scully-tile",).to_variant()),
    );
    let id = call(
        "/Scripting",
        "org.kde.kwin.Scripting",
        "loadScript",
        Some(&(path_str.as_str(), "scully-tile").to_variant()),
    )
    .map_err(|e| e.to_string())?
    .child_value(0)
    .get::<i32>()
    .ok_or("unexpected loadScript reply")?;

    // The script object path moved between KWin releases; try both homes.
    let run = call(&format!("/Scripting/Script{id}"), "org.kde.kwin.Script", "run", None)
        .or_else(|_| call(&format!("/{id}"), "org.kde.kwin.Script", "run", None));
    run.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids_stay_as_square_as_possible() {
        assert_eq!(grid_for(1), (1, 1));
        assert_eq!(grid_for(2), (2, 1));
        assert_eq!(grid_for(3), (2, 2));
        assert_eq!(grid_for(4), (2, 2));
        assert_eq!(grid_for(5), (3, 2));
        assert_eq!(grid_for(6), (3, 2));
        assert_eq!(grid_for(7), (3, 3));
        assert_eq!(grid_for(9), (3, 3));
        assert_eq!(grid_for(10), (4, 3));
        assert_eq!(grid_for(0), (0, 0));
    }
}
