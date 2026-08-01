// Runtime theming from server-synced settings.
//
// The static stylesheet in `style.css` is only the first paint. The `look.*`
// registry keys are the user's actual theme, synced across their devices by
// the server — so on every settings change this regenerates a CSS provider
// that overrides the named colours and fonts, and the windows retint their
// text tags. Changing "Background" in Scully or in the web client restyles
// both.


use crate::app::AppRef;

/// The GTK named colours the runtime theme may override, and the registry key
/// each one tracks. Colour values are validated before being written into CSS
/// so a malformed stored value cannot break the whole provider.
const COLOR_MAP: &[(&str, &str)] = &[
    ("bg", "look.color.bg"),
    ("bg_soft", "look.color.bg_soft"),
    ("fg", "look.color.fg"),
    ("fg_muted", "look.color.fg_muted"),
    ("accent", "look.color.accent"),
    ("good", "look.color.good"),
    ("warn", "look.color.warn"),
    ("bad", "look.color.bad"),
    ("border", "look.color.border"),
    ("member_owner", "look.color.member.owner"),
    ("member_admin", "look.color.member.admin"),
    ("member_op", "look.color.member.op"),
    ("member_halfop", "look.color.member.halfop"),
    ("member_voice", "look.color.member.voice"),
    ("buffer_unread", "look.color.buffer.unread"),
    ("buffer_highlight", "look.color.buffer.highlight"),
];

/// Accept a CSS colour we can safely embed: hex, or an alphanumeric name.
/// Anything else (or anything containing `;`/`}`) is rejected rather than
/// spliced into the stylesheet.
fn safe_color(value: &str) -> Option<&str> {
    let v = value.trim();
    let hex_ok = v.strip_prefix('#').is_some_and(|h| {
        matches!(h.len(), 3 | 4 | 6 | 8) && h.chars().all(|c| c.is_ascii_hexdigit())
    });
    let name_ok = !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric());
    (hex_ok || name_ok).then_some(v)
}

/// First concrete family out of a CSS font stack: the registry default is a
/// web stack (`'Input Mono', ui-monospace, …`) whose generic entries mean
/// nothing to Pango.
fn first_family(stack: &str) -> Option<String> {
    stack
        .split(',')
        .map(|part| part.trim().trim_matches(['\'', '"']).trim())
        .find(|f| {
            !f.is_empty()
                && !matches!(
                    *f,
                    "ui-monospace" | "monospace" | "sans-serif" | "serif" | "system-ui"
                )
        })
        .map(str::to_string)
}

/// Build the override stylesheet from the current settings.
fn build_css(app: &AppRef) -> String {
    let mut css = String::new();

    for (name, key) in COLOR_MAP {
        if let Some(value) = app.setting(key).as_str().and_then(safe_color) {
            css.push_str(&format!("@define-color {name} {value};\n"));
        }
    }

    // Fonts. The size is stored in px; GTK CSS accepts px directly.
    let mut font_rules = String::new();
    if let Some(stack) = app.setting("look.font.family").as_str() {
        if let Some(family) = first_family(stack) {
            // Keep monospace fallback: the column layout depends on it.
            font_rules.push_str(&format!("font-family: \"{family}\", monospace;\n"));
        }
    }
    let size_key = if crate::is_mobile_class() {
        "look.font.size.mobile"
    } else {
        "look.font.size"
    };
    if let Some(px) = app.setting(size_key).as_f64() {
        if (8.0..=32.0).contains(&px) {
            font_rules.push_str(&format!("font-size: {px}px;\n"));
        }
    }
    if let Some(weight) = app.setting("look.font.weight").as_f64() {
        if (100.0..=900.0).contains(&weight) {
            font_rules.push_str(&format!("font-weight: {};\n", weight as i32));
        }
    }
    if !font_rules.is_empty() {
        css.push_str(&format!("window {{ {font_rules} }}\n"));
    }

    // Member-rank colours reference the named colours defined above.
    css.push_str(
        ".member-owner { color: @member_owner; }\n\
         .member-admin { color: @member_admin; }\n\
         .member-op { color: @member_op; }\n\
         .member-halfop { color: @member_halfop; }\n\
         .member-voice { color: @member_voice; }\n\
         .badge { color: @buffer_unread; }\n\
         .badge-highlight { background-color: @buffer_highlight; }\n\
         .badge-call { color: @member_voice; }\n\
         .call-panel { padding: 6px; }\n\
         .call-status { color: @member_voice; font-size: 90%; }\n",
    );

    css
}

thread_local! {
    static ACTIVE_PROVIDER: std::cell::RefCell<Option<gtk::CssProvider>> =
        const { std::cell::RefCell::new(None) };
}

/// Apply (or re-apply) the settings-derived theme.
///
/// Loads a fresh provider at USER priority — above the static APPLICATION
/// stylesheet — and drops the previous one, so repeated settings changes
/// don't stack providers.
pub fn apply(app: &AppRef) {
    let Some(display) = gtk::gdk::Display::default() else { return };
    let css = build_css(app);

    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);

    ACTIVE_PROVIDER.with(|slot| {
        if let Some(old) = slot.borrow_mut().take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
        *slot.borrow_mut() = Some(provider);
    });
}

/// The user's nick colour palette (`look.nick.colors`), falling back to the
/// built-in one. Windows retint their nick tags from this on settings changes.
pub fn nick_palette(app: &AppRef) -> Vec<String> {
    let configured: Vec<String> = app
        .setting("look.nick.colors")
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| safe_color(s).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        crate::format::DEFAULT_NICK_PALETTE.iter().map(|s| s.to_string()).collect()
    } else {
        configured
    }
}

/// The colour for one's own nick, when configured.
pub fn self_color(app: &AppRef) -> Option<String> {
    app.setting("look.nick.self_color")
        .as_str()
        .and_then(safe_color)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_css_injection_in_colour_values() {
        // Settings come from the server, but the server accepts what the user
        // typed in ANOTHER client — a stored value must not be able to break
        // or extend the stylesheet.
        assert_eq!(safe_color("#16161d"), Some("#16161d"));
        assert_eq!(safe_color(" tomato "), Some("tomato"));
        assert_eq!(safe_color("#16161d; } window { background: red"), None);
        assert_eq!(safe_color("url(javascript:x)"), None);
        assert_eq!(safe_color(""), None);
        assert_eq!(safe_color("#12345"), None, "wrong hex length");
    }

    #[test]
    fn picks_the_first_concrete_font_family() {
        assert_eq!(
            first_family("'Input Mono', 'Input', ui-monospace, SFMono-Regular, monospace"),
            Some("Input Mono".to_string())
        );
        assert_eq!(first_family("ui-monospace, monospace"), None);
        assert_eq!(first_family("\"JetBrains Mono\""), Some("JetBrains Mono".to_string()));
    }
}
