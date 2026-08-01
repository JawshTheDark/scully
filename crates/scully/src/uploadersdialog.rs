// Uploads & providers (#514): pick where uploads go, and manage your own
// uploaders (a catbox account, your Zipline, an S3 bucket).
//
// The forms are schema-driven end to end: `GET /api/uploaders` ships each
// driver's `configSchema`, and this window renders whatever it says — a new
// driver on the server is a new form here with no client change, which is the
// whole point of the API. Secrets are write-only: the server only ever says
// whether one is set, an empty secret field on edit means "keep the stored
// one", and nothing here can read a stored value back.


use gtk::glib;
use gtk::prelude::*;

use crate::app::AppRef;

/// Open the uploaders window, fetching the current state first.
pub fn open(app: &AppRef) {
    let Some(rest) = app.rest.borrow().clone() else { return };
    let owner = app.clone();
    app.spawn_async(
        async move { rest.uploaders().await },
        move |result| match result {
            Ok(info) => build(&owner, info),
            Err(e) => tracing::warn!(error = %e, "could not load uploaders"),
        },
    );
}

fn build(app: &AppRef, info: lurker_client::UploadersInfo) {
    let window = gtk::Window::builder()
        .title("Uploads")
        .default_width(520)
        .default_height(420)
        .modal(false)
        .transient_for(&app.gtk_app.windows()[0])
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
            .label("WHERE UPLOADS GO")
            .xalign(0.0)
            .css_classes(["chanctl-heading"])
            .build(),
    );

    let status = gtk::Label::builder().xalign(0.0).css_classes(["login-status"]).build();

    // ── Selection: instance default plus every allowed uploader ──────────
    let group_head = gtk::CheckButton::new();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);

    let make_choice = |label: &str, sub: &str, active: bool| -> (gtk::Box, gtk::CheckButton) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let radio = gtk::CheckButton::new();
        radio.set_group(Some(&group_head));
        radio.set_active(active);
        row.append(&radio);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        col.append(&gtk::Label::builder().label(label).xalign(0.0).build());
        if !sub.is_empty() {
            col.append(
                &gtk::Label::builder()
                    .label(sub)
                    .xalign(0.0)
                    .css_classes(["settings-desc"])
                    .build(),
            );
        }
        col.set_hexpand(true);
        row.append(&col);
        (row, radio)
    };

    let select = |app: &AppRef, id: Option<i64>, status: &gtk::Label| {
        let Some(rest) = app.rest.borrow().clone() else { return };
        let status = status.clone();
        app.spawn_async(
            async move { rest.select_uploader(id).await },
            move |result| match result {
                Ok(()) => status.set_text("saved"),
                Err(e) => status.set_text(&format!("could not save: {e}")),
            },
        );
    };

    let (row, radio) =
        make_choice("Instance default", "whatever this server is configured to use", info.selected_id.is_none());
    {
        let app = app.clone();
        let status = status.clone();
        radio.connect_toggled(move |r| {
            if r.is_active() {
                select(&app, None, &status);
            }
        });
    }
    list.append(&row);

    for up in &info.uploaders {
        let id = up.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
        let label = up.get("label").and_then(|v| v.as_str()).unwrap_or("uploader").to_string();
        let driver = up.get("driver").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let editable = up.get("editable").and_then(|v| v.as_bool()).unwrap_or(false);

        let (row, radio) = make_choice(&label, &driver, info.selected_id == Some(id));
        {
            let app = app.clone();
            let status = status.clone();
            radio.connect_toggled(move |r| {
                if r.is_active() {
                    select(&app, Some(id), &status);
                }
            });
        }
        if editable {
            let edit = gtk::Button::builder().label("Edit").css_classes(["toolbtn"]).build();
            {
                let app = app.clone();
                let window = window.clone();
                let up = up.clone();
                let drivers = info.drivers.clone();
                edit.connect_clicked(move |_| {
                    let schema = drivers
                        .iter()
                        .find(|d| d.get("driver").and_then(|v| v.as_str())
                            == up.get("driver").and_then(|v| v.as_str()))
                        .cloned();
                    open_editor(&app, &window, schema, Some(up.clone()));
                });
            }
            row.append(&edit);

            let del = gtk::Button::builder().label("✕").css_classes(["toolbtn"]).build();
            {
                let app = app.clone();
                let window = window.clone();
                let label = label.clone();
                del.connect_clicked(move |_| {
                    let confirm = gtk::AlertDialog::builder()
                        .message(format!("Delete uploader {label}?"))
                        .detail("Uploads fall back to the instance default.")
                        .buttons(["Cancel", "Delete"])
                        .cancel_button(0)
                        .default_button(0)
                        .build();
                    let app = app.clone();
                    let window = window.clone();
                    confirm.choose(Some(&window.clone()), gtk::gio::Cancellable::NONE, move |ans| {
                        if ans == Ok(1) {
                            let Some(rest) = app.rest.borrow().clone() else { return };
                            let app2 = app.clone();
                            let window = window.clone();
                            app.spawn_async(
                                async move { rest.delete_uploader(id).await },
                                move |result| {
                                    if result.is_ok() {
                                        // Rebuild from fresh server state.
                                        window.close();
                                        open(&app2);
                                    }
                                },
                            );
                        }
                    });
                });
            }
            row.append(&del);
        }
        list.append(&row);
    }
    outer.append(&list);

    // ── Add a personal uploader ───────────────────────────────────────────
    if info.allow_user_defined {
        let creatable: Vec<serde_json::Value> = info
            .drivers
            .iter()
            .filter(|d| d.get("creatable").and_then(|v| v.as_bool()).unwrap_or(false))
            .cloned()
            .collect();
        if !creatable.is_empty() {
            outer.append(
                &gtk::Label::builder()
                    .label("ADD YOUR OWN")
                    .xalign(0.0)
                    .css_classes(["chanctl-heading"])
                    .build(),
            );
            let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let names: Vec<String> = creatable
                .iter()
                .map(|d| {
                    d.get("label")
                        .and_then(|v| v.as_str())
                        .unwrap_or("driver")
                        .to_string()
                })
                .collect();
            let strs: Vec<&str> = names.iter().map(String::as_str).collect();
            let dropdown = gtk::DropDown::from_strings(&strs);
            let add = gtk::Button::builder().label("Add…").css_classes(["toolbtn"]).build();
            {
                let app = app.clone();
                let window = window.clone();
                let dropdown = dropdown.clone();
                add.connect_clicked(move |_| {
                    let idx = dropdown.selected() as usize;
                    if let Some(schema) = creatable.get(idx).cloned() {
                        open_editor(&app, &window, Some(schema), None);
                    }
                });
            }
            add_row.append(&dropdown);
            add_row.append(&add);
            outer.append(&add_row);
        }
    } else {
        outer.append(
            &gtk::Label::builder()
                .label("This server does not allow personal uploaders.")
                .xalign(0.0)
                .css_classes(["settings-desc"])
                .build(),
        );
    }

    outer.append(&status);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&outer)
        .vexpand(true)
        .build();
    window.set_child(Some(&scroller));

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

/// The add/edit form for one uploader, rendered from the driver's own
/// `configSchema`. `existing` set → edit (PATCH); absent → create (POST).
fn open_editor(
    app: &AppRef,
    parent: &gtk::Window,
    driver_desc: Option<serde_json::Value>,
    existing: Option<serde_json::Value>,
) {
    let Some(desc) = driver_desc else { return };
    let driver_id =
        desc.get("driver").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let driver_label =
        desc.get("label").and_then(|v| v.as_str()).unwrap_or(&driver_id).to_string();
    let schema: Vec<serde_json::Value> = desc
        .get("configSchema")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let editing_id = existing.as_ref().and_then(|e| e.get("id")).and_then(|v| v.as_i64());
    let stored_config = existing
        .as_ref()
        .and_then(|e| e.get("config"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let secrets_set = existing
        .as_ref()
        .and_then(|e| e.get("secretsSet"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let window = gtk::Window::builder()
        .title(match editing_id {
            Some(_) => format!("Edit {driver_label} uploader"),
            None => format!("Add a {driver_label} uploader"),
        })
        .default_width(480)
        .modal(false)
        .transient_for(parent)
        .destroy_with_parent(true)
        .build();
    crate::fit_to_screen(&window);
    window.add_css_class("chanctl");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(14);
    outer.set_margin_bottom(14);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(10);

    let label_entry = gtk::Entry::builder()
        .placeholder_text(format!("My {driver_label}"))
        .hexpand(true)
        .build();
    if let Some(l) = existing.as_ref().and_then(|e| e.get("label")).and_then(|v| v.as_str()) {
        label_entry.set_text(l);
    }
    grid.attach(&gtk::Label::builder().label("Name").xalign(0.0).build(), 0, 0, 1, 1);
    grid.attach(&label_entry, 1, 0, 1, 1);

    // (key, entry, is_secret) per schema field, in schema order.
    let mut fields: Vec<(String, gtk::Entry, bool)> = Vec::new();
    for (i, field) in schema.iter().enumerate() {
        let key = field.get("key").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let flabel = field.get("label").and_then(|v| v.as_str()).unwrap_or(&key).to_string();
        let is_secret = field.get("type").and_then(|v| v.as_str()) == Some("secret");
        let desc_text =
            field.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let entry = gtk::Entry::builder().hexpand(true).tooltip_text(desc_text).build();
        if is_secret {
            entry.set_visibility(false);
            entry.set_input_purpose(gtk::InputPurpose::Password);
            // Write-only: the placeholder says whether one is stored; leaving
            // it blank on an edit keeps the stored value.
            let has = secrets_set.get(&key).and_then(|v| v.as_bool()).unwrap_or(false);
            entry.set_placeholder_text(Some(if has { "(unchanged)" } else { "" }));
        } else if let Some(v) = stored_config.get(&key).and_then(|v| v.as_str()) {
            entry.set_text(v);
        } else if let Some(d) = field.get("default").and_then(|v| v.as_str()) {
            entry.set_text(d);
        }
        grid.attach(
            &gtk::Label::builder().label(&flabel).xalign(0.0).build(),
            0,
            (i + 1) as i32,
            1,
            1,
        );
        grid.attach(&entry, 1, (i + 1) as i32, 1, 1);
        fields.push((key, entry, is_secret));
    }
    outer.append(&grid);

    let status = gtk::Label::builder().xalign(0.0).css_classes(["login-status"]).build();
    outer.append(&status);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    buttons.append(&cancel);
    buttons.append(&save);
    outer.append(&buttons);

    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let app = app.clone();
        let window = window.clone();
        let parent = parent.clone();
        save.connect_clicked(move |_| {
            let mut values = serde_json::Map::new();
            for (key, entry, is_secret) in &fields {
                let v = entry.text().to_string();
                // An untouched secret on edit is omitted → server keeps the
                // stored one. Everything else is sent, blanks included, so a
                // cleared field really clears.
                if *is_secret && v.is_empty() && editing_id.is_some() {
                    continue;
                }
                values.insert(key.clone(), serde_json::Value::String(v));
            }
            let label = label_entry.text().trim().to_string();
            let Some(rest) = app.rest.borrow().clone() else { return };
            let app2 = app.clone();
            let window = window.clone();
            let parent = parent.clone();
            let status = status.clone();
            let driver_id = driver_id.clone();
            app.spawn_async(
                async move {
                    match editing_id {
                        Some(id) => {
                            rest.update_uploader(id, &label, serde_json::Value::Object(values))
                                .await
                        }
                        None => {
                            rest.create_uploader(
                                &driver_id,
                                &label,
                                serde_json::Value::Object(values),
                            )
                            .await
                        }
                    }
                },
                move |result| match result {
                    Ok(_) => {
                        window.close();
                        // The list window's data is stale now; rebuild it.
                        parent.close();
                        open(&app2);
                    }
                    Err(e) => status.set_text(&format!("could not save: {e}")),
                },
            );
        });
    }

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&outer)
        .build();
    window.set_child(Some(&scroller));
    window.present();
}
