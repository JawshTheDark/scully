//! Transport and state for a Lurker client.
//!
//! Three layers, deliberately separable:
//!
//! * [`rest`] — auth and the request/response management surface (§3, §10).
//! * [`socket`] — one WebSocket, its reconnect policy and outbound rate limit (§4).
//! * [`store`] — the synchronous reducer holding every §8/§9 rule.
//!
//! The store is transport-free so the protocol rules can be tested against
//! constructed frames, and the socket is store-free so it can be driven by any
//! consumer. A UI wires them together.

pub mod error;
pub mod rest;
pub mod socket;
pub mod store;

pub use error::{ConnState, Error, Result};
pub use rest::{Edition, NetworkRow, Rest, ServerConfig, SettingOption, SettingsBootstrap, TokenResponse, UploadResponse, UploadersInfo, VoiceCall, VoicePolicy, VoiceToken};
pub use socket::{ClientEvent, Socket, SocketConfig};
pub use store::{Buffer, Network, Presence, Store, StoreEvent};
