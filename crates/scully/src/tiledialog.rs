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

    outer.append(
        &gtk::Label::builder()
            .label("LAYOUT")
            .xalign(0.0)
            .css_classes(["chanctl-heading"])
            .build(),
    );
    let layout_group = gtk::CheckButton::new();
    let mut layout_radios: Vec<(Layout, gtk::CheckButton)> = Vec::new();
    for (i, (layout, label)) in [
        (Layout::Grid, "Grid — as square as possible"),
        (Layout::Stacked, "Stacked — top to bottom, full width"),
        (Layout::SideBySide, "Side by side — full height columns"),
    ]
    .into_iter()
    .enumerate()
    {
        let rb = gtk::CheckButton::builder().label(label).active(i == 0).build();
        rb.set_group(Some(&layout_group));
        outer.append(&rb);
        layout_radios.push((layout, rb));
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
            let scale = monitors[mon_idx].0.scale_factor();
            let layout = layout_radios
                .iter()
                .find(|(_, r)| r.is_active())
                .map(|(l, _)| *l)
                .unwrap_or(Layout::Grid);
            tile_windows(&app, &selected, geo, scale, layout);
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

/// How the cells divide the display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// As square as possible — the shape tiling conventions converge on.
    Grid,
    /// One full-width column, windows stacked top to bottom.
    Stacked,
    /// One full-height row, windows side by side.
    SideBySide,
}

/// Cell dimensions for n windows under a layout.
pub fn dims_for(n: usize, layout: Layout) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    match layout {
        Layout::Grid => {
            let cols = (n as f64).sqrt().ceil() as usize;
            (cols, n.div_ceil(cols))
        }
        Layout::Stacked => (1, n),
        Layout::SideBySide => (n, 1),
    }
}

fn tile_windows(
    app: &AppRef,
    keys: &[BufferKey],
    geo: gtk::gdk::Rectangle,
    scale: i32,
    layout: Layout,
) {
    let (cols, rows) = dims_for(keys.len(), layout);
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

    // Give the shell a beat to map the fresh surfaces, then place through
    // whichever door this world offers.
    glib::timeout_add_local_once(std::time::Duration::from_millis(450), move || {
        match place_all(&placements, scale) {
            Ok(backend) => tracing::info!(backend, "windows placed"),
            Err(e) => tracing::info!(error = %e, "no placement backend — popouts are cell-sized, shell decides position"),
        }
    });
}

/// Place windows through the platform's door. Every backend is a spawned
/// incantation, not a linked library: PowerShell on Windows, osascript on
/// macOS, and on Linux the KWin DBus first, then sway/i3 IPC, then wmctrl
/// for any X11 WM. Spawning keeps the build identical on every target and
/// turns a missing door into a log line instead of a crash.
fn place_all(placements: &[(String, i32, i32, i32, i32)], scale: i32) -> Result<&'static str, String> {
    #[cfg(target_os = "windows")]
    {
        return windows_place(placements, scale).map(|()| "win32");
    }
    #[cfg(target_os = "macos")]
    {
        let _ = scale; // AppleScript speaks logical points, like GDK.
        return macos_place(placements).map(|()| "osascript");
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = scale; // Wayland/X11 tools speak logical coordinates here.
        let mut errs = Vec::new();
        match kwin_place(placements) {
            Ok(()) => return Ok("kwin"),
            Err(e) => errs.push(format!("kwin: {e}")),
        }
        if std::env::var_os("SWAYSOCK").is_some() {
            match ipc_place("swaymsg", placements) {
                Ok(()) => return Ok("sway"),
                Err(e) => errs.push(format!("sway: {e}")),
            }
        }
        if std::env::var_os("I3SOCK").is_some() {
            match ipc_place("i3-msg", placements) {
                Ok(()) => return Ok("i3"),
                Err(e) => errs.push(format!("i3: {e}")),
            }
        }
        match wmctrl_place(placements) {
            Ok(()) => return Ok("wmctrl"),
            Err(e) => errs.push(format!("wmctrl: {e}")),
        }
        Err(errs.join("; "))
    }
}

/// sway and i3 share the i3 IPC command language; only the client binary
/// differs. Windows are floated first — a tiled window ignores move/resize.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn ipc_place(client: &str, placements: &[(String, i32, i32, i32, i32)]) -> Result<(), String> {
    for (title, x, y, w, h) in placements {
        // Exact-match regex on the title, escaped: sway criteria are regex.
        let escaped = regex_escape(title);
        let cmd = format!(
            "[title=\"^{escaped}$\"] floating enable, resize set {w} {h}, move absolute position {x} {y}"
        );
        let out = std::process::Command::new(client)
            .arg(&cmd)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let escape = matches!(c, '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}'
                | '|' | '^' | '$' | '\\');
            escape.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect()
}

/// Any X11 window manager: wmctrl -r <title> -e. Only meaningful in an X11
/// session; under Wayland the tool sees no windows and reports so.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn wmctrl_place(placements: &[(String, i32, i32, i32, i32)]) -> Result<(), String> {
    if std::env::var("XDG_SESSION_TYPE").map(|t| t == "wayland").unwrap_or(false) {
        return Err("wayland session — wmctrl cannot see the windows".into());
    }
    for (title, x, y, w, h) in placements {
        let out = std::process::Command::new("wmctrl")
            .args(["-r", title, "-e", &format!("0,{x},{y},{w},{h}")])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
    }
    Ok(())
}

/// Windows: EnumWindows + SetWindowPos through an Add-Type PowerShell stub,
/// matched by exact window title. Win32 speaks PHYSICAL pixels, so the
/// logical cells are multiplied by the monitor's scale factor.
#[cfg(target_os = "windows")]
fn windows_place(placements: &[(String, i32, i32, i32, i32)], scale: i32) -> Result<(), String> {
    let table: Vec<String> = placements
        .iter()
        .map(|(t, x, y, w, h)| {
            format!(
                "@{{T={};X={};Y={};W={};H={}}}",
                ps_quote(t),
                x * scale,
                y * scale,
                w * scale,
                h * scale
            )
        })
        .collect();
    let script = format!(
        r#"Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class ScullyTile {{
  [DllImport("user32.dll")] public static extern bool EnumWindows(Func<IntPtr,IntPtr,bool> cb, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int w, int hh, uint f);
}}
'@
$wants = @({wants})
[ScullyTile]::EnumWindows({{ param($h, $l)
  $sb = New-Object System.Text.StringBuilder 512
  [void][ScullyTile]::GetWindowText($h, $sb, 512)
  $t = $sb.ToString()
  foreach ($w in $wants) {{
    if ($t -eq $w.T) {{ [void][ScullyTile]::SetWindowPos($h, [IntPtr]::Zero, $w.X, $w.Y, $w.W, $w.H, 0x0044) }}
  }}
  $true
}}, [IntPtr]::Zero) | Out-Null
"#,
        wants = table.join(", ")
    );
    let dir = crate::paths::data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("tile.ps1");
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// macOS: System Events via osascript, matched by window name.
///
/// Two SEPARATE permissions gate this, and the field run confirmed users
/// meet them in order: the Automation prompt ("control the system") appears
/// by itself, but UI scripting ALSO needs Accessibility, which never
/// prompts — it must be granted by hand. The script deliberately has no
/// try-wrapping: the first version swallowed every error and reported
/// success while moving nothing, which is the worst of all outcomes.
#[cfg(target_os = "macos")]
fn macos_place(placements: &[(String, i32, i32, i32, i32)]) -> Result<(), String> {
    let mut lines = String::new();
    for (title, x, y, w, h) in placements {
        let t = title.replace('"', "\\\"");
        lines.push_str(&format!(
            "  set position of (every window whose name is \"{t}\") to {{{x}, {y}}}\n  set size of (every window whose name is \"{t}\") to {{{w}, {h}}}\n"
        ));
    }
    let script = format!(
        "tell application \"System Events\"\n tell (first process whose name contains \"scully\")\n{lines} end tell\nend tell"
    );
    let out = std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(if err.contains("assistive") {
            format!(
                "{err} — grant Accessibility to Scully (and Terminal, if launched from one) in \
                 System Settings > Privacy & Security > Accessibility; this is separate from \
                 the Automation prompt you already accepted"
            )
        } else if err.contains("1743") || err.contains("Not authorized") {
            format!("{err} — allow Scully to control System Events under Privacy > Automation")
        } else {
            err
        });
    }
    Ok(())
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
        let g = |n| dims_for(n, Layout::Grid);
        assert_eq!(g(1), (1, 1));
        assert_eq!(g(2), (2, 1));
        assert_eq!(g(3), (2, 2));
        assert_eq!(g(4), (2, 2));
        assert_eq!(g(5), (3, 2));
        assert_eq!(g(6), (3, 2));
        assert_eq!(g(7), (3, 3));
        assert_eq!(g(9), (3, 3));
        assert_eq!(g(10), (4, 3));
        assert_eq!(g(0), (0, 0));
    }

    #[test]
    fn stacked_and_side_by_side_are_single_file() {
        assert_eq!(dims_for(3, Layout::Stacked), (1, 3), "full width, top to bottom");
        assert_eq!(dims_for(3, Layout::SideBySide), (3, 1), "full height columns");
        assert_eq!(dims_for(0, Layout::Stacked), (0, 0));
    }
}
