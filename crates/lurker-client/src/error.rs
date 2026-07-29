use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Dead session (§3.4). Any `401` from `/api/*` or a refused WS upgrade
    /// means this and nothing else — the server never uses 401 for downstream
    /// failures — so the correct response is always: clear the stored token and
    /// return to login.
    #[error("session expired or credentials rejected")]
    Unauthorized,

    /// `429` with an optional `Retry-After`, which §3.4 says to honor.
    #[error("rate limited{}", match .retry_after {
        Some(s) => format!(", retry after {s}s"),
        None => String::new(),
    })]
    RateLimited { retry_after: Option<u64> },

    /// HTTP `426` on the WS upgrade: this client's protocol version is below
    /// the server's `minProtocolVersion`. Not retryable — the client needs
    /// updating.
    #[error("server requires a newer protocol version than {} — update the client", lurker_proto::PROTOCOL_VERSION)]
    UpgradeRequired,

    /// HTTP `403` on the WS upgrade — an Origin check. Native clients send no
    /// Origin and should never see this.
    #[error("websocket upgrade forbidden")]
    Forbidden,

    #[error("{message}")]
    Api { status: u16, message: String },

    #[error("server URL is not valid")]
    BadBaseUrl,

    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("could not parse server response: {0}")]
    Decode(#[from] serde_json::Error),
}

impl Error {
    /// Whether reconnecting could plausibly succeed.
    ///
    /// The three that cannot are exactly the ones needing user action: a dead
    /// session, a too-old client, and a rejected Origin. Retrying those spins
    /// forever while showing the user nothing actionable.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Unauthorized | Self::UpgradeRequired | Self::Forbidden)
    }
}

/// A connection's user-visible state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    /// Signalled on the **first received frame**, never on socket open — a
    /// refused upgrade looks like an open-then-close to some WS APIs (§4.4).
    Connected,
    /// Waiting to retry, with the delay.
    Backoff(std::time::Duration),
    /// Terminal: needs user action.
    Failed(String),
}

impl fmt::Display for ConnState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Connecting => write!(f, "connecting"),
            Self::Connected => write!(f, "connected"),
            Self::Backoff(d) => write!(f, "reconnecting in {}s", d.as_secs()),
            Self::Failed(m) => write!(f, "{m}"),
        }
    }
}
