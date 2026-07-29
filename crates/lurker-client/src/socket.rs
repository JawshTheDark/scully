// The WebSocket connection manager (§4).
//
// Owns one socket, its reconnect policy, and the outbound rate limit. It knows
// nothing about the store: it emits [`ClientEvent`]s and accepts [`ClientVerb`]s,
// so the reducer stays synchronous and testable.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::StatusCode, Message,
};
use url::Url;

use lurker_proto::{ClientVerb, ServerFrame, PROTOCOL_VERSION};

use crate::error::{ConnState, Error, Result};

/// Reconnect backoff bounds. The iOS reference client uses exponential 1→30 s,
/// reset on the first received frame (§4.4).
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Outbound flood control, mirroring the server's per-socket token bucket:
/// capacity 120, refill 40/sec (§4.2). Exhausting the server's bucket earns an
/// error frame then a `1008` close, so the client throttles itself to stay
/// safely inside it rather than discovering the limit by being disconnected.
const BUCKET_CAPACITY: f64 = 120.0;
const BUCKET_REFILL_PER_SEC: f64 = 40.0;

/// Receive-side frame cap. The server's 256 KiB limit is on **inbound** frames
/// (ours); its own `backlog` frames can be far larger, so this is deliberately
/// generous rather than symmetric.
const MAX_INCOMING_FRAME: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum ClientEvent {
    State(ConnState),
    Frame(Box<ServerFrame>),
    /// A frame arrived that we could not parse. Non-fatal by §2, but worth
    /// surfacing loudly: it means a shape this client gets wrong, and silently
    /// dropping it looks exactly like the server never sent anything.
    Undecodable { raw: String, error: String },
}

pub struct SocketConfig {
    /// Base HTTP(S) URL of the instance; the `ws(s)://…/ws` URL is derived.
    pub base: Url,
    pub token: String,
}

/// Handle to a running connection task.
pub struct Socket {
    verbs: async_channel::Sender<ClientVerb>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Socket {
    /// Queue a verb. Returns false once the connection task has stopped.
    pub fn send(&self, verb: ClientVerb) -> bool {
        self.verbs.try_send(verb).is_ok()
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// A simple token bucket matching the server's flood-control shape.
struct Bucket {
    tokens: f64,
    last: std::time::Instant,
}

impl Bucket {
    fn new() -> Self {
        Self { tokens: BUCKET_CAPACITY, last: std::time::Instant::now() }
    }

    /// Time to wait before one more message may be sent.
    fn acquire(&mut self) -> Duration {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * BUCKET_REFILL_PER_SEC).min(BUCKET_CAPACITY);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Duration::ZERO
        } else {
            let deficit = 1.0 - self.tokens;
            self.tokens = 0.0;
            Duration::from_secs_f64(deficit / BUCKET_REFILL_PER_SEC)
        }
    }
}

/// Build the `/ws` URL, carrying the version and resume cursor.
///
/// `?v` is always sent: omitting it means "treat me as current", so a future
/// `minProtocolVersion` bump would feed us frames we misparse instead of
/// rejecting us cleanly with a 426 (§2).
fn ws_url(base: &Url, since: i64) -> Result<Url> {
    let mut url = base.join("ws").map_err(|_| Error::BadBaseUrl)?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        s => s.to_string().leak(),
    };
    url.set_scheme(scheme).map_err(|_| Error::BadBaseUrl)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("v", &PROTOCOL_VERSION.to_string());
        if since > 0 {
            q.append_pair("since", &since.to_string());
        }
    }
    Ok(url)
}

/// Map a refused upgrade to the specific error §4.1 defines for it.
fn upgrade_error(status: StatusCode) -> Error {
    match status {
        StatusCode::FORBIDDEN => Error::Forbidden,
        StatusCode::UPGRADE_REQUIRED => Error::UpgradeRequired,
        StatusCode::UNAUTHORIZED => Error::Unauthorized,
        other => Error::Api { status: other.as_u16(), message: format!("upgrade failed: {other}") },
    }
}

/// Spawn the connection task.
///
/// `cursor` is read fresh before every connect attempt, so a reconnect always
/// resumes from the newest id the store has actually applied rather than a
/// value captured at spawn time.
pub fn spawn<F>(
    config: SocketConfig,
    events: async_channel::Sender<ClientEvent>,
    mut cursor: F,
) -> Socket
where
    F: FnMut() -> i64 + Send + 'static,
{
    let (verb_tx, verb_rx) = async_channel::unbounded::<ClientVerb>();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;

        loop {
            if *shutdown_rx.borrow() {
                return;
            }
            let _ = events.send(ClientEvent::State(ConnState::Connecting)).await;

            match run_once(&config, &events, &verb_rx, &mut shutdown_rx, cursor()).await {
                // A clean or transport-level end: retry.
                Ok(saw_frame) => {
                    if saw_frame {
                        backoff = BACKOFF_MIN;
                    }
                }
                Err(e) => {
                    if !e.is_retryable() {
                        let _ = events
                            .send(ClientEvent::State(ConnState::Failed(e.to_string())))
                            .await;
                        return;
                    }
                    tracing::warn!(error = %e, "socket dropped");
                }
            }

            if *shutdown_rx.borrow() {
                return;
            }
            let _ = events.send(ClientEvent::State(ConnState::Backoff(backoff))).await;
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown_rx.changed() => return,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    });

    Socket { verbs: verb_tx, shutdown: shutdown_tx }
}

/// One connection attempt. Returns whether any frame was received, which is
/// what resets the backoff.
async fn run_once(
    config: &SocketConfig,
    events: &async_channel::Sender<ClientEvent>,
    verbs: &async_channel::Receiver<ClientVerb>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    since: i64,
) -> Result<bool> {
    let url = ws_url(&config.base, since)?;
    let mut request = url.as_str().into_client_request()?;
    // Native clients authenticate with Bearer on the upgrade itself; browsers
    // ride the cookie because they cannot set headers here (§3).
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", config.token)
            .parse()
            .map_err(|_| Error::Unauthorized)?,
    );

    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_INCOMING_FRAME))
        .max_frame_size(Some(MAX_INCOMING_FRAME));

    let (stream, _) =
        match tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false).await {
            Ok(ok) => ok,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                return Err(upgrade_error(resp.status()))
            }
            Err(e) => return Err(e.into()),
        };

    let (mut sink, mut stream) = stream.split();

    // Reads and writes run as independent tasks rather than two arms of one
    // `select!`. Sharing a loop made them block each other in both directions:
    // a continuously-ready inbound stream starved outbound verbs, and — worse —
    // the rate limiter's `sleep` sat inside the loop, so throttling writes also
    // stopped reading frames and stalled the heartbeat. Splitting them removes
    // the whole class of head-of-line blocking.
    let writer_verbs = verbs.clone();
    let mut writer_shutdown = shutdown.clone();
    let writer = tokio::spawn(async move {
        let mut bucket = Bucket::new();
        loop {
            tokio::select! {
                _ = writer_shutdown.changed() => break,
                verb = writer_verbs.recv() => {
                    let Ok(verb) = verb else { break };
                    let wait = bucket.acquire();
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                    let payload = match serde_json::to_string(&verb) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::error!(error = %e, "could not encode verb");
                            continue;
                        }
                    };
                    if sink.send(Message::text(payload)).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    let mut saw_frame = false;
    let result: Result<bool> = loop {
        tokio::select! {
            _ = shutdown.changed() => break Ok(saw_frame),

            incoming = stream.next() => {
                match incoming {
                    None => break Ok(saw_frame),
                    Some(Err(e)) => break Err(e.into()),
                    // Heartbeat is WS-level ping/pong, not JSON (§4.2).
                    // tungstenite answers Ping automatically as frames are
                    // read, so there is nothing to implement here — but the
                    // read loop must keep running or the server's ~60 s sweep
                    // will terminate us. That is the other reason writes can
                    // never be allowed to block this loop.
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) => break Ok(saw_frame),
                    Some(Ok(Message::Binary(_))) => {} // JSON text frames only
                    Some(Ok(Message::Text(text))) => {
                        if !saw_frame {
                            saw_frame = true;
                            // Signal connected on the FIRST FRAME, not on
                            // socket open: a refused upgrade looks like an
                            // open-then-close to some WS APIs (§4.4).
                            let _ = events.send(ClientEvent::State(ConnState::Connected)).await;
                        }
                        match serde_json::from_str::<ServerFrame>(&text) {
                            Ok(frame) => {
                                let _ = events.send(ClientEvent::Frame(Box::new(frame))).await;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "undecodable frame");
                                let _ = events
                                    .send(ClientEvent::Undecodable {
                                        raw: text.to_string(),
                                        error: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    };

    writer.abort();
    result?;

    let _ = events.send(ClientEvent::State(ConnState::Disconnected)).await;
    Ok(saw_frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_upgrades_scheme_and_always_sends_the_version() {
        let base = Url::parse("https://chat.irc.so/").unwrap();
        let url = ws_url(&base, 0).unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.path(), "/ws");
        assert!(url.query().unwrap().contains("v=1"));
        // A fresh connect omits `since` rather than sending 0.
        assert!(!url.query().unwrap().contains("since"));
    }

    #[test]
    fn ws_url_carries_the_resume_cursor() {
        let base = Url::parse("https://chat.irc.so/").unwrap();
        let url = ws_url(&base, 4242).unwrap();
        assert!(url.query().unwrap().contains("since=4242"));
    }

    #[test]
    fn plain_http_maps_to_ws() {
        let base = Url::parse("http://localhost:8015/").unwrap();
        assert_eq!(ws_url(&base, 0).unwrap().scheme(), "ws");
    }

    #[test]
    fn upgrade_statuses_map_to_their_documented_meanings() {
        // §4.1 lists these in the order the server checks them; each needs a
        // different client response, and only one of the three is retryable.
        assert!(matches!(upgrade_error(StatusCode::FORBIDDEN), Error::Forbidden));
        assert!(matches!(upgrade_error(StatusCode::UPGRADE_REQUIRED), Error::UpgradeRequired));
        assert!(matches!(upgrade_error(StatusCode::UNAUTHORIZED), Error::Unauthorized));
        assert!(!upgrade_error(StatusCode::UNAUTHORIZED).is_retryable());
        assert!(!upgrade_error(StatusCode::UPGRADE_REQUIRED).is_retryable());
        assert!(upgrade_error(StatusCode::BAD_GATEWAY).is_retryable());
    }

    #[test]
    fn bucket_allows_a_burst_then_throttles() {
        let mut b = Bucket::new();
        for _ in 0..BUCKET_CAPACITY as usize {
            assert!(b.acquire().is_zero(), "burst up to capacity should not wait");
        }
        assert!(!b.acquire().is_zero(), "past capacity the client must throttle itself");
    }
}
