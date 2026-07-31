// Scully — a native GTK4 desktop client for a self-hosted Lurker bouncer.
//
// Named for the one at the desk who wants evidence. Sibling app to Spooky
// (Android); both speak the Lurker client protocol, v1.
//
// No web view anywhere: the UI is GTK widgets, the scrollback is a GtkTextView
// (which is what buys real text selection, IME and complex-script shaping for
// free), and the protocol work lives in `lurker-client` / `lurker-proto`.

mod app;
mod attention;
mod buffermenu;
mod callmod;
mod chanlist;
mod channeldialog;
mod commands;
mod credentials;
mod finder;
mod format;
mod input;
mod login;
mod media;
mod modelist;
mod networkdialog;
mod nickmenu;
mod paths;
mod settings;
mod theme;
#[cfg(feature = "voice")]
mod voice;
#[cfg(feature = "voice")]
mod callwindow;
mod whois;
mod window;

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use app::AppRef;

const APP_ID: &str = "io.jawsh.Scully";
const STYLE: &str = include_str!("style.css");

fn main() -> glib::ExitCode {
    // Disable the AT-SPI accessibility bridge. It is non-functional on many
    // sessions (a broken `org.a11y.atspi.Registry` floods the log with
    // registration failures for every widget and window, and has been
    // implicated in crashes), and Scully has no accessibility integration to
    // lose. Set before GTK initialises so the bridge never starts. An explicit
    // user override still wins.
    if std::env::var_os("GTK_A11Y").is_none() {
        std::env::set_var("GTK_A11Y", "none");
    }

    // Log the location of any panic. GTK signal handlers cannot unwind (a panic
    // there aborts), so a logged location is often the only trace we get.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "PANIC");
        default_hook(info);
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "scully=info,lurker_client=info".into()),
        )
        .init();

    // A second launch normally hands off to the running instance (presence and
    // read state are account-wide, so two processes would fight). SCULLY_APP_ID
    // overrides the identity so a development build can run beside a real
    // session instead of opening a window in it.
    let app_id = std::env::var("SCULLY_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
    let gtk_app = gtk::Application::builder().application_id(&app_id).build();

    gtk_app.connect_startup(|_| {
        load_css();
        // Name the themed icon for every window. On Wayland the compositor
        // finds an app's icon by matching the surface's app_id to a .desktop
        // file of the same name — which is why the desktop file and the icon
        // are both named for the application id rather than "scully". Without
        // this the shell falls back to a generic placeholder.
        gtk::Window::set_default_icon_name(APP_ID);
    });

    gtk_app.connect_activate(|gtk_app| {
        // A second launch reuses the running instance: presence and read state
        // are account-wide, so a second process would fight the first.
        if let Some(existing) = existing_app() {
            open_chat_window(&existing);
            return;
        }
        let app = app::App::new(gtk_app.clone());
        register_app(&app);
        install_actions(&app);

        if !login::try_resume(&app) {
            login::show(&app);
        }
    });

    gtk_app.connect_shutdown(|_| {
        if let Some(app) = existing_app() {
            app.shutdown();
        }
    });

    gtk_app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(STYLE);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

// The single App instance, kept in a thread-local because GTK is
// single-threaded and every consumer already lives on the main context.
thread_local! {
    static APP: std::cell::RefCell<Option<AppRef>> = const { std::cell::RefCell::new(None) };
}

fn register_app(app: &AppRef) {
    APP.with(|slot| *slot.borrow_mut() = Some(app.clone()));
}

fn existing_app() -> Option<AppRef> {
    APP.with(|slot| slot.borrow().clone())
}

/// The registered App, for modules that need an `AppRef` from a `&self`
/// context (the theme engine, called from inside App methods).
pub fn main_app() -> Option<AppRef> {
    existing_app()
}

/// Maximise a window when the screen is phone-sized.
///
/// A handset compositor (Phosh on FuriOS, for one) will not maximise a window
/// that declares itself non-resizable, and a modest desktop default size then
/// floats in the middle of the display with wallpaper around it. Deciding from
/// the monitor's own geometry keeps this a property of the screen rather than a
/// device check.
pub fn fit_to_screen(window: &impl IsA<gtk::Window>) {
    let Some(display) = gtk::gdk::Display::default() else { return };
    let Some(obj) = display.monitors().item(0) else { return };
    let Ok(monitor) = obj.downcast::<gtk::gdk::Monitor>() else { return };
    let geo = monitor.geometry();
    if geo.width() <= 800 || geo.height() <= 900 {
        window.as_ref().set_maximized(true);
    }
}

/// Open another full window onto the same store and socket.
pub fn open_chat_window(app: &AppRef) {
    let win = window::ChatWindow::new(app);
    win.present();
    app.chat_windows.borrow_mut().push(win);
}

/// Pop one buffer out into its own lightweight window.
///
/// Same store, same socket, no sidebars — the way to watch several channels
/// at once. Re-popping a buffer that already has a popout re-presents it
/// rather than stacking duplicates.
pub fn open_popout(app: &AppRef, key: lurker_proto::BufferKey) {
    if let Some(existing) =
        app.chat_windows.borrow().iter().find(|w| w.pinned_key() == Some(&key)).cloned()
    {
        existing.present();
        return;
    }
    let win = window::ChatWindow::popout(app, key);
    win.present();
    app.chat_windows.borrow_mut().push(win);
}

/// Drop a closed window from the registry, breaking the App→Window→App cycle.
pub fn forget_chat_window(app: &AppRef, target: &Rc<window::ChatWindow>) {
    app.chat_windows.borrow_mut().retain(|w| !Rc::ptr_eq(w, target));
}

fn install_actions(app: &AppRef) {
    // Open a buffer by key: "<networkId>/<target>", or "-/<target>" for the
    // app-scoped system buffer. Exposed as an action so it can be driven from
    // a notification tap, and so the click path is reachable in testing.
    let activate_buffer =
        gio::SimpleAction::new("activate-buffer", Some(glib::VariantTy::STRING));
    let owner = app.clone();
    activate_buffer.connect_activate(move |_, param| {
        let Some(spec) = param.and_then(|p| p.get::<String>()) else { return };
        let Some((net, target)) = spec.split_once('/') else { return };
        let key = lurker_proto::BufferKey::new(net.parse::<i64>().ok(), target);
        if let Some(win) = owner.chat_windows.borrow().first() {
            win.open_key(&key);
        }
    });
    app.gtk_app.add_action(&activate_buffer);

    let settings = gio::SimpleAction::new("settings", None);
    let owner = app.clone();
    settings.connect_activate(move |_, _| crate::settings::SettingsWindow::open(&owner));
    app.gtk_app.add_action(&settings);
    app.gtk_app.set_accels_for_action("app.settings", &["<Control>comma"]);

    let quit = gio::SimpleAction::new("quit", None);
    let owner = app.clone();
    quit.connect_activate(move |_, _| {
        owner.shutdown();
        owner.gtk_app.quit();
    });
    app.gtk_app.add_action(&quit);
    app.gtk_app.set_accels_for_action("app.quit", &["<Control>q"]);
}
