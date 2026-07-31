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
    if let Some(op) = field(whois, &["operator"]) {
        out.push(format!("  {op}"));
    }
    if let Some(modes) = field(whois, &["modes"]) {
        out.push(format!("  modes {modes}"));
    }
    // The real host/IP behind a cloak (RPL_WHOISHOST / RPL_WHOISACTUALLY).
    // Oper-visible on most networks, and one of the main reasons to whois at
    // all — dropping it made the formatted view strictly worse than the raw one.
    match (field(whois, &["actual_hostname"]), field(whois, &["actual_ip"])) {
        (Some(h), Some(ip)) if h != ip => out.push(format!("  connecting from {h} ({ip})")),
        (Some(h), None) => out.push(format!("  connecting from {h}")),
        (Some(_), Some(ip)) | (None, Some(ip)) => out.push(format!("  connecting from {ip}")),
        (None, None) => {}
    }
    if field(whois, &["secure", "ssl"]).as_deref() == Some("true") {
        out.push("  using a secure connection".to_string());
    }
    if let Some(fp) = field(whois, &["certfp"]) {
        out.push(format!("  TLS certificate {fp}"));
    }
    if let Some(reg) = field(whois, &["registered_nick"]) {
        out.push(format!("  {reg}"));
    }
    if let Some(bot) = field(whois, &["bot"]) {
        out.push(format!("  {bot}"));
    }
    // Free-form server lines (RPL_WHOISSPECIAL): security groups, IP
    // reputation, connection class — whatever the ircd chooses to volunteer,
    // and typically the oper-only detail. Rendered verbatim, because their
    // wording is entirely the server's own.
    if let Some(Value::Array(items)) = whois.get("special") {
        for item in items {
            if let Some(line) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                out.push(format!("  {line}"));
            }
        }
    } else if let Some(one) = field(whois, &["special"]) {
        out.push(format!("  {one}"));
    }
    match (field(whois, &["country"]), field(whois, &["country_code"])) {
        (Some(c), Some(code)) => out.push(format!("  {c} ({code})")),
        (Some(c), None) => out.push(format!("  {c}")),
        _ => {}
    }
    if let Some(asn) = field(whois, &["asn"]) {
        out.push(format!("  {asn}"));
    }
    if let Some(help) = field(whois, &["helpop"]) {
        out.push(format!("  {help}"));
    }
    if let Some(away) = field(whois, &["away"]) {
        out.push(format!("  away: {away}"));
    }
    // Idle and signon arrive together (RPL_WHOISIDLE) and read as one fact:
    // how long quiet, and since when connected.
    let idle = field(whois, &["idle"]).and_then(|s| s.parse::<i64>().ok());
    let signon = field(whois, &["logon", "signon"]).and_then(|s| s.parse::<i64>().ok());
    match (idle, signon) {
        (Some(secs), Some(ts)) => {
            out.push(format!("  idle {}, signed on {}", human_idle(secs), local_time(ts)))
        }
        (Some(secs), None) => out.push(format!("  idle {}", human_idle(secs))),
        (None, Some(ts)) => out.push(format!("  signed on {}", local_time(ts))),
        (None, None) => {}
    }
    out
}

/// A unix timestamp as local wall-clock text. Falls back to the raw seconds if
/// the value is out of range, so a nonsense stamp still shows *something*
/// rather than dropping the line.
fn local_time(unix_secs: i64) -> String {
    gtk::glib::DateTime::from_unix_local(unix_secs)
        .ok()
        .and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| unix_secs.to_string())
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
    fn an_opered_whois_keeps_every_field_the_server_volunteered() {
        // Modelled on a real opered WHOIS: the formatted view used to drop the
        // modes, the real IP behind the cloak, and the free-form server lines
        // (security groups, IP reputation) — which are precisely the fields you
        // opered up to see. It was strictly less informative than the raw text.
        let w = json!({
            "nick": "Guest22618", "ident": "pez808",
            "hostname": "D22F3C53.2FBF0C20.4E6D4592.IP", "real_name": "pez808",
            "modes": "+iwxz",
            "actual_hostname": "*@172.19.0.3", "actual_ip": "172.19.0.3",
            "server": "irc.so", "server_info": "irc.so IRC Server",
            "channels": "#glossedglare",
            "secure": true,
            "special": [
                "is in security-groups: known-users,tls-users",
                "is using an IP with a reputation score of 633",
            ],
            "away": "afk since 2026-07-30 23:30:39-0400",
            "idle": 3800, "logon": 1785438177,
        });
        let lines = format(&w);
        let all = lines.join("\n");

        assert!(all.contains("modes +iwxz"), "modes dropped:\n{all}");
        assert!(all.contains("172.19.0.3"), "real IP dropped:\n{all}");
        assert!(all.contains("security-groups: known-users,tls-users"), "special dropped:\n{all}");
        assert!(all.contains("reputation score of 633"), "special dropped:\n{all}");
        assert!(all.contains("idle 1h 3m 20s"), "idle wrong:\n{all}");
        assert!(all.contains("signed on"), "signon dropped:\n{all}");
        assert!(all.contains("away: afk since"), "away dropped:\n{all}");
    }

    #[test]
    fn a_cloaked_host_equal_to_the_ip_is_not_printed_twice() {
        let lines = format(&json!({
            "nick": "x", "actual_hostname": "10.0.0.5", "actual_ip": "10.0.0.5",
        }));
        let line = lines.iter().find(|l| l.contains("connecting from")).expect("host line");
        assert_eq!(line.trim(), "connecting from 10.0.0.5", "duplicated: {line}");
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
