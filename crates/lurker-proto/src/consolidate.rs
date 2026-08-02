// Folding presence noise into per-identity summary lines.
//
// A faithful port of `shared/consolidate.ts`, which both first-party clients
// use and which §8 names as the canonical set. Matching it matters beyond
// looks: the page-sizing unit a client requests (`countBy:'renderable'`) is
// defined as "rows outside CONSOLIDATABLE_TYPES", so a client that folds a
// different set than the server sizes pages for gets short pages and the
// buffer visibly assembles itself as the reader watches.
//
// Algorithm:
//   1. Group consecutive consolidatable events into a run. Any other row ends it.
//   2. Within a run, accumulate an action sequence per identity:
//        J = join, L = leave (part/quit), R = rename, H = rehost (chghost).
//      A rename transfers the identity to the new key so the chain is followed.
//   3. Classify by the first and last J|L:
//        L…J → reconnected     J…J → joined
//        L…L → left            J…L → joinedAndLeft
//      An identity with no J|L falls back to rename over rehost.
//   4. A run of exactly one event passes through unchanged.

use std::collections::{HashMap, HashSet};

use crate::event::{EventType, MessageEvent};

/// Default cap on names shown per category before "and N others".
pub const DEFAULT_MAX_NAMES: usize = 5;

/// Per-identity action within a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Join,
    Leave,
    Rename,
    Rehost,
}

/// The six ways a run's net effect on an identity can be classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConsolidationKind {
    Joined,
    Left,
    Reconnected,
    JoinedAndLeft,
    Renamed,
    Rehosted,
}

impl ConsolidationKind {
    /// Fixed display order, so the readout reads the same way every time.
    const ORDER: [Self; 6] = [
        Self::Joined,
        Self::Left,
        Self::Reconnected,
        Self::JoinedAndLeft,
        Self::Renamed,
        Self::Rehosted,
    ];

    fn verb(self) -> &'static str {
        match self {
            Self::Joined => "joined",
            Self::Left => "left",
            Self::Reconnected => "reconnected",
            Self::JoinedAndLeft => "joined briefly",
            Self::Renamed => "",
            Self::Rehosted => "changed host",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Nick(String),
    Rename { from: String, to: String },
}

impl Entry {
    fn sort_name(&self) -> &str {
        match self {
            Self::Nick(n) => n,
            Self::Rename { to, .. } => to,
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Nick(n) => n.clone(),
            Self::Rename { from, to } => format!("{from} → {to}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub kind: ConsolidationKind,
    pub visible: Vec<Entry>,
    pub hidden: usize,
}

/// A synthetic row replacing a run of folded presence events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub groups: Vec<Group>,
    pub event_count: usize,
    pub time: Option<String>,
    pub first_id: Option<i64>,
    pub last_id: Option<i64>,
}

impl Summary {
    /// One line, e.g. `alice and bob joined; carol left; dave → dan`.
    pub fn render(&self) -> String {
        self.groups
            .iter()
            .map(|g| {
                let names: Vec<String> = g.visible.iter().map(Entry::render).collect();
                let listed = join_names(&names, g.hidden);
                match g.kind {
                    // A rename reads as "a → b" with no verb attached.
                    ConsolidationKind::Renamed => listed,
                    kind => format!("{listed} {}", kind.verb()),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// `alice`, `alice and bob`, `alice, bob and carol`, `alice, bob and 3 others`.
fn join_names(names: &[String], hidden: usize) -> String {
    let mut parts: Vec<String> = names.to_vec();
    if hidden > 0 {
        parts.push(format!("{hidden} other{}", if hidden == 1 { "" } else { "s" }));
    }
    match parts.len() {
        0 => String::new(),
        1 => parts.remove(0),
        _ => {
            let last = parts.pop().expect("len >= 2");
            format!("{} and {last}", parts.join(", "))
        }
    }
}

/// A row in the rendered stream: either a passed-through event or a summary.
#[derive(Clone, Debug)]
pub enum Row<'a> {
    Event(&'a MessageEvent),
    Summary(Summary),
}

#[derive(Clone, Debug)]
pub struct Options {
    pub enabled: bool,
    /// Nicks who spoke recently; these float to the top of a capped list so a
    /// name the reader cares about is not the one hidden behind "and N others".
    pub recent_speakers: Option<HashSet<String>>,
    pub max_names: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { enabled: true, recent_speakers: None, max_names: DEFAULT_MAX_NAMES }
    }
}

#[derive(Clone, Debug)]
struct Identity {
    display_nick: String,
    original_nick: String,
    actions: Vec<Action>,
}

fn classify(actions: &[Action]) -> ConsolidationKind {
    let presence: Vec<Action> = actions
        .iter()
        .copied()
        .filter(|a| matches!(a, Action::Join | Action::Leave))
        .collect();

    // No presence change: a rename outranks a rehost, since "alice → bob" says
    // more than "alice changed host" for an identity that did both.
    if presence.is_empty() {
        return if actions.contains(&Action::Rename) {
            ConsolidationKind::Renamed
        } else {
            ConsolidationKind::Rehosted
        };
    }

    // `chghost` is deliberately transparent to this scan (#593): after a
    // netsplit each rejoining user emits JOIN then CHGHOST as they identify, so
    // the sequence is [J, H]. That must read as a plain "joined" rather than
    // splitting the summary into "N joined" plus "N changed host".
    let was_present = presence[0] == Action::Leave;
    let is_present = presence[presence.len() - 1] == Action::Join;
    match (was_present, is_present) {
        (false, true) => ConsolidationKind::Joined,
        (true, false) => ConsolidationKind::Left,
        (false, false) => ConsolidationKind::JoinedAndLeft,
        (true, true) => ConsolidationKind::Reconnected,
    }
}

/// Cap a bucket, floating recent speakers to the front while otherwise keeping
/// first-seen order (a stable sort).
fn cap(entries: Vec<Entry>, max_names: usize, recent: Option<&HashSet<String>>) -> (Vec<Entry>, usize) {
    if entries.len() <= max_names {
        return (entries, 0);
    }
    let mut ranked = entries;
    ranked.sort_by_key(|e| {
        // fold(), not ascii-lowercase: the recent set is keyed by IRC
        // casefold, where `Alice[]` and `alice{}` are the same person.
        let name = crate::casefold::fold(e.sort_name());
        u8::from(!recent.is_some_and(|r| r.contains(&name)))
    });
    let hidden = ranked.len() - max_names;
    ranked.truncate(max_names);
    (ranked, hidden)
}

fn consolidate_run(events: &[&MessageEvent], opts: &Options) -> Vec<Group> {
    let recent: Option<HashSet<String>> = opts
        .recent_speakers
        .as_ref()
        .map(|s| s.iter().map(|n| crate::casefold::fold(n)).collect());
    let max_names = opts.max_names.max(1);

    // Insertion order is preserved explicitly rather than by map iteration,
    // because a rename moves an identity to a new key and would otherwise
    // re-insert it at the end — losing when it first appeared in the run.
    let mut order: Vec<Identity> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    for e in events {
        let nick = e.nick.clone().unwrap_or_default();
        let lc = nick.to_ascii_lowercase();

        if e.event_type == EventType::Nick {
            let new_nick = e.new_nick.clone().unwrap_or_default();
            let new_lc = new_nick.to_ascii_lowercase();
            match index.remove(&lc) {
                Some(i) => {
                    order[i].actions.push(Action::Rename);
                    order[i].display_nick = new_nick;
                    index.insert(new_lc, i);
                }
                None => {
                    order.push(Identity {
                        display_nick: new_nick,
                        original_nick: nick,
                        actions: vec![Action::Rename],
                    });
                    index.insert(new_lc, order.len() - 1);
                }
            }
            continue;
        }

        let i = *index.entry(lc).or_insert_with(|| {
            order.push(Identity {
                display_nick: nick.clone(),
                original_nick: nick.clone(),
                actions: Vec::new(),
            });
            order.len() - 1
        });
        match e.event_type {
            EventType::Join => order[i].actions.push(Action::Join),
            EventType::Part | EventType::Quit => order[i].actions.push(Action::Leave),
            EventType::Chghost => order[i].actions.push(Action::Rehost),
            _ => {}
        }
    }

    let mut buckets: HashMap<ConsolidationKind, Vec<Entry>> = HashMap::new();
    for id in &order {
        let kind = classify(&id.actions);
        let entry = if kind == ConsolidationKind::Renamed {
            Entry::Rename { from: id.original_nick.clone(), to: id.display_nick.clone() }
        } else {
            Entry::Nick(id.display_nick.clone())
        };
        buckets.entry(kind).or_default().push(entry);
    }

    ConsolidationKind::ORDER
        .iter()
        .filter_map(|kind| {
            let entries = buckets.remove(kind)?;
            if entries.is_empty() {
                return None;
            }
            let (visible, hidden) = cap(entries, max_names, recent.as_ref());
            Some(Group { kind: *kind, visible, hidden })
        })
        .collect()
}

/// Fold runs of presence noise in `events` into summary rows.
///
/// A run of exactly one event is passed through unchanged, so a lone
/// "alice joined" still renders with its familiar `-->` styling.
pub fn consolidate<'a>(events: &'a [MessageEvent], opts: &Options) -> Vec<Row<'a>> {
    if !opts.enabled {
        return events.iter().map(Row::Event).collect();
    }

    let mut out: Vec<Row<'a>> = Vec::new();
    let mut run: Vec<&'a MessageEvent> = Vec::new();

    let flush = |run: &mut Vec<&'a MessageEvent>, out: &mut Vec<Row<'a>>| {
        match run.len() {
            0 => {}
            1 => out.push(Row::Event(run[0])),
            _ => {
                let groups = consolidate_run(run, opts);
                out.push(Row::Summary(Summary {
                    groups,
                    event_count: run.len(),
                    time: run[run.len() - 1].time.clone(),
                    first_id: run[0].id,
                    last_id: run[run.len() - 1].id,
                }));
            }
        }
        run.clear();
    };

    for e in events {
        if e.event_type.is_consolidatable() {
            run.push(e);
        } else {
            flush(&mut run, &mut out);
            out.push(Row::Event(e));
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: EventType, nick: &str) -> MessageEvent {
        MessageEvent {
            event_type: t,
            nick: Some(nick.into()),
            id: Some(1),
            network_id: Some(1),
            ..Default::default()
        }
    }

    fn renamed(from: &str, to: &str) -> MessageEvent {
        MessageEvent { new_nick: Some(to.into()), ..ev(EventType::Nick, from) }
    }

    fn summary_of(events: &[MessageEvent]) -> String {
        let rows = consolidate(events, &Options::default());
        assert_eq!(rows.len(), 1, "expected a single folded row");
        match &rows[0] {
            Row::Summary(s) => s.render(),
            Row::Event(_) => panic!("expected a summary"),
        }
    }

    #[test]
    fn a_lone_event_passes_through_unchanged() {
        let events = vec![ev(EventType::Join, "alice")];
        let rows = consolidate(&events, &Options::default());
        assert!(matches!(rows[0], Row::Event(_)), "a run of one must not become a summary");
    }

    #[test]
    fn non_consolidatable_rows_break_a_run() {
        let events = vec![
            ev(EventType::Join, "alice"),
            ev(EventType::Join, "bob"),
            ev(EventType::Message, "carol"),
            ev(EventType::Quit, "dave"),
            ev(EventType::Quit, "erin"),
        ];
        let rows = consolidate(&events, &Options::default());
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], Row::Summary(_)));
        assert!(matches!(rows[1], Row::Event(_)));
        assert!(matches!(rows[2], Row::Summary(_)));
    }

    #[test]
    fn classifies_the_four_presence_outcomes() {
        // J…J
        assert_eq!(
            summary_of(&[ev(EventType::Join, "alice"), ev(EventType::Join, "bob")]),
            "alice and bob joined"
        );
        // L…L
        assert_eq!(
            summary_of(&[ev(EventType::Quit, "alice"), ev(EventType::Part, "bob")]),
            "alice and bob left"
        );
        // L…J — was present, left, came back.
        assert_eq!(
            summary_of(&[
                ev(EventType::Quit, "alice"),
                ev(EventType::Join, "alice"),
                ev(EventType::Join, "bob"),
            ]),
            "bob joined; alice reconnected"
        );
        // J…L — was absent, appeared, gone again.
        assert_eq!(
            summary_of(&[
                ev(EventType::Join, "alice"),
                ev(EventType::Part, "alice"),
                ev(EventType::Join, "bob"),
            ]),
            "bob joined; alice joined briefly"
        );
    }

    #[test]
    fn chghost_is_transparent_to_the_presence_scan() {
        // #593: post-netsplit each rejoining user emits JOIN then CHGHOST as
        // they identify. That must read as a plain "joined", not split into
        // "N joined" plus "N changed host".
        let events = vec![
            ev(EventType::Join, "alice"),
            ev(EventType::Chghost, "alice"),
            ev(EventType::Join, "bob"),
            ev(EventType::Chghost, "bob"),
        ];
        assert_eq!(summary_of(&events), "alice and bob joined");
    }

    #[test]
    fn a_host_change_alone_earns_its_own_category() {
        let events = vec![ev(EventType::Chghost, "alice"), ev(EventType::Chghost, "bob")];
        assert_eq!(summary_of(&events), "alice and bob changed host");
    }

    #[test]
    fn a_rename_follows_the_identity_chain() {
        let events = vec![renamed("alice", "alicia"), renamed("alicia", "ally")];
        // One identity, not three: the rename transfers the key each time.
        assert_eq!(summary_of(&events), "alice → ally");
    }

    #[test]
    fn a_rename_outranks_a_rehost_for_one_identity() {
        let events = vec![
            ev(EventType::Chghost, "alice"),
            renamed("alice", "alicia"),
            ev(EventType::Join, "bob"),
        ];
        assert_eq!(summary_of(&events), "bob joined; alice → alicia");
    }

    #[test]
    fn a_rename_then_leave_reads_as_a_leave_under_the_new_nick() {
        let events = vec![
            renamed("alice", "alicia"),
            ev(EventType::Quit, "alicia"),
            ev(EventType::Join, "bob"),
        ];
        assert_eq!(summary_of(&events), "bob joined; alicia left");
    }

    #[test]
    fn identity_keys_are_case_insensitive() {
        let events = vec![
            ev(EventType::Join, "Alice"),
            ev(EventType::Part, "alice"),
            ev(EventType::Join, "bob"),
        ];
        // One identity that joined and left, not two separate people.
        assert_eq!(summary_of(&events), "bob joined; Alice joined briefly");
    }

    #[test]
    fn names_beyond_the_cap_collapse_into_a_count() {
        let events: Vec<MessageEvent> =
            (0..8).map(|i| ev(EventType::Join, &format!("user{i}"))).collect();
        let rendered = summary_of(&events);
        assert!(rendered.ends_with("and 3 others joined"), "got {rendered:?}");
        assert!(rendered.starts_with("user0, user1"), "first-seen order is kept");
    }

    #[test]
    fn recent_speakers_survive_the_cap() {
        // The whole point of the ranking: a name the reader was just talking to
        // must not be the one hidden behind "and N others".
        let events: Vec<MessageEvent> =
            (0..8).map(|i| ev(EventType::Join, &format!("user{i}"))).collect();
        let opts = Options {
            recent_speakers: Some(["USER7".to_string()].into_iter().collect()),
            ..Options::default()
        };
        let rows = consolidate(&events, &opts);
        let Row::Summary(s) = &rows[0] else { panic!() };
        assert!(
            s.render().contains("user7"),
            "a recent speaker must float above the cap: {}",
            s.render()
        );
        assert_eq!(s.groups[0].hidden, 3);
    }

    #[test]
    fn summary_reports_the_run_extent() {
        let mut first = ev(EventType::Join, "alice");
        first.id = Some(10);
        let mut last = ev(EventType::Quit, "bob");
        last.id = Some(14);
        last.time = Some("2026-07-28T00:00:00Z".into());
        let events = vec![first, last];
        let rows = consolidate(&events, &Options::default());
        let Row::Summary(s) = &rows[0] else { panic!() };
        assert_eq!((s.first_id, s.last_id, s.event_count), (Some(10), Some(14), 2));
        assert_eq!(s.time.as_deref(), Some("2026-07-28T00:00:00Z"));
    }

    #[test]
    fn disabled_options_pass_everything_through() {
        let events = vec![ev(EventType::Join, "alice"), ev(EventType::Join, "bob")];
        let opts = Options { enabled: false, ..Options::default() };
        let rows = consolidate(&events, &opts);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| matches!(r, Row::Event(_))));
    }
}
