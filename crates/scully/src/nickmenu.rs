// The nicklist context menu's commands, as pure verb construction.
//
// Every menu item maps through [`verbs_for`] so the whole surface — including
// all ten qaohv mode changes — is unit-tested without a display. The window
// renders the menu and sends whatever this returns.
//
// Mode changes, kick and ban go over `raw` — the documented escape hatch for
// commands without typed verbs (§6). CTCP uses its typed verb so the status
// line surfaces in the right buffer. Query uses `open-buffer`, which is
// exactly the "explicit user intent" case that verb is reserved for (§4.3).

use gtk::gio;
use gtk::prelude::*;
use lurker_proto::ClientVerb;

/// Menu commands. String ids double as the GAction target values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Whois,
    Query,
    // ── Channel user modes, give and take, highest to lowest ──
    GiveOwner,   // +q
    TakeOwner,   // -q
    GiveAdmin,   // +a
    TakeAdmin,   // -a
    GiveOp,      // +o
    TakeOp,      // -o
    GiveHalfop,  // +h
    TakeHalfop,  // -h
    GiveVoice,   // +v
    TakeVoice,   // -v
    // ── Removal ──
    Kick,
    Ban,
    KickBan,
    // ── CTCP ──
    CtcpPing,
    CtcpVersion,
    CtcpTime,
    CtcpClientinfo,
    // ── Misc ──
    Ignore,
    Slap,
}

/// Channel-authority rank, highest to lowest. `None` is an ordinary user.
///
/// Standard IRC hierarchy; the letters come from the member's prefix modes
/// (the server's `PREFIX` order, `qaohv`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    None,
    Voice,
    Halfop,
    Op,
    Admin,
    Owner,
}

impl Rank {
    /// From a member's highest prefix-mode letter.
    pub fn from_mode(letter: Option<&str>) -> Self {
        match letter {
            Some("q") => Self::Owner,
            Some("a") => Self::Admin,
            Some("o") => Self::Op,
            Some("h") => Self::Halfop,
            Some("v") => Self::Voice,
            _ => Self::None,
        }
    }
}

impl Cmd {
    /// The minimum rank the CLICKER needs for this command to do anything.
    ///
    /// `None` means no privilege at all (whois, query, ctcp, slap, ignore —
    /// things you do to a nick, not to the channel). Gating the menu on this
    /// hides actions the user cannot perform, rather than letting them fire a
    /// MODE the server will just reject.
    ///
    /// The hierarchy: you can only grant/revoke a status you outrank. Setting
    /// op needs op+ (an op can op others); setting admin needs admin+; owner
    /// needs owner. Kick/ban follow the common op-and-up convention (halfops
    /// vary by server, so op is the safe floor for showing them).
    pub fn min_rank(self) -> Rank {
        match self {
            Self::GiveOwner | Self::TakeOwner => Rank::Owner,
            Self::GiveAdmin | Self::TakeAdmin => Rank::Admin,
            Self::GiveOp | Self::TakeOp => Rank::Op,
            Self::GiveHalfop | Self::TakeHalfop => Rank::Op,
            Self::GiveVoice | Self::TakeVoice => Rank::Halfop,
            Self::Kick | Self::KickBan => Rank::Halfop,
            Self::Ban => Rank::Op,
            // No channel privilege required.
            Self::Whois
            | Self::Query
            | Self::Slap
            | Self::Ignore
            | Self::CtcpPing
            | Self::CtcpVersion
            | Self::CtcpTime
            | Self::CtcpClientinfo => Rank::None,
        }
    }

    /// Whether a clicker of `mine` rank may use this command.
    pub fn permitted_for(self, mine: Rank) -> bool {
        mine >= self.min_rank()
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "whois" => Self::Whois,
            "query" => Self::Query,
            "give-q" => Self::GiveOwner,
            "take-q" => Self::TakeOwner,
            "give-a" => Self::GiveAdmin,
            "take-a" => Self::TakeAdmin,
            "give-o" => Self::GiveOp,
            "take-o" => Self::TakeOp,
            "give-h" => Self::GiveHalfop,
            "take-h" => Self::TakeHalfop,
            "give-v" => Self::GiveVoice,
            "take-v" => Self::TakeVoice,
            "kick" => Self::Kick,
            "ban" => Self::Ban,
            "kickban" => Self::KickBan,
            "ctcp-ping" => Self::CtcpPing,
            "ctcp-version" => Self::CtcpVersion,
            "ctcp-time" => Self::CtcpTime,
            "ctcp-clientinfo" => Self::CtcpClientinfo,
            "ignore" => Self::Ignore,
            "slap" => Self::Slap,
            _ => return None,
        })
    }

    fn mode_change(self) -> Option<&'static str> {
        Some(match self {
            Self::GiveOwner => "+q",
            Self::TakeOwner => "-q",
            Self::GiveAdmin => "+a",
            Self::TakeAdmin => "-a",
            Self::GiveOp => "+o",
            Self::TakeOp => "-o",
            Self::GiveHalfop => "+h",
            Self::TakeHalfop => "-h",
            Self::GiveVoice => "+v",
            Self::TakeVoice => "-v",
            _ => return None,
        })
    }

    fn ctcp_type(self) -> Option<&'static str> {
        Some(match self {
            Self::CtcpPing => "PING",
            Self::CtcpVersion => "VERSION",
            Self::CtcpTime => "TIME",
            Self::CtcpClientinfo => "CLIENTINFO",
            _ => return None,
        })
    }
}

/// The ban mask for a member: `*!*@host` when the host is known
/// (userhost-in-names / WHO data), else the nick-only fallback `nick!*@*`.
/// Host bans survive nick changes; nick bans are better than nothing.
pub fn ban_mask(nick: &str, host: Option<&str>) -> String {
    match host {
        Some(h) if !h.is_empty() => format!("*!*@{h}"),
        _ => format!("{nick}!*@*"),
    }
}

/// The verbs a menu command sends. `channel` is the buffer the menu was opened
/// in (also the CTCP `issuing_target`); `host` is the member's known host for
/// ban masks. `Query` is not handled here — opening a DM changes window state,
/// which is the caller's job.
pub fn verbs_for(
    cmd: Cmd,
    network_id: i64,
    channel: &str,
    nick: &str,
    host: Option<&str>,
) -> Vec<ClientVerb> {
    if let Some(change) = cmd.mode_change() {
        return vec![ClientVerb::Raw {
            network_id,
            line: format!("MODE {channel} {change} {nick}"),
        }];
    }
    if let Some(ctcp) = cmd.ctcp_type() {
        return vec![ClientVerb::Ctcp {
            network_id,
            target: nick.to_string(),
            ctcp_type: ctcp.to_string(),
            args: String::new(),
            issuing_target: channel.to_string(),
        }];
    }
    match cmd {
        Cmd::Whois => vec![ClientVerb::Raw {
            network_id,
            // The doubled nick asks the server the target is on for idle/away
            // detail rather than a summary from a hub.
            line: format!("WHOIS {nick} {nick}"),
        }],
        Cmd::Kick => vec![ClientVerb::Raw {
            network_id,
            line: format!("KICK {channel} {nick}"),
        }],
        Cmd::Ban => vec![ClientVerb::Raw {
            network_id,
            line: format!("MODE {channel} +b {}", ban_mask(nick, host)),
        }],
        // Ban first, then kick — the reverse gives them a rejoin window.
        Cmd::KickBan => vec![
            ClientVerb::Raw {
                network_id,
                line: format!("MODE {channel} +b {}", ban_mask(nick, host)),
            },
            ClientVerb::Raw {
                network_id,
                line: format!("KICK {channel} {nick}"),
            },
        ],
        // Ignore is GLOBAL by scope and by-identity by mask, matching the
        // web's defaults (#350 / IgnoreModal): field-reported broken when it
        // was pinned to the clicked network with a bare-nick mask — ignore a
        // pest on one network and their messages kept flowing from another,
        // and a /nick change would have shed the rule entirely. Host-keyed
        // when we know the host (survives renames), nick!*@* otherwise.
        Cmd::Ignore => vec![ClientVerb::AddIgnore {
            network_id: None,
            rule: serde_json::json!({
                "mask": ban_mask(nick, host),
                "levels": ["ALL"],
            }),
        }],
        Cmd::Slap => vec![ClientVerb::Action {
            network_id,
            target: channel.to_string(),
            text: format!("slaps {nick} around a bit with a large trout"),
            client_id: None,
        }],
        // Handled above or by the caller.
        Cmd::Query | _ => Vec::new(),
    }
}

/// Build the nicklist context menu for a clicker of the given `rank`.
///
/// Items the clicker lacks authority for ([`Cmd::permitted_for`]) are omitted
/// entirely — an op sees op/voice/kick/ban but not the owner/admin controls,
/// and an ordinary user sees only whois/query/slap/ctcp/ignore. Empty
/// submenus are dropped so no dead "Modes ▸" stub remains.
pub fn menu_model(rank: Rank) -> gio::Menu {
    let item = |label: &str, id: &str| {
        gio::MenuItem::new(Some(label), Some(&format!("nick.cmd::{id}")))
    };
    let show = |cmd: Cmd| cmd.permitted_for(rank);

    let root = gio::Menu::new();

    // Info & classics — always available.
    let info = gio::Menu::new();
    info.append_item(&item("Whois", "whois"));
    info.append_item(&item("Query (open DM)", "query"));
    info.append_item(&item("Slap", "slap"));
    root.append_section(None, &info);

    // Mode ladder — only the rungs the clicker can set. "−" is U+2212.
    let modes = gio::Menu::new();
    let mode_items = [
        (Cmd::GiveOwner, "Give owner (+q)", "give-q"),
        (Cmd::TakeOwner, "Take owner (\u{2212}q)", "take-q"),
        (Cmd::GiveAdmin, "Give admin (+a)", "give-a"),
        (Cmd::TakeAdmin, "Take admin (\u{2212}a)", "take-a"),
        (Cmd::GiveOp, "Give op (+o)", "give-o"),
        (Cmd::TakeOp, "Take op (\u{2212}o)", "take-o"),
        (Cmd::GiveHalfop, "Give halfop (+h)", "give-h"),
        (Cmd::TakeHalfop, "Take halfop (\u{2212}h)", "take-h"),
        (Cmd::GiveVoice, "Give voice (+v)", "give-v"),
        (Cmd::TakeVoice, "Take voice (\u{2212}v)", "take-v"),
    ];
    for (cmd, label, id) in mode_items {
        if show(cmd) {
            modes.append_item(&item(label, id));
        }
    }
    if modes.n_items() > 0 {
        root.append_submenu(Some("Modes"), &modes);
    }

    // Removal.
    let removal = gio::Menu::new();
    for (cmd, label, id) in [
        (Cmd::Kick, "Kick", "kick"),
        (Cmd::Ban, "Ban", "ban"),
        (Cmd::KickBan, "Kick + ban", "kickban"),
    ] {
        if show(cmd) {
            removal.append_item(&item(label, id));
        }
    }
    if removal.n_items() > 0 {
        root.append_submenu(Some("Kick / ban"), &removal);
    }

    // CTCP — no privilege.
    let ctcp = gio::Menu::new();
    ctcp.append_item(&item("Ping", "ctcp-ping"));
    ctcp.append_item(&item("Version", "ctcp-version"));
    ctcp.append_item(&item("Time", "ctcp-time"));
    ctcp.append_item(&item("Clientinfo", "ctcp-clientinfo"));
    root.append_submenu(Some("CTCP"), &ctcp);

    // Ignore — client-side, always available.
    let danger = gio::Menu::new();
    danger.append_item(&item("Ignore this nick", "ignore"));
    root.append_section(None, &danger);

    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_lines(cmd: Cmd) -> Vec<String> {
        verbs_for(cmd, 1, "#chan", "troublemaker", Some("evil.example.net"))
            .into_iter()
            .map(|v| match v {
                ClientVerb::Raw { line, .. } => line,
                other => panic!("expected raw, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn every_qaohv_mode_has_give_and_take() {
        // The full ladder, both directions — the thoroughness the menu
        // promises. Each is a single MODE line naming channel, change, nick.
        let cases = [
            (Cmd::GiveOwner, "MODE #chan +q troublemaker"),
            (Cmd::TakeOwner, "MODE #chan -q troublemaker"),
            (Cmd::GiveAdmin, "MODE #chan +a troublemaker"),
            (Cmd::TakeAdmin, "MODE #chan -a troublemaker"),
            (Cmd::GiveOp, "MODE #chan +o troublemaker"),
            (Cmd::TakeOp, "MODE #chan -o troublemaker"),
            (Cmd::GiveHalfop, "MODE #chan +h troublemaker"),
            (Cmd::TakeHalfop, "MODE #chan -h troublemaker"),
            (Cmd::GiveVoice, "MODE #chan +v troublemaker"),
            (Cmd::TakeVoice, "MODE #chan -v troublemaker"),
        ];
        for (cmd, expected) in cases {
            assert_eq!(raw_lines(cmd), [expected], "{cmd:?}");
        }
    }

    #[test]
    fn ban_prefers_the_host_mask() {
        assert_eq!(ban_mask("bob", Some("evil.example.net")), "*!*@evil.example.net");
        assert_eq!(ban_mask("bob", None), "bob!*@*", "no host → nick mask");
        assert_eq!(ban_mask("bob", Some("")), "bob!*@*", "empty host → nick mask");
        assert_eq!(raw_lines(Cmd::Ban), ["MODE #chan +b *!*@evil.example.net"]);
    }

    #[test]
    fn kickban_bans_before_kicking() {
        // The other order hands the target a rejoin window.
        let lines = raw_lines(Cmd::KickBan);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("MODE #chan +b"), "ban first: {lines:?}");
        assert!(lines[1].starts_with("KICK "), "kick second: {lines:?}");
    }

    #[test]
    fn ctcp_uses_the_typed_verb_with_the_issuing_channel() {
        let verbs = verbs_for(Cmd::CtcpVersion, 2, "#chan", "bob", None);
        let [ClientVerb::Ctcp { network_id, target, ctcp_type, issuing_target, .. }] =
            verbs.as_slice()
        else {
            panic!("expected one ctcp verb, got {verbs:?}");
        };
        assert_eq!((*network_id, target.as_str()), (2, "bob"));
        assert_eq!(ctcp_type, "VERSION");
        assert_eq!(issuing_target, "#chan");
    }

    #[test]
    fn ignore_is_global_and_keyed_to_identity() {
        // The web's defaults (#350): global scope — ignoring a pest on one
        // network must silence them on all of them (field-reported broken
        // when this was Some(network)) — and a host mask when the host is
        // known, so a /nick change doesn't shed the rule.
        let verbs = verbs_for(Cmd::Ignore, 3, "#chan", "spammer", Some("bad.example.net"));
        let [ClientVerb::AddIgnore { network_id, rule }] = verbs.as_slice() else {
            panic!("expected add-ignore, got {verbs:?}");
        };
        assert_eq!(*network_id, None, "global, not per-network");
        assert_eq!(rule["mask"], "*!*@bad.example.net");

        // No host known yet: fall back to a nick mask that can't over-match.
        let verbs = verbs_for(Cmd::Ignore, 3, "#chan", "spammer", None);
        let [ClientVerb::AddIgnore { rule, .. }] = verbs.as_slice() else {
            panic!("expected add-ignore, got {verbs:?}");
        };
        assert_eq!(rule["mask"], "spammer!*@*");
    }

    #[test]
    fn slap_is_an_action_with_the_traditional_trout() {
        let verbs = verbs_for(Cmd::Slap, 1, "#chan", "amiantos", None);
        let [ClientVerb::Action { target, text, .. }] = verbs.as_slice() else {
            panic!("expected action, got {verbs:?}");
        };
        assert_eq!(target, "#chan");
        assert_eq!(text, "slaps amiantos around a bit with a large trout");
    }

    #[test]
    fn authority_gates_mode_changes_by_rank() {
        // An op can op/deop, voice/devoice, kick and ban — but cannot touch
        // owner or admin status.
        let op = Rank::Op;
        assert!(Cmd::GiveOp.permitted_for(op));
        assert!(Cmd::TakeOp.permitted_for(op));
        assert!(Cmd::GiveVoice.permitted_for(op));
        assert!(Cmd::Kick.permitted_for(op));
        assert!(Cmd::Ban.permitted_for(op));
        assert!(!Cmd::GiveOwner.permitted_for(op), "an op cannot make owners");
        assert!(!Cmd::GiveAdmin.permitted_for(op), "an op cannot make admins");

        // An owner can do everything.
        let owner = Rank::Owner;
        for cmd in [Cmd::GiveOwner, Cmd::GiveAdmin, Cmd::GiveOp, Cmd::Kick, Cmd::Ban] {
            assert!(cmd.permitted_for(owner), "{cmd:?}");
        }

        // A voiced user has no channel authority.
        let voice = Rank::Voice;
        for cmd in [Cmd::GiveOp, Cmd::GiveVoice, Cmd::Kick, Cmd::Ban] {
            assert!(!cmd.permitted_for(voice), "{cmd:?}");
        }
    }

    #[test]
    fn privilege_free_actions_are_always_permitted() {
        // whois/query/slap/ctcp/ignore need no rank — a plain user sees them.
        let none = Rank::None;
        for cmd in [
            Cmd::Whois, Cmd::Query, Cmd::Slap, Cmd::Ignore,
            Cmd::CtcpPing, Cmd::CtcpVersion, Cmd::CtcpTime, Cmd::CtcpClientinfo,
        ] {
            assert!(cmd.permitted_for(none), "{cmd:?} should need no privilege");
        }
    }

    #[test]
    fn rank_from_mode_letter_orders_correctly() {
        assert!(Rank::from_mode(Some("q")) > Rank::from_mode(Some("o")));
        assert!(Rank::from_mode(Some("o")) > Rank::from_mode(Some("v")));
        assert_eq!(Rank::from_mode(None), Rank::None);
        assert_eq!(Rank::from_mode(Some("x")), Rank::None, "unknown letter → none");
    }

    #[test]
    fn every_menu_id_round_trips() {
        for id in [
            "whois", "query", "give-q", "take-q", "give-a", "take-a", "give-o", "take-o",
            "give-h", "take-h", "give-v", "take-v", "kick", "ban", "kickban", "ctcp-ping",
            "ctcp-version", "ctcp-time", "ctcp-clientinfo", "ignore", "slap",
        ] {
            assert!(Cmd::from_id(id).is_some(), "unmapped menu id {id}");
        }
        assert!(Cmd::from_id("nonsense").is_none());
    }
}
