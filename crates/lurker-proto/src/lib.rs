//! Wire types and pure rules for the Lurker client protocol, version 1.
//!
//! Verified against `amiantos/lurker` v1.1.5 and `docs/CLIENT_PROTOCOL.md`.
//! This crate performs no I/O and knows nothing about transports or UI, so the
//! rules the protocol doc calls out as "every one of these was a real bug once"
//! (§9) can be unit-tested directly. Doc references in comments (§4.3, §8 rule
//! 5, …) point into `CLIENT_PROTOCOL.md`.
//!
//! Two invariants shape the whole crate:
//!
//! * **Unknown is never fatal.** Unrecognized frame kinds, event types and
//!   fields are ignored rather than rejected, because the protocol evolves
//!   additively and `protocolVersion` does not move for new ones (§2).
//! * **The server owns truth.** Buffer existence, read state and the notify
//!   decision are mirrored from frames, never derived locally (§9.1, §9.4,
//!   §5.3).

pub(crate) mod de;

pub mod casefold;
pub mod consolidate;
pub mod event;
pub mod frame;
pub mod isupport;
pub mod mirc;
pub mod timeparse;
pub mod verb;

pub use consolidate::{consolidate, ConsolidationKind, Summary as ConsolidationSummary};
pub use casefold::{fold, is_channel, is_sentinel, BufferKey, SERVER_TARGET, SYSTEM_TARGET};
pub use event::{EventType, Member, MessageEvent};
pub use frame::{ChanlistRow, Contact, ContactTarget, FavoriteEntry, 
    AwayState, BacklogFrame, BacklogMode, HistoryFrame, HistoryMode, NetworkSnapshot, NetworkState,
    PeerPresence, ReadState, ServerFrame, SnapshotChannel, SnapshotFrame,
};
pub use mirc::{Segment, Style};
pub use verb::{ClientVerb, EventMode, PageUnit};

/// The protocol version this client implements. Announced on the WS upgrade as
/// `/ws?v=1`.
///
/// Always send it: omitting `?v` means "treat me as current", so a future
/// `minProtocolVersion` bump would feed us frames we misparse instead of
/// rejecting us cleanly with HTTP 426 (§2).
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum inbound WS frame the server accepts (§4.2). Uploads go over REST.
pub const MAX_WS_FRAME_BYTES: usize = 256 * 1024;

/// Server-side clamp on a `history` request's `limit` (§8).
pub const MAX_HISTORY_LIMIT: u32 = 500;
