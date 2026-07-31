// The login window.
//
// The password is typed here, exchanged for a session token via
// `POST /api/auth/login/token` (§3.1), and then forgotten — only the token is
// persisted, and only ever to the keyring. Nothing else in the app ever holds
// the password.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use lurker_client::Rest;

use crate::app::AppRef;
use crate::credentials::{self, Credentials};

pub struct LoginWindow {
    app: AppRef,
    window: gtk::ApplicationWindow,
    server: gtk::Entry,
    username: gtk::Entry,
    password: gtk::PasswordEntry,
    connect: gtk::Button,
    status: gtk::Label,
    busy: RefCell<bool>,
}

impl LoginWindow {
    pub fn new(app: &AppRef) -> Rc<Self> {
        // Deliberately NOT `resizable(false)`: a phone compositor refuses to
        // maximise a window that says it cannot resize, which left the login
        // form floating mid-screen surrounded by wallpaper.
        let window = gtk::ApplicationWindow::builder()
            .application(&app.gtk_app)
            .title("Scully — connect")
            .default_width(420)
            .default_height(520)
            .build();
        crate::fit_to_screen(&window);

        let heading = gtk::Label::builder().label("scully").css_classes(["brand"]).build();
        let subtitle = gtk::Label::builder()
            .label("Sign in to your Lurker server")
            .css_classes(["subtitle"])
            .build();

        let server = gtk::Entry::builder()
            .placeholder_text("https://lurker.example.com")
            .input_purpose(gtk::InputPurpose::Url)
            .activates_default(true)
            .build();
        let username =
            gtk::Entry::builder().placeholder_text("username").activates_default(true).build();
        let password = gtk::PasswordEntry::builder()
            .placeholder_text("password")
            .show_peek_icon(true)
            .activates_default(true)
            .build();

        // Prefill from the last session so a re-login is one field, not three.
        if let Some((base, user)) = credentials::last_account() {
            server.set_text(base.as_str());
            username.set_text(&user);
        }

        let connect = gtk::Button::builder()
            .label("Connect")
            .css_classes(["suggested-action", "connect"])
            .build();
        let status = gtk::Label::builder()
            .css_classes(["login-status"])
            .wrap(true)
            .xalign(0.0)
            // GTK CSS has no `max-width`, so the wrap point is a widget property.
            .max_width_chars(46)
            .build();

        let form = gtk::Box::new(gtk::Orientation::Vertical, 10);
        form.set_margin_top(28);
        form.set_margin_bottom(28);
        form.set_margin_start(28);
        form.set_margin_end(28);
        form.append(&heading);
        form.append(&subtitle);
        form.append(&server);
        form.append(&username);
        form.append(&password);
        form.append(&connect);
        form.append(&status);
        form.add_css_class("login");
        window.set_child(Some(&form));
        window.set_default_widget(Some(&connect));

        let this = Rc::new(LoginWindow {
            app: app.clone(),
            window,
            server,
            username,
            password,
            connect,
            status,
            busy: RefCell::new(false),
        });

        let handler = this.clone();
        this.connect.connect_clicked(move |_| handler.attempt());
        let handler = this.clone();
        this.password.connect_activate(move |_| handler.attempt());

        this
    }

    pub fn present(&self) {
        self.window.present();
        if self.server.text().is_empty() {
            self.server.grab_focus();
        } else if self.username.text().is_empty() {
            self.username.grab_focus();
        } else {
            self.password.grab_focus();
        }
    }

    fn set_busy(&self, busy: bool, message: &str) {
        *self.busy.borrow_mut() = busy;
        self.connect.set_sensitive(!busy);
        self.server.set_sensitive(!busy);
        self.username.set_sensitive(!busy);
        self.password.set_sensitive(!busy);
        self.status.set_text(message);
    }

    fn attempt(self: &Rc<Self>) {
        if *self.busy.borrow() {
            return;
        }
        let raw = self.server.text().trim().to_string();
        let user = self.username.text().trim().to_string();
        let pass = self.password.text().to_string();

        if raw.is_empty() || user.is_empty() || pass.is_empty() {
            self.status.set_text("Server, username and password are all required.");
            return;
        }

        // Accept `example.com` as well as a full URL, and always end in `/` so
        // `Url::join` resolves `api/...` against the host rather than replacing
        // the last path segment.
        let with_scheme =
            if raw.contains("://") { raw.clone() } else { format!("https://{raw}") };
        let Ok(mut base) = url::Url::parse(&with_scheme) else {
            self.status.set_text("That does not look like a valid server address.");
            return;
        };
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }

        let Ok(rest) = Rest::new(base.clone()) else {
            self.status.set_text("Could not initialise the HTTP client.");
            return;
        };

        self.set_busy(true, "Checking server…");

        let this = self.clone();
        let probe = rest.clone();
        self.app.spawn_async(
            async move {
                // §1: discover edition and protocol range before anything else,
                // so an incompatible client fails with a clear message instead
                // of misparsing frames later.
                let config = probe.config().await?;
                if !config.is_compatible() {
                    return Err(lurker_client::Error::UpgradeRequired);
                }
                // Instance capabilities (voiceEnabled &c.) are read by
                // App::start_session, which runs on the resume path too — this
                // config fetch is only the protocol-compatibility gate.
                probe.login_token(&user, &pass).await.map(|t| (t, user))
            },
            move |result| match result {
                Ok((token, user)) => this.on_token(rest, base, user, token.token),
                Err(e) => {
                    let message = match e {
                        // Mint is password-only: a passkey-only account cannot
                        // mint a native token and the endpoint simply 401s, so
                        // name both causes (§3.1).
                        lurker_client::Error::Unauthorized => {
                            "Incorrect username or password — or this account has no password set \
                             (passkey-only accounts cannot mint a token)."
                                .to_string()
                        }
                        other => other.to_string(),
                    };
                    this.set_busy(false, &message);
                }
            },
        );
    }

    fn on_token(self: &Rc<Self>, rest: Rest, base: url::Url, user: String, token: String) {
        let storage = Credentials::for_server(&base, &user).store(&token);
        credentials::remember_last(&base, &user);
        *self.app.account.borrow_mut() = Some((base, user));

        tracing::info!("session token stored in {}", storage.describe());
        self.set_busy(false, "");

        self.app.start_session(rest.with_token(token));
        crate::open_chat_window(&self.app);
        self.window.close();
    }
}

/// Try to resume a stored session without prompting.
///
/// Returns false when there is nothing usable, in which case the caller shows
/// the login window.
pub fn try_resume(app: &AppRef) -> bool {
    let Some((base, user)) = credentials::last_account() else { return false };
    let Some(token) = Credentials::for_server(&base, &user).load() else { return false };
    let Ok(rest) = Rest::new(base.clone()) else { return false };

    *app.account.borrow_mut() = Some((base, user));
    app.start_session(rest.with_token(token));
    crate::open_chat_window(app);
    true
}

/// Show the login window.
pub fn show(app: &AppRef) {
    let login = LoginWindow::new(app);
    login.present();
    // The app owns the window so it outlives this call; the strong reference is
    // dropped when the window closes, which breaks the App→Window→App cycle.
    *app.login_window.borrow_mut() = Some(login.clone());
    let owner = app.clone();
    login.window.connect_destroy(move |_| {
        owner.login_window.replace(None);
    });
}
