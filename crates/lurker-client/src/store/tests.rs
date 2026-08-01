// Tests for the reducer.
//
// Frames are built as JSON and parsed through the real deserializers rather
// than constructed as Rust structs, so each test exercises the wire shape the
// server actually sends.

use super::*;
use lurker_proto::ServerFrame;
use serde_json::json;

fn frame(v: serde_json::Value) -> ServerFrame {
    serde_json::from_value(v).expect("frame should parse")
}

fn apply(store: &mut Store, v: serde_json::Value) -> Vec<StoreEvent> {
    store.apply_frame(frame(v))
}

/// A persisted chat message on network 1.
fn msg(id: i64, target: &str, nick: &str, text: &str) -> serde_json::Value {
    json!({
        "kind": "irc", "type": "message", "id": id, "networkId": 1,
        "target": target, "nick": nick, "text": text, "time": "2026-07-28T00:00:00Z"
    })
}

fn backlog(target: &str, mode: &str, events: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "kind": "backlog", "networkId": 1, "target": target, "mode": mode,
        "events": events, "hasMoreOlder": false, "joined": true
    })
}

/// A row as it appears inside `events[]` (no envelope `kind`).
fn row(id: i64, text: &str) -> serde_json::Value {
    json!({
        "type": "message", "kind": "privmsg", "id": id, "networkId": 1,
        "target": "#chat", "nick": "someone", "text": text, "time": "2026-07-28T00:00:00Z"
    })
}

fn key(target: &str) -> BufferKey {
    BufferKey::new(Some(1), target)
}

fn texts(store: &Store, target: &str) -> Vec<String> {
    store.buffer(&key(target))
        .map(|b| b.events.iter().filter_map(|e| e.text.clone()).collect())
        .unwrap_or_default()
}

// ── End-to-end, from real captured frames ─────────────────────────────────

#[test]
fn the_real_connect_and_hydrate_sequence_fills_the_channel() {
    // Captured from a live v1.1.5 server. This is the exact path that was
    // broken: a shell arrives for the channel, the user opens it, and the
    // `history`/`latest` reply hydrates it. A single mistyped field (`alt`,
    // which is a boolean) made every one of these frames fail to parse, so the
    // buffer list populated from shells while every channel stayed empty and
    // sent messages never echoed back. Parsing real bytes here is the point —
    // hand-written fixtures are where that bug hid.
    let mut store = Store::new();

    // 1. The server log, with the id-suffixed target.
    apply(
        &mut store,
        json!({"kind":"backlog","networkId":1,"target":":server:1","mode":"replace",
               "hasMoreOlder":false,"joined":true,"events":[
            {"id":1,"networkId":1,"target":":server:1","time":"2026-07-29T00:30:24.606Z",
             "type":"notice","nick":"lurker","text":"Connecting to ircd:6667…","kind":"",
             "self":false,"userhost":null,"alt":false,"matched":false,"matchedRuleId":null,
             "fromIgnored":false,"mirrored":false,"dm":false,"notifyAlways":false,"notify":false}]}),
    );

    // 2. The channel arrives as a shell — exists, no content.
    apply(
        &mut store,
        json!({"kind":"backlog","networkId":1,"target":"#TestChan","mode":"shell",
               "events":[],"hasMoreOlder":true,"joined":true}),
    );
    apply(&mut store, json!({"kind":"backlog-complete"}));

    let chan = BufferKey::new(Some(1), "#TestChan");
    assert!(store.buffer(&chan).is_some(), "shell must materialise the buffer");
    assert!(!store.buffer(&chan).unwrap().hydrated, "a shell is not hydrated");
    assert!(store.buffer(&chan).unwrap().events.is_empty());

    // The server log keyed by its bare form despite the `:server:1` wire target.
    assert!(store.buffer(&BufferKey::server(1)).is_some());
    assert_eq!(store.buffer(&BufferKey::server(1)).unwrap().display_name, ":server:1");

    // 3. The hydrate reply.
    apply(
        &mut store,
        json!({"kind":"history","networkId":1,"target":"#TestChan","mode":"latest","token":1,
               "speakers":[],"hasMoreOlder":false,"hasMoreNewer":false,"inputHistory":[],
               "events":[
            {"id":20,"networkId":1,"target":"#TestChan","time":"2026-07-29T00:30:24.662Z",
             "type":"join","nick":"reprouser","text":null,"kind":"","self":false,
             "userhost":"reprouser!~u@x.irc","alt":false,"matched":false,"matchedRuleId":null,
             "fromIgnored":false,"mirrored":false,"dm":false,"notifyAlways":false,"notify":false},
            {"id":23,"networkId":1,"target":"#TestChan","time":"2026-07-29T00:31:16.088Z",
             "type":"message","nick":"reprouser","text":"probe message 1","kind":"privmsg",
             "self":true,"userhost":"reprouser!~u@x.irc","alt":false,"matched":false,
             "matchedRuleId":null,"fromIgnored":false,"mirrored":false,
             "msgid":"z6ufx7e2fkhyqrjv9thed4nfgs","dm":false,"notifyAlways":false,"notify":false},
            {"id":24,"networkId":1,"target":"#TestChan","time":"2026-07-29T00:31:16.088Z",
             "type":"message","nick":"reprouser","text":"probe message 2","kind":"privmsg",
             "self":true,"userhost":"reprouser!~u@x.irc","alt":true,"matched":false,
             "matchedRuleId":null,"fromIgnored":false,"mirrored":false,
             "msgid":"ya7gwhc929rfc96qjsa5d27nkw","dm":false,"notifyAlways":false,"notify":false}]}),
    );

    let buf = store.buffer(&chan).expect("channel still present");
    assert!(buf.hydrated, "a latest reply hydrates the buffer");
    assert_eq!(buf.events.len(), 3, "history events must land in the buffer");
    assert_eq!(
        buf.events.iter().filter_map(|e| e.text.clone()).collect::<Vec<_>>(),
        ["probe message 1", "probe message 2"]
    );
    // Only networked persisted rows advanced the cursor.
    assert_eq!(store.cursor(), 24);

    // 4. A live echo of our own send appends, rather than being dropped.
    apply(
        &mut store,
        json!({"type":"message","target":"#TestChan","nick":"reprouser",
               "text":"probe message 3","kind":"irc","self":true,
               "userhost":"reprouser!~u@x.irc","time":"2026-07-29T00:32:44.205Z",
               "msgid":"2napr9h2f5mxixcjdw7qiee7u2","userId":1,"networkId":1,"id":26,
               "alt":true,"matched":false,"matchedRuleId":null,"fromIgnored":false,
               "dm":false,"notifyAlways":false,"notify":false}),
    );
    let buf = store.buffer(&chan).unwrap();
    assert_eq!(buf.events.len(), 4, "the live self-echo must render (§9.3)");
    assert!(buf.events.back().unwrap().is_self);
    assert_eq!(store.cursor(), 26);
}

// ── Pin state across the snapshot/materialize ordering ────────────────────

#[test]
fn a_pinned_buffer_keeps_its_pin_when_it_materializes_after_the_snapshot() {
    // The snapshot (with the `pinned` list) arrives before any buffer exists,
    // so a pinned buffer materialising later from the backlog must still adopt
    // the pin — otherwise it renders under its network instead of the pinned
    // section after every reload.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"snapshot","networks":[
            {"networkId":1,"state":"connected","nick":"me","pinned":["#Chat"]}],
            "globalIgnores":[]}),
    );
    // The buffer doesn't exist yet; the pin was only recorded on the network.
    assert!(store.buffer(&key("#chat")).is_none());

    // Now it materialises from a backlog frame.
    apply(&mut store, backlog("#Chat", "replace", vec![row(1, "hi")]));
    assert!(store.buffer(&key("#chat")).unwrap().pinned, "pin must survive materialization");
}

// ── ISUPPORT caching ──────────────────────────────────────────────────────

#[test]
fn isupport_is_cached_from_the_server_backlog_and_survives_eviction() {
    // The 005 burst arrives in the :server: backlog at connect. Caching it in
    // the Network means a channel-mode dialog opened later sees the full
    // CHANMODES even after the ring evicted the original lines — the bug the
    // user hit on Libera, where the dialog fell back to the RFC minimum.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"snapshot","networks":[{"networkId":1,"state":"connected","nick":"me"}],
               "globalIgnores":[]}),
    );

    let isupport_line = "CHANMODES=eIbq,k,flj,CFLMPQRSTcgimnprstuz PREFIX=(qaohv)~&@%+ \
                         TOPICLEN=390 EXCEPTS INVEX are supported by this server";
    apply(
        &mut store,
        json!({"kind":"backlog","networkId":1,"target":":server:1","mode":"replace",
               "hasMoreOlder":false,"events":[
            {"id":1,"networkId":1,"target":":server:1","type":"motd","text":isupport_line}]}),
    );

    let net = store.networks.get(&1).expect("network 1");
    assert!(net.isupport.advertised(), "cache should have seen real 005 tokens");
    // The full Libera flag set, not the RFC imnpst.
    assert_eq!(net.isupport.flag_modes, "CFLMPQRSTcgimnprstuz");
    assert_eq!(net.isupport.list_modes, "eIbq");
    assert_eq!(net.isupport.prefix_modes, "qaohv");
    assert_eq!(net.isupport.topic_len, Some(390));

    // Now flood the :server: buffer past the ring cap; the cache must persist.
    for i in 100..100 + (RING_CAPACITY as i64 + 50) {
        apply(
            &mut store,
            json!({"kind":"irc","type":"motd","id":i,"networkId":1,"target":":server:1",
                   "text":"noise line"}),
        );
    }
    let net = store.networks.get(&1).unwrap();
    assert_eq!(net.isupport.flag_modes, "CFLMPQRSTcgimnprstuz", "cache outlives eviction");
}

// ── §4.4 resume cursor ────────────────────────────────────────────────────

#[test]
fn system_buffer_ids_never_advance_the_cursor() {
    // §4.4: the system buffer has a SEPARATE id sequence. Feeding one of its
    // ids into the cursor corrupts resume for every other buffer — the exact
    // bug called out for both first-party clients.
    let mut store = Store::new();
    apply(&mut store, msg(100, "#chat", "a", "hi"));
    assert_eq!(store.cursor(), 100);

    apply(
        &mut store,
        json!({"kind":"irc","type":"system","id":999_999,"networkId":null,
               "target":":system:","text":"reconnected","level":"info"}),
    );
    assert_eq!(store.cursor(), 100, "system id must not advance the cursor");
}

#[test]
fn cursor_is_seeded_from_the_snapshot() {
    // §4.3: shells carry no rows, so without seeding from `cursor` a full
    // burst would leave the cursor at 0 and refetch everything on reconnect.
    let mut store = Store::new();
    apply(&mut store, json!({"kind":"snapshot","cursor":4242,"networks":[],"globalIgnores":[]}));
    assert_eq!(store.cursor(), 4242);
}

#[test]
fn ephemeral_events_never_advance_the_cursor() {
    let mut store = Store::new();
    apply(&mut store, msg(10, "#chat", "a", "hi"));
    apply(&mut store, json!({"kind":"irc","type":"typing","networkId":1,
                             "target":"#chat","nick":"b","state":"active"}));
    assert_eq!(store.cursor(), 10);
}

// ── §8 merge rules ────────────────────────────────────────────────────────

#[test]
fn shell_never_un_hydrates_a_populated_buffer() {
    // §8 rule 3 — the reason shells are their own mode rather than
    // `replace`-with-empty.
    let mut store = Store::new();
    apply(&mut store, backlog("#chat", "replace", vec![row(1, "one"), row(2, "two")]));
    assert_eq!(texts(&store, "#chat").len(), 2);

    apply(
        &mut store,
        json!({"kind":"backlog","networkId":1,"target":"#chat","mode":"shell",
               "events":[],"hasMoreOlder":true}),
    );
    assert_eq!(texts(&store, "#chat").len(), 2, "shell must not wipe the buffer");
}

#[test]
fn overlapping_replace_preserves_paged_in_scrollback() {
    // §8 rule 5. `:system:` and offline `:server:` buffers get a full replace
    // on EVERY snapshot, including the in-band resync on visibility return —
    // dropping older rows there loses the user's scrollback each time they tab
    // back.
    let mut store = Store::new();
    apply(&mut store, backlog("#chat", "replace", vec![row(10, "ten"), row(11, "eleven")]));
    // User pages older history in.
    apply(
        &mut store,
        json!({"kind":"history","networkId":1,"target":"#chat","mode":"before",
               "events":[row(8,"eight"), row(9,"nine")],"hasMoreOlder":false}),
    );
    assert_eq!(texts(&store, "#chat"), ["eight", "nine", "ten", "eleven"]);

    // A resync replace whose oldest id (10) overlaps our newest (11).
    apply(&mut store, backlog("#chat", "replace", vec![row(10, "ten"), row(11, "eleven")]));
    assert_eq!(
        texts(&store, "#chat"),
        ["eight", "nine", "ten", "eleven"],
        "overlapping replace must keep older rows"
    );
}

#[test]
fn disjoint_replace_wipes_wholesale_rather_than_leaving_a_hole() {
    // §8 rule 5, the other branch: if the slice's oldest id is beyond our
    // newest, rows went missing in between.
    let mut store = Store::new();
    apply(&mut store, backlog("#chat", "replace", vec![row(1, "one"), row(2, "two")]));
    apply(&mut store, backlog("#chat", "replace", vec![row(500, "five hundred")]));
    assert_eq!(texts(&store, "#chat"), ["five hundred"]);
}

#[test]
fn replace_keeps_live_events_newer_than_the_slice_tail() {
    // §8 rule 6: a message can land mid-hydrate.
    let mut store = Store::new();
    apply(&mut store, backlog("#chat", "replace", vec![row(1, "one")]));
    apply(&mut store, msg(99, "#chat", "live", "landed mid-hydrate"));
    // A disjoint replace that predates the live row.
    apply(&mut store, backlog("#chat", "replace", vec![row(50, "fifty")]));
    assert!(
        texts(&store, "#chat").contains(&"landed mid-hydrate".to_string()),
        "live row newer than the slice tail must survive a replace"
    );
}

#[test]
fn has_more_older_is_recorded_on_append_and_shell_too() {
    // §8 rule 4: dropping it on a gap-fill strands the pager.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"backlog","networkId":1,"target":"#chat","mode":"shell",
               "events":[],"hasMoreOlder":true}),
    );
    assert!(store.buffer(&key("#chat")).unwrap().has_more_older);

    apply(
        &mut store,
        json!({"kind":"backlog","networkId":1,"target":"#chat","mode":"append",
               "events":[row(1,"one")],"hasMoreOlder":true}),
    );
    assert!(store.buffer(&key("#chat")).unwrap().has_more_older);
}

#[test]
fn events_are_ordered_by_id_not_arrival_or_time() {
    // §5.3: `time` is not monotonic with respect to `id` because a bouncer
    // upstream can replay old messages live.
    let mut store = Store::new();
    apply(&mut store, backlog("#chat", "replace", vec![row(5, "five")]));
    apply(&mut store, msg(7, "#chat", "a", "seven"));
    apply(
        &mut store,
        json!({"kind":"history","networkId":1,"target":"#chat","mode":"before",
               "events":[row(6,"six")],"hasMoreOlder":false}),
    );
    assert_eq!(texts(&store, "#chat"), ["five", "six", "seven"]);
}

#[test]
fn a_jump_detaches_the_buffer_and_latest_reattaches_it() {
    // §8: after `around`, live events must not be spliced into the visible
    // slice (the Discord/Slack jump convention).
    let mut store = Store::new();
    apply(&mut store, backlog("#chat", "replace", vec![row(100, "hundred")]));
    apply(
        &mut store,
        json!({"kind":"history","networkId":1,"target":"#chat","mode":"around",
               "anchorId":5,"events":[row(5,"anchor")],"hasMoreOlder":true}),
    );
    assert!(store.buffer(&key("#chat")).unwrap().detached);

    apply(&mut store, msg(101, "#chat", "a", "live while detached"));
    assert!(
        !texts(&store, "#chat").contains(&"live while detached".to_string()),
        "live rows must not splice into a detached slice"
    );

    apply(
        &mut store,
        json!({"kind":"history","networkId":1,"target":"#chat","mode":"latest",
               "events":[row(100,"hundred")],"hasMoreOlder":true}),
    );
    assert!(!store.buffer(&key("#chat")).unwrap().detached);
}

// ── §9.1 buffer materialization ───────────────────────────────────────────

#[test]
fn read_state_for_an_unknown_buffer_does_not_resurrect_it() {
    // Bug #319: `mark-all-read` fans out read-state for CLOSED buffers too,
    // and materializing from it put them back in the sidebar.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"read-state","networkId":1,"target":"#gone","unread":0,"highlights":0}),
    );
    assert!(store.buffer(&key("#gone")).is_none());
}

#[test]
fn typing_does_not_materialize_a_dm() {
    // Bug #292: typing-tag DM creation.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"irc","type":"typing","networkId":1,"target":"stranger","state":"active"}),
    );
    assert!(store.buffer(&key("stranger")).is_none());
}

#[test]
fn member_update_for_an_unknown_buffer_is_dropped() {
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"irc","type":"member-update","networkId":1,"target":"#nope",
               "member":{"nick":"a","modes":[]}}),
    );
    assert!(store.buffer(&key("#nope")).is_none());
}

#[test]
fn channel_joined_materializes_but_a_join_request_does_not() {
    // §9.1: `{type:'join'}` is intent; the buffer appears on `channel-joined`.
    let mut store = Store::new();
    store.note_pending_join(key("#new"));
    assert!(store.buffer(&key("#new")).is_none());

    apply(&mut store, json!({"kind":"irc","type":"channel-joined","networkId":1,"target":"#new"}));
    assert!(store.buffer(&key("#new")).unwrap().joined);
}

#[test]
fn join_error_creates_nothing() {
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"irc","type":"join-error","networkId":1,"target":"#banned",
               "text":"cannot join","reason":"+i"}),
    );
    assert!(store.buffer(&key("#banned")).is_none());
}

#[test]
fn an_incoming_dm_materializes_the_buffer() {
    let mut store = Store::new();
    apply(&mut store, msg(1, "amiantos", "amiantos", "hey"));
    assert!(store.buffer(&key("amiantos")).is_some());
}

#[test]
fn a_notice_from_an_unknown_nick_materializes_a_buffer() {
    // §9.1: that is how NickServ/ChanServ get their own buffer rather than
    // dumping into `:server:`. The server already made this decision; the
    // client must not special-case it.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"irc","type":"notice","id":1,"networkId":1,"target":"NickServ",
               "nick":"NickServ","text":"You are now identified."}),
    );
    assert!(store.buffer(&key("nickserv")).is_some());
}

#[test]
fn buffer_closed_drops_the_buffer_entirely() {
    // §9.1: closed = absent — messages, drafts and membership all go.
    let mut store = Store::new();
    apply(&mut store, msg(1, "#chat", "a", "hi"));
    let events = apply(
        &mut store,
        json!({"kind":"buffer-closed","networkId":1,"target":"#chat"}),
    );
    assert!(store.buffer(&key("#chat")).is_none());
    assert!(events.contains(&StoreEvent::BufferListChanged));
}

#[test]
fn channel_parted_keeps_history_and_clears_members() {
    // §9.1: resolve, never materialize.
    let mut store = Store::new();
    apply(&mut store, json!({"kind":"irc","type":"channel-joined","networkId":1,"target":"#chat"}));
    apply(&mut store, msg(1, "#chat", "a", "hi"));
    apply(
        &mut store,
        json!({"kind":"irc","type":"names","networkId":1,"target":"#chat",
               "members":[{"nick":"a","modes":["o"]}]}),
    );
    apply(&mut store, json!({"kind":"irc","type":"channel-parted","networkId":1,"target":"#chat"}));

    let buf = store.buffer(&key("#chat")).unwrap();
    assert!(!buf.joined);
    assert!(buf.members.is_empty());
    assert_eq!(texts(&store, "#chat"), ["hi"], "history survives a part");
}

#[test]
fn snapshot_channels_never_create_or_remove_buffers() {
    // §4.3: `channels` is empty for offline networks and is read before
    // auto-rejoin JOINs land, so judging buffer existence from it decides a
    // user left channels they are still in.
    let mut store = Store::new();
    apply(&mut store, msg(1, "#kept", "a", "hi"));
    apply(
        &mut store,
        json!({"kind":"snapshot","networks":[
            {"networkId":1,"state":"connected","nick":"me","channels":[
                {"name":"#other","topic":"t","members":[]}]}],"globalIgnores":[]}),
    );
    assert!(store.buffer(&key("#kept")).is_some(), "must not remove buffers");
    assert!(store.buffer(&key("#other")).is_none(), "must not create buffers");
}

// ── §9.6 ordering, dedupe and side effects ────────────────────────────────

#[test]
fn a_replayed_event_does_not_duplicate_a_row() {
    let mut store = Store::new();
    apply(&mut store, msg(5, "#chat", "a", "once"));
    apply(&mut store, msg(5, "#chat", "a", "once"));
    assert_eq!(texts(&store, "#chat"), ["once"]);
}

#[test]
fn a_replayed_topic_does_not_revert_a_newer_one() {
    // §9.6: the topic-revert bug came from mutating state below a missing
    // dedupe check.
    let mut store = Store::new();
    apply(
        &mut store,
        json!({"kind":"irc","type":"topic","id":10,"networkId":1,"target":"#chat",
               "text":"old topic","nick":"a"}),
    );
    apply(
        &mut store,
        json!({"kind":"irc","type":"topic","id":20,"networkId":1,"target":"#chat",
               "text":"new topic","nick":"b"}),
    );
    // Resume overlap replays the older row.
    apply(
        &mut store,
        json!({"kind":"irc","type":"topic","id":10,"networkId":1,"target":"#chat",
               "text":"old topic","nick":"a"}),
    );
    assert_eq!(store.buffer(&key("#chat")).unwrap().topic.as_deref(), Some("new topic"));
}

#[test]
fn a_replayed_join_does_not_double_add_a_member() {
    let mut store = Store::new();
    apply(&mut store, json!({"kind":"irc","type":"channel-joined","networkId":1,"target":"#chat"}));
    let join = json!({"kind":"irc","type":"join","id":3,"networkId":1,
                      "target":"#chat","nick":"newcomer"});
    apply(&mut store, join.clone());
    apply(&mut store, join);
    let buf = store.buffer(&key("#chat")).unwrap();
    assert_eq!(buf.members.len(), 1);
    assert_eq!(buf.events.iter().filter(|e| e.event_type == EventType::Join).count(), 1);
}

#[test]
fn membership_side_effects_ride_the_rendered_events() {
    let mut store = Store::new();
    apply(&mut store, json!({"kind":"irc","type":"channel-joined","networkId":1,"target":"#chat"}));
    apply(
        &mut store,
        json!({"kind":"irc","type":"names","networkId":1,"target":"#chat",
               "members":[{"nick":"alice","modes":["o"]},{"nick":"bob","modes":[]}]}),
    );
    apply(
        &mut store,
        json!({"kind":"irc","type":"nick","id":1,"networkId":1,"target":"#chat",
               "nick":"bob","newNick":"robert"}),
    );
    apply(
        &mut store,
        json!({"kind":"irc","type":"kick","id":2,"networkId":1,"target":"#chat",
               "nick":"alice","kicked":"robert"}),
    );

    let members = &store.buffer(&key("#chat")).unwrap().members;
    assert!(members.contains_key("alice"));
    assert!(!members.contains_key("bob"), "nick change should rename, not keep the old key");
    assert!(!members.contains_key("robert"), "kick removes the kicked nick");
}

// ── §5.3 notify gating ────────────────────────────────────────────────────

#[test]
fn a_muted_highlight_is_styled_but_raises_no_alert() {
    // §5.3: a NONOTIFY rule forces notify:false while the message is still
    // delivered and still counts toward unread. `matched` drives styling;
    // `notify` alone gates the alert.
    let mut store = Store::new();
    let events = apply(
        &mut store,
        json!({"kind":"irc","type":"message","id":1,"networkId":1,"target":"#chat",
               "nick":"a","text":"ping","matched":true,"notify":false}),
    );
    assert!(!events.iter().any(|e| matches!(e, StoreEvent::Notify(..))));
    let buf = store.buffer(&key("#chat")).unwrap();
    assert!(buf.events[0].matched, "still styled as a highlight");
}

#[test]
fn a_notifying_message_raises_exactly_one_alert_even_if_replayed() {
    let mut store = Store::new();
    let m = json!({"kind":"irc","type":"message","id":1,"networkId":1,"target":"#chat",
                   "nick":"a","text":"ping","matched":true,"notify":true});
    let first = apply(&mut store, m.clone());
    let replay = apply(&mut store, m);
    assert_eq!(first.iter().filter(|e| matches!(e, StoreEvent::Notify(..))).count(), 1);
    assert_eq!(replay.iter().filter(|e| matches!(e, StoreEvent::Notify(..))).count(), 0);
}

// ── Typing indicators ─────────────────────────────────────────────────────

#[test]
fn typing_tracks_in_existing_buffers_and_still_never_materializes() {
    let mut store = Store::new();
    apply(&mut store, msg(1, "#chat", "a", "hi"));

    let events = apply(
        &mut store,
        json!({"kind":"irc","type":"typing","networkId":1,"target":"#chat",
               "nick":"Bob","state":"active"}),
    );
    assert!(events.iter().any(|e| matches!(e, StoreEvent::TypingChanged(_))));
    assert_eq!(store.buffer(&key("#chat")).unwrap().typing_nicks(), ["Bob"]);

    // `done` clears it.
    apply(
        &mut store,
        json!({"kind":"irc","type":"typing","networkId":1,"target":"#chat",
               "nick":"Bob","state":"done"}),
    );
    assert!(store.buffer(&key("#chat")).unwrap().typing_nicks().is_empty());

    // Bug #292 must stay fixed: typing still creates nothing.
    apply(
        &mut store,
        json!({"kind":"irc","type":"typing","networkId":1,"target":"stranger2",
               "nick":"stranger2","state":"active"}),
    );
    assert!(store.buffer(&key("stranger2")).is_none());
}

#[test]
fn a_message_clears_the_senders_typing_state() {
    // The composed message arriving IS the end of typing; waiting for the
    // `done` TAGMSG leaves a ghost "is typing…" after their line renders.
    let mut store = Store::new();
    apply(&mut store, msg(1, "#chat", "a", "hi"));
    apply(
        &mut store,
        json!({"kind":"irc","type":"typing","networkId":1,"target":"#chat",
               "nick":"Bob","state":"active"}),
    );
    apply(&mut store, msg(2, "#chat", "Bob", "here is what I was typing"));
    assert!(store.buffer(&key("#chat")).unwrap().typing_nicks().is_empty());
}

// ── §9.4 read state ───────────────────────────────────────────────────────

#[test]
fn read_state_is_taken_verbatim_and_badges_are_recomputed() {
    let mut store = Store::new();
    apply(&mut store, msg(1, "#a", "x", "hi"));
    apply(&mut store, msg(2, "#b", "x", "hi"));
    apply(&mut store, json!({"kind":"read-state","networkId":1,"target":"#a",
                             "unread":5,"highlights":2,"lastReadId":1}));
    apply(&mut store, json!({"kind":"read-state","networkId":1,"target":"#b",
                             "unread":1,"highlights":3,"lastReadId":2}));
    assert_eq!(store.total_highlights(), 5);

    // Reading #a clears its highlights; the badge must fall, not accumulate.
    apply(&mut store, json!({"kind":"read-state","networkId":1,"target":"#a",
                             "unread":0,"highlights":0,"lastReadId":9}));
    assert_eq!(store.total_highlights(), 3);
}

// ── §9.2 identity ─────────────────────────────────────────────────────────

#[test]
fn inconsistent_target_casing_resolves_to_one_buffer() {
    let mut store = Store::new();
    apply(&mut store, msg(1, "#Chat", "a", "first"));
    apply(&mut store, msg(2, "#chat", "b", "second"));
    apply(&mut store, msg(3, "#CHAT", "c", "third"));
    assert_eq!(store.buffers.len(), 1);
    assert_eq!(texts(&store, "#chat").len(), 3);
    // First-seen casing is display-canonical.
    assert_eq!(store.buffer(&key("#chat")).unwrap().display_name, "#Chat");
}

// ── §4.3 correlation ──────────────────────────────────────────────────────

#[test]
fn a_dropped_socket_clears_pending_opens() {
    // §4.3: otherwise a reply that never arrived claims someone else's open
    // later, and the user gets dragged to a buffer another device opened.
    let mut store = Store::new();
    store.note_pending_open(key("#mine"));
    store.on_socket_lost();
    assert!(!store.pending_opens.contains(&key("#mine")));
    assert!(!store.backlog_complete);
}

// ── Ring discipline (§8 rule 8 / in-memory cap) ───────────────────────────

#[test]
fn eviction_rearms_the_pager() {
    // §8: `hasMoreOlder` describes the slice the SERVER sent; once our ring
    // drops rows there is more older history than we hold regardless.
    let mut store = Store::new();
    let rows: Vec<_> = (1..=RING_CAPACITY as i64 + 10).map(|i| row(i, "x")).collect();
    apply(&mut store, backlog("#chat", "replace", rows));
    let buf = store.buffer(&key("#chat")).unwrap();
    assert_eq!(buf.events.len(), RING_CAPACITY);
    assert!(buf.has_more_older, "eviction must re-arm the pager itself");
}

#[test]
fn dedupe_survives_eviction_of_the_row_being_replayed() {
    // The freshness high-water mark is tracked separately from the ring so
    // that evicting a row cannot reopen the dedupe window for it.
    let mut store = Store::new();
    let rows: Vec<_> = (1..=RING_CAPACITY as i64 + 10).map(|i| row(i, "x")).collect();
    apply(&mut store, backlog("#chat", "replace", rows));
    let before = store.buffer(&key("#chat")).unwrap().events.len();
    // Row 1 was evicted; replaying it must not re-insert it.
    apply(&mut store, msg(1, "#chat", "someone", "x"));
    assert_eq!(store.buffer(&key("#chat")).unwrap().events.len(), before);
}

#[test]
fn call_presence_frame_tracks_counts_and_emits_on_change() {
    // Lurker #680: the call-presence frame drives a per-buffer participant count.
    let mut store = Store::new();

    let evs = apply(
        &mut store,
        json!({"kind": "call-presence", "networkId": 1, "target": "#dev", "count": 2}),
    );
    assert_eq!(store.call_count(&key("#dev")), 2);
    assert!(evs
        .iter()
        .any(|e| matches!(e, StoreEvent::CallPresenceChanged(k) if *k == key("#dev"))));

    // Re-sending the same count must not emit — no spurious repaints.
    let evs = apply(
        &mut store,
        json!({"kind": "call-presence", "networkId": 1, "target": "#dev", "count": 2}),
    );
    assert!(evs.is_empty(), "unchanged count re-emitted");

    // count 0 means the call ended: the entry clears and a change fires.
    let evs = apply(
        &mut store,
        json!({"kind": "call-presence", "networkId": 1, "target": "#dev", "count": 0}),
    );
    assert_eq!(store.call_count(&key("#dev")), 0);
    assert!(evs.iter().any(|e| matches!(e, StoreEvent::CallPresenceChanged(_))));
}

#[test]
fn hydrate_call_presence_replaces_stale_entries_with_the_snapshot() {
    // On reconnect the REST snapshot is authoritative: a call that ended while
    // we were offline must clear, not linger, since the frame only sends deltas.
    let mut store = Store::new();
    apply(&mut store, json!({"kind":"call-presence","networkId":1,"target":"#dev","count":1}));
    apply(&mut store, json!({"kind":"call-presence","networkId":1,"target":"#ops","count":4}));

    let changed = store.hydrate_call_presence(1, &[("#dev".to_string(), 3)]);
    assert_eq!(store.call_count(&key("#dev")), 3, "updated to snapshot count");
    assert_eq!(store.call_count(&key("#ops")), 0, "stale call cleared");
    assert!(changed.contains(&key("#dev")));
    assert!(changed.contains(&key("#ops")));
}

// ── Friends / contacts ────────────────────────────────────────────────────

fn contacts_snapshot(contacts: serde_json::Value) -> serde_json::Value {
    json!({ "kind": "contacts-snapshot", "contacts": contacts })
}

/// A store with network 1 connected, so presence isn't forced offline.
fn connected_store() -> Store {
    let mut store = Store::new();
    apply(
        &mut store,
        json!({ "kind": "snapshot", "networks": [
            { "networkId": 1, "state": "connected", "nick": "me" },
            { "networkId": 2, "state": "connected", "nick": "me" }
        ]}),
    );
    store
}

fn presence_event(network: i64, nick: &str, state: &str) -> serde_json::Value {
    json!({
        "kind": "irc", "type": "peer-presence", "networkId": network,
        "target": format!(":server:{network}"), "nick": nick, "state": state
    })
}

#[test]
fn contacts_snapshot_populates_the_friends_list() {
    let mut store = connected_store();
    let out = apply(
        &mut store,
        contacts_snapshot(json!([
            { "id": 7, "displayName": "amiantos", "notifyOnline": true,
              "targets": [{ "networkId": 1, "nick": "amiantos", "isPrimary": true }] }
        ])),
    );
    assert!(out.iter().any(|e| matches!(e, StoreEvent::ContactsChanged)));
    assert_eq!(store.contacts().len(), 1);
    assert_eq!(store.contacts()[0].display_name, "amiantos");
}

#[test]
fn friends_sort_by_name_not_by_who_is_online() {
    // A list that reorders itself as people connect is one you can't learn.
    let mut store = connected_store();
    apply(
        &mut store,
        contacts_snapshot(json!([
            { "id": 1, "displayName": "Zed",
              "targets": [{ "networkId": 1, "nick": "zed", "isPrimary": true }] },
            { "id": 2, "displayName": "alice",
              "targets": [{ "networkId": 1, "nick": "alice", "isPrimary": true }] }
        ])),
    );
    apply(&mut store, presence_event(1, "zed", "online"));
    let names: Vec<&str> = store.contacts().iter().map(|c| c.display_name.as_str()).collect();
    assert_eq!(names, vec!["alice", "Zed"], "case-insensitive alphabetical, online-ness ignored");
}

#[test]
fn a_friend_row_shows_the_primary_targets_presence_not_the_best_one() {
    // The row opens the PRIMARY DM. An alt online elsewhere must not light the
    // dot green for a conversation that is actually going nowhere.
    let mut store = connected_store();
    apply(
        &mut store,
        contacts_snapshot(json!([
            { "id": 1, "displayName": "amiantos", "targets": [
                { "networkId": 1, "nick": "amiantos", "isPrimary": true },
                { "networkId": 2, "nick": "amiantos_", "isPrimary": false }
            ]}
        ])),
    );
    apply(&mut store, presence_event(1, "amiantos", "offline"));
    apply(&mut store, presence_event(2, "amiantos_", "online"));
    let c = store.contact(1).unwrap();
    assert_eq!(store.contact_presence(c), Presence::Offline);
}

#[test]
fn back_from_away_reads_as_online() {
    // The server speaks four state words. Matching only three leaves a friend
    // stuck showing away after they return.
    let mut store = connected_store();
    apply(&mut store, presence_event(1, "amiantos", "away"));
    assert_eq!(store.presence(1, "amiantos"), Presence::Away);
    apply(&mut store, presence_event(1, "amiantos", "back"));
    assert_eq!(store.presence(1, "amiantos"), Presence::Online);
}

#[test]
fn presence_is_case_insensitive() {
    let mut store = connected_store();
    apply(&mut store, presence_event(1, "AmIaNtOs", "online"));
    assert_eq!(store.presence(1, "amiantos"), Presence::Online);
}

#[test]
fn everyone_is_offline_on_a_network_that_is_down() {
    let mut store = connected_store();
    apply(&mut store, presence_event(1, "amiantos", "online"));
    apply(
        &mut store,
        json!({ "kind": "snapshot", "networks": [{ "networkId": 1, "state": "disconnected" }]}),
    );
    assert_eq!(store.presence(1, "amiantos"), Presence::Offline);
}

#[test]
fn an_unknown_peer_on_a_live_network_is_not_offline() {
    // No row may only mean the server has no MONITOR. "Unknown" must not be
    // painted as "offline".
    let store = connected_store();
    assert_eq!(store.presence(1, "nobody"), Presence::Unknown);
}

#[test]
fn a_friends_primary_dm_is_hidden_from_its_own_network() {
    let mut store = connected_store();
    apply(
        &mut store,
        contacts_snapshot(json!([
            { "id": 1, "displayName": "amiantos", "targets": [
                { "networkId": 1, "nick": "AmIaNtOs", "isPrimary": true },
                { "networkId": 1, "nick": "alt", "isPrimary": false }
            ]}
        ])),
    );
    assert!(store.is_friend_primary_dm(&key("amiantos")), "folded match, either casing");
    assert!(!store.is_friend_primary_dm(&key("alt")), "non-primary targets stay in their section");
    assert!(!store.is_friend_primary_dm(&key("#chat")));
}

#[test]
fn clicking_a_friend_reuses_the_open_dm_rather_than_forking_on_case() {
    let mut store = connected_store();
    apply(&mut store, backlog("AmIaNtOs", "replace", vec![row(1, "hi")]));
    apply(
        &mut store,
        contacts_snapshot(json!([
            { "id": 1, "displayName": "amiantos",
              "targets": [{ "networkId": 1, "nick": "amiantos", "isPrimary": true }] }
        ])),
    );
    let c = store.contact(1).unwrap();
    assert_eq!(store.contact_dm_key(c), Some(key("amiantos")));
}

#[test]
fn contact_updated_and_deleted_keep_the_list_in_sync() {
    let mut store = connected_store();
    apply(
        &mut store,
        contacts_snapshot(json!([
            { "id": 1, "displayName": "old",
              "targets": [{ "networkId": 1, "nick": "amiantos", "isPrimary": true }] }
        ])),
    );
    apply(
        &mut store,
        json!({ "kind": "contact-updated", "contact": {
            "id": 1, "displayName": "new",
            "targets": [{ "networkId": 1, "nick": "amiantos", "isPrimary": true }] }}),
    );
    assert_eq!(store.contacts()[0].display_name, "new");
    assert!(store.contact_for(1, "AMIANTOS").is_some());

    let out = apply(&mut store, json!({ "kind": "contact-deleted", "contactId": 1 }));
    assert!(out.iter().any(|e| matches!(e, StoreEvent::ContactsChanged)));
    assert!(store.contacts().is_empty());
}

#[test]
fn peer_presence_never_materializes_a_buffer() {
    // It is addressed to `:server:` only so the server's closed-buffer guard
    // can't drop it; it is network state, not a row.
    let mut store = connected_store();
    let before = store.buffers.len();
    let out = apply(&mut store, presence_event(1, "amiantos", "online"));
    assert_eq!(store.buffers.len(), before);
    assert!(out.iter().any(|e| matches!(e, StoreEvent::PresenceChanged(1))));
}
