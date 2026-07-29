# Native voice for Lurker via LiveKit — notes for amiantos

Context: I've been building **Scully**, a native GTK4 desktop client for Lurker
(no Electron, no webview — the scrollback is a real `GtkTextView`). I added voice
calls to it and, in doing so, had to teach the Lurker server to act as a token
authority. This is a writeup of the approach and the small API surface it needs,
in case any of it is useful upstream. It's a fork patch today, not a PR — I
wanted to sanity-check the design with you first.

**TL;DR for the channel:** native voice, no browser. LiveKit SFU carries media;
Lurker only mints room-scoped JWTs (`POST /api/voice/token`), gated on network
ownership + channel membership, off by default. Client joins over libwebrtc via
the `livekit` Rust crate + `cpal`. Presence rides a `LURKER-CALL` CTCP. Full
writeup + the one gnarly dependency gotcha below.

---

## Why LiveKit, not Jitsi

For a *native* client this wasn't close:

- **LiveKit** ships a first-class Rust SDK (`livekit`) over native **libwebrtc**.
  Rooms/participants/tracks in Rust, audio straight to PipeWire. No JS.
- **Jitsi**'s client is `lib-jitsi-meet` — JavaScript over XMPP/Colibri. There's
  no real native path; you'd hand-roll the signaling and still need a media
  engine.

Worth stating plainly because it trips people up: **WebRTC is not a browser.**
libwebrtc is a native C++ media engine (capture, Opus/VP8, ICE, SRTP, jitter
buffer). Linking it into a GTK app is as "native" as linking GTK. The only real
cost is build weight (a large prebuilt libwebrtc download + a clang toolchain),
so on the client I put the whole thing behind a `voice` cargo feature that's off
by default.

## The division of labour

The key decision: **Lurker never touches media.** It's purely the token
authority, which is the one thing it's uniquely positioned to be — it already
knows who the user is and which networks/channels they're in. That maps exactly
onto how Scully already mints session tokens: the client holds a short-lived,
room-scoped token and never sees the LiveKit API secret.

```
  Scully ──POST /api/voice/token──► Lurker ──(signs JWT with LIVEKIT_API_SECRET)
    │                                 │
    │ ◄────── { url, room, token } ───┘
    │
    └──────── join(url, token) ──────► LiveKit SFU  ◄── other participants
                (media flows here; Lurker is not in the path)
```

## Server API (what I added to Lurker)

Three small pieces, all in the existing style (Express routers, env-gated like
the DCC feature):

**1. `services/voice.ts`** — pure config + room naming + JWT minting
(`livekit-server-sdk`). Gated exactly like DCC: off unless
`LURKER_VOICE_ENABLED` is truthy **and** the three `LIVEKIT_*` vars are present.

**2. `POST /api/voice/token`** — `requireAuth` (cookie or native bearer), body
`{ networkId, target }`. Gates, in order:

| Check | Failure | Why |
|---|---|---|
| `voiceEnabled()` | `503` | operator opt-in + SFU configured |
| `getNetwork(id, user)` | `404` | you can only call on a network you own |
| channel: `conn.channels.has(target)` | `403` | must be *in* the channel (DMs skip this — opening a call *is* the invite) |

On success it derives the room name server-side (so a client can't request a
token for an arbitrary room), mints a JWT scoped to that one room with identity =
the caller's live IRC nick, and returns `{ token, room, url }`. Grant is
`roomJoin + canPublish + canSubscribe + canPublishData`, 2h TTL.

**3. `GET /api/config`** — added a `voiceEnabled` boolean so clients can hide all
call UI when the instance doesn't offer it.

Plus a `livekit` service in `docker-compose.yml` behind a `--profile voice` (so
it's defined but inert unless opted in), and the four env vars documented inline
on the `lurker` service.

### Room naming — the one non-obvious bit

Rooms are derived, never client-supplied:

- Channel `#dev` on network 7 → `net-7-c-#dev` (folded ASCII-only, matching
  Lurker's casemapping stance — I don't fold `[]{}` etc).
- DM between `Alice` and `bob` → `net-7-d-alice-bob`.

The DM case is the trap: if A calls B, A's `target` is `"B"` and B's is `"A"`.
Naming the room verbatim from `(self, target)` would put the two ends in **two
different rooms**. So DM rooms are keyed by the *canonical sorted nick pair*.
This is unit-tested from both directions. (The whole naming/gating layer is pure
+ tested — 12 service tests, 5 route tests.)

## Presence / signaling — over IRC itself

Rather than invent a parallel presence channel, "a call is happening" rides a
CTCP: on call start the client sends `LURKER-CALL START <room>` to the buffer;
other Scully clients recognize it and light up a join affordance. A plain IRC
client just sees a harmless `LURKER-CALL` CTCP. The wire format is a tiny, lenient
parser (case-insensitive verb, tolerant of extra fields) so it can evolve without
breaking older clients.

Open question for you here: **does Lurker relay unknown channel CTCPs to
clients as events?** I wired the send + the inbound parser + rendering, but I
couldn't confirm the round-trip without two live clients. If Lurker swallows
unknown CTCPs, the presence half needs a different carrier (a dedicated
frame/verb would be the clean answer, and is the kind of thing that'd want to be
upstream rather than bolted on).

## Client side (Scully), for completeness

- `livekit` (Rust) + `cpal` for the actual mic capture / speaker playback that
  the SDK leaves to the embedder. Bridged into GTK's main loop via the same
  async-channel pattern as the WebSocket.
- Voice-only for now (one mic track, N remote audio tracks, mute, hangup); video
  is a clean phase 2.
- Everything gated behind `--features voice`, so the default build never links
  libwebrtc.

## Heads-up: the livekit Rust crate tree is currently broken on crates.io

This cost me the most time and is worth knowing if you or anyone else touches the
Rust SDK right now:

- **`livekit` 0.8.0 / 0.8.1 don't compile at all.** Their pinned
  `livekit-common` 0.1.0 renamed/dropped symbols (`CLIENT_PROTOCOL_DATA_STREAM_V2`
  → `_RPC`, `ClientCapability`, `RemoteParticipantRegistry`) that the sibling
  crates + `livekit` itself still import. The published tree is internally
  inconsistent.
- **0.7.53 (latest 0.7)** compiles, but only after pinning two transitive deps
  that cargo otherwise resolves to broken-newer versions:
  - `livekit-data-stream = "=0.1.0"` — 0.1.1 imports the dropped `common` symbols.
  - `livekit-protocol = "=0.7.10"` — 0.7.12 added a required field
    (`ConnectWhatsAppCallRequest.wait_until_answered`) that `livekit-api` 0.5.6
    constructs the struct without.

With those three pins it builds clean and libwebrtc links fine. I've documented
each pin inline in `Cargo.toml` with the exact symbol/field it fixes.

## Honest status

Server side is done and tested (17 tests, tsc clean). Client compiles against the
real libwebrtc/livekit/cpal APIs and all the pure logic is unit-tested — but I
have **not** run live two-party audio yet (needs a running SFU + real hardware +
a second participant). So treat the media path as "structurally complete,
runtime-unverified."

Happy to turn any of the server side into a proper PR if the token-authority
shape looks right to you — especially want your read on the CTCP-vs-dedicated-verb
question for presence.

— jawsh (Scully)
