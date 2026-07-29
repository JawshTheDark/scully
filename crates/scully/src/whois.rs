// Formatting a structured `whois_result` payload into readable lines.
//
// The payload is the irc-framework whois shape (nick, ident, hostname,
// real_name, server, channels, idle, account, …), relayed by Lurker as an
// ephemeral event. Pure and testable — the window just injects these lines
// into whichever buffer the user's setting points at.

use serde_json::Value;

/// One field's value as a trimmed string, if present and non-empty.
fn field<'a>(w: &'a Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = w.get(k) {
            let text = match v {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Human-readable whois lines for the given payload. The first line names the
/// nick; `error: "not_found"` yields a single "no such nick" line.
pub fn format(whois: &Value) -> Vec<String> {
    let nick = field(whois, &["nick"]).unwrap_or_else(|| "?".into());

    if field(whois, &["error"]).as_deref() == Some("not_found") {
        return vec![format!("whois {nick}: no such nick")];
    }

    let mut out = Vec::new();

    // Identity line: nick is ident@host (real name).
    let ident = field(whois, &["ident", "user"]);
    let host = field(whois, &["hostname", "host"]);
    let real = field(whois, &["real_name", "realname", "real"]);
    let mut ident_line = format!("whois {nick}");
    if let (Some(i), Some(h)) = (&ident, &host) {
        ident_line.push_str(&format!(" is {i}@{h}"));
    } else if let Some(h) = &host {
        ident_line.push_str(&format!(" is {h}"));
    }
    if let Some(r) = &real {
        ident_line.push_str(&format!(" ({r})"));
    }
    out.push(ident_line);

    if let Some(account) = field(whois, &["account"]) {
        out.push(format!("  logged in as {account}"));
    }
    if let Some(server) = field(whois, &["server"]) {
        let info = field(whois, &["server_info"]).map(|i| format!(" ({i})")).unwrap_or_default();
        out.push(format!("  on {server}{info}"));
    }
    // Channels can be an array or a space-joined string.
    let channels = match whois.get("channels") {
        Some(Value::Array(a)) => {
            let names: Vec<String> =
                a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
            (!names.is_empty()).then(|| names.join(" "))
        }
        _ => field(whois, &["channels"]),
    };
    if let Some(chans) = channels {
        out.push(format!("  on channels: {chans}"));
    }
    if field(whois, &["secure", "ssl"]).as_deref() == Some("true") {
        out.push("  using a secure connection".to_string());
    }
    if let Some(op) = field(whois, &["operator"]) {
        out.push(format!("  {op}"));
    }
    if let Some(away) = field(whois, &["away"]) {
        out.push(format!("  away: {away}"));
    }
    if let Some(idle) = field(whois, &["idle"]) {
        // idle is seconds.
        if let Ok(secs) = idle.parse::<i64>() {
            out.push(format!("  idle {}", human_idle(secs)));
        }
    }
    out
}

fn human_idle(secs: i64) -> String {
    let (d, h, m, s) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_a_full_whois() {
        let w = json!({
            "nick": "amiantos", "ident": "~brad", "hostname": "user/amiantos",
            "real_name": "Brad Root", "account": "amiantos", "server": "irc.libera.chat",
            "server_info": "Helsinki, FI", "channels": ["#lurker", "@#dev"],
            "secure": true, "idle": 3725,
        });
        let lines = format(&w);
        assert_eq!(lines[0], "whois amiantos is ~brad@user/amiantos (Brad Root)");
        assert!(lines.iter().any(|l| l == "  logged in as amiantos"));
        assert!(lines.iter().any(|l| l.contains("irc.libera.chat") && l.contains("Helsinki")));
        assert!(lines.iter().any(|l| l == "  on channels: #lurker @#dev"));
        assert!(lines.iter().any(|l| l.contains("secure connection")));
        assert!(lines.iter().any(|l| l == "  idle 1h 2m 5s"));
    }

    #[test]
    fn handles_not_found() {
        let lines = format(&json!({"nick": "ghost", "error": "not_found"}));
        assert_eq!(lines, ["whois ghost: no such nick"]);
    }

    #[test]
    fn tolerates_a_sparse_payload() {
        // Only a nick — must still produce a sane single line, no panic.
        let lines = format(&json!({"nick": "x"}));
        assert_eq!(lines, ["whois x"]);
    }

    #[test]
    fn channels_as_string_also_work() {
        let lines = format(&json!({"nick": "y", "channels": "#a #b"}));
        assert!(lines.iter().any(|l| l == "  on channels: #a #b"));
    }

    #[test]
    fn human_idle_reads_naturally() {
        assert_eq!(human_idle(0), "0s");
        assert_eq!(human_idle(59), "59s");
        assert_eq!(human_idle(3725), "1h 2m 5s");
        assert_eq!(human_idle(90061), "1d 1h 1m 1s");
    }
}
