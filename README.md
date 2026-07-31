# Scully

A native GTK4 desktop client for [Lurker](https://github.com/amiantos/lurker) —
Rust, no web view anywhere. Built against Lurker **v1.1.5**, client protocol
**version 1**.

Named for the one at the desk who wants evidence. Sibling app to **Spooky**
(Android); both speak the Lurker client protocol.

## Why GTK4 rather than a webview

The scrollback is a `GtkTextView`, which is the reason this approach is worth the
Linux-first tradeoff: text selection, clipboard, IME, emoji, and complex-script
shaping all come from Pango for free. Those are most of an IRC client, and they
are exactly what a hand-rolled GPU text widget gets wrong. CSS supplies the retro
terminal look, so libadwaita is deliberately *not* used — the app respects your
desktop's own decorations instead of importing GNOME's design language.

## Layout

    crates/
      lurker-proto/    wire types + pure rules — no I/O, no UI
      lurker-client/   REST, WebSocket, and the reducer
      scully/          the application

The split is load-bearing. `docs/CLIENT_PROTOCOL.md` §9 is a list of rules
prefaced with "every one of these was a real bug once" — resume-cursor
corruption, buffers resurrected by ambient frames, replayed events reverting a
topic. Those live in `store.rs` as synchronous code over constructed frames, so
each rule is a unit test rather than something you discover against a live
server. Comments cite the section they implement (§4.4, §8 rule 5, …).

Multi-window falls out of the same split: one socket and one store, with N
windows registered as observers. A window owns only which buffer it is looking at
and what it has drawn. Two windows cannot disagree, and `Ctrl+N` costs nothing.

## Status

Working: login and token mint, keyring storage, session resume, the snapshot
burst, buffer list with unread/highlight badges, scrollback with paging, live
messages, nicklist, read state, send with slash commands, desktop notifications,
reconnect with resume, multi-window (Ctrl+N), channel popouts (⬈ button or middle-click a buffer row — one channel per lightweight window, as many as you like), a right-click nicklist menu (whois, query, slap, the full ±qaohv mode ladder, kick/ban/kick+ban with host masks, CTCP, ignore), join/part consolidation, mIRC
colour and formatting codes, a top toolbar, and a full settings window
(Ctrl+,) generated from the server's self-describing registry — including the
event-noise tier and consolidation toggles, which feed the page-sizing unit the
client requests.

Also: inline images/video/audio with per-device toggles (This device settings), Ctrl+V or drag-drop a file to upload it through Lurker's pipeline and paste the link, a double-click channel-control dialog (topic with colour, modes filtered to what the server's ISUPPORT advertises), and the system log pinned to the top of the buffer list.

Also: clickable links (open externally), a right-click nicklist menu gated by your channel authority, whois results optionally shown in the active buffer, and YouTube link previews (title + description, off by default).

Also: message search (Ctrl+F here, Ctrl+Shift+F everywhere) with jump-to-message,
a buffer quick-switcher (Ctrl+K), and ~60 slash commands.

Also, behind a build flag: **native voice calls** (`--features voice`). A
self-hosted LiveKit SFU handles the media; Lurker mints room-scoped tokens
(`/api/voice/token`); Scully joins over libwebrtc — a native media engine,
**not** a webview — with the microphone and speakers driven by `cpal`. Active
calls surface as a "📞 N" badge on the buffer list, fed by Lurker's server-side
`call-presence` frame (Lurker #680) and a per-network snapshot on connect, so
you see a call whether or not you're in it; the 📞 button or `/call` joins.
Presence being *always on* while joining is feature-gated means a stock build
still shows call badges without linking libwebrtc. Channel operators get call
moderation (mute / remove, from the nicklist menu) and a per-channel join
policy in the channel-control dialog; both mirror the server's own op rules, so
the controls are hidden rather than offered-then-refused.

## Parity with Lurker, deliberately not a clone

Scully covers Lurker's daily-driver surface — buffers, history, uploads, media,
settings, channel and nick management, search — but reaches it in desktop idioms
rather than porting the web UI:

| Lurker (web) | Scully |
|---|---|
| Modal dialogs over a scrim | Real top-level windows, `transient_for` their parent |
| One page, one conversation | Channel popouts: N windows over one socket and store |
| Search modal | Keyboard-first finder: type, arrow, Enter to jump |
| Quick switcher | Same idea, native window, fuzzy subsequence ranking |
| Settings pane | Same registry, rendered as GTK controls; plus device-local settings the server does not sync |
| Ban list in a modal | Inline in the channel-control dialog |

Not yet: DCC transfers, the bespoke highlights/ignores editors, channel browser
(`/list`), bookmarks. Voice video (voice-only for now).

## Protocol decisions worth knowing

Some of these are easy to get backwards, and the failures are silent:

- **Hydration uses `history`/`latest`, never `open-buffer`.** `open-buffer` is a
  *write*: it reopens closed buffers, mints DM rows, and JOINs unjoined channels
  — on every device you own. It is also gated for paused accounts, so a client
  that hydrates through it shows a paused user nothing at all. (§4.3)
- **The system buffer has its own id sequence.** Feeding its ids into the resume
  cursor corrupts resume for every other buffer. (§4.4)
- **`notify` alone gates alerts; `matched` only styles.** A muted-channel
  highlight arrives `matched: true, notify: false` — it should look like a
  highlight and make no sound. The server owns that verdict because push fires
  when no client is attached. (§5.3)
- **Case folding is ASCII-only.** RFC 1459 casemapping is deliberately absent
  from Lurker; "fixing" it unilaterally would split identity with the server.
  There is a test pinning its absence. (§9.2)
- **No optimistic rendering.** Sent messages render when the server echoes them
  back with a real id. (§9.3)

## Small screens (Linux phones)

Below 640 logical pixels Scully shows one pane at a time: the conversation list,
then the conversation, with a back arrow between them and the nicklist dropped —
at that width a column of names costs more than it tells you. The trigger is
window width, never a device check, so a desktop window dragged narrow behaves
the same way, which is also how you test it without a phone.

This makes it usable on FuriOS (FuriPhone), Phosh, and anything else running
GTK4 on a handset. Releases include an `aarch64` Linux build for exactly that.

## Build and run

    cargo run -p scully

Requires GTK 4.12+. A Secret Service provider (gnome-keyring, KWallet) is used
for the session token when present; otherwise it falls back to a `0600` file and
says so in the log.

### Voice calls (optional)

Voice is off by default on both ends. It is a native media path, not a webview,
so the client half is a deliberate build flag:

    cargo run -p scully --features voice

That pulls a prebuilt **libwebrtc** through `webrtc-sys` (a large download and a
`clang` toolchain), plus `cpal` for the microphone and speakers. The default
build stays lean and never links it. (Note: the livekit 0.7.53 crate tree needs
three transitive version pins to resolve — see `crates/scully/Cargo.toml`, which
documents why.)

Server-side, run the bundled LiveKit SFU and point Lurker at it. In
`upstream/docker-compose.yml`, uncomment the four `LURKER_VOICE_ENABLED` /
`LIVEKIT_*` vars (choose your own secret, matching the `livekit` service), then:

    docker compose --profile voice up -d

Lurker only mints room-scoped tokens; the SFU carries the media. Behind real NAT
you also need a public IP and a TURN server (see the compose comments). Once the
server advertises `voiceEnabled`, a 📞 button appears in each channel — or type
`/call`.

To check a server without logging in:

    cargo run -p lurker-client --example probe -- https://your-lurker

## Licence

MPL-2.0, matching upstream Lurker.
