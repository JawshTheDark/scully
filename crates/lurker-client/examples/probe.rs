//! Live protocol probe — no credentials required.
//!
//! Verifies against a real instance the parts of the client that don't need a
//! session: `/api/config` parsing, compatibility checking, and that the WS
//! upgrade path (TLS, URL construction, Bearer header) reaches the server and
//! that a rejected token maps to the documented `401` rather than a generic
//! transport error.
//!
//!     cargo run -p lurker-client --example probe -- https://your-lurker

use lurker_client::{socket, ConnState, Rest};

#[tokio::main]
async fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:8015".into());
    let base = url::Url::parse(&base).expect("valid URL");
    let base = if base.path().ends_with('/') { base } else { format!("{base}/").parse().unwrap() };

    let rest = Rest::new(base.clone()).unwrap();

    println!("── GET /api/config ──");
    match rest.config().await {
        Ok(cfg) => {
            println!("  edition            {:?}", cfg.edition);
            println!("  protocolVersion    {}", cfg.protocol_version);
            println!("  minProtocolVersion {}", cfg.min_protocol_version);
            println!(
                "  client v{} compatible: {}",
                lurker_proto::PROTOCOL_VERSION,
                cfg.is_compatible()
            );
        }
        Err(e) => {
            println!("  FAILED: {e}");
            return;
        }
    }

    println!("\n── WS upgrade with a deliberately invalid token ──");
    let (tx, rx) = async_channel::unbounded();
    let sock = socket::spawn(
        socket::SocketConfig { base, token: "not-a-real-token".into() },
        tx,
        || 0,
    );

    // Expect: Connecting, then a terminal Failed carrying the 401 meaning.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let socket::ClientEvent::State(state) = &ev {
            println!("  state: {state}");
            match state {
                ConnState::Failed(_) => {
                    println!("\n  ✓ rejected token produced a terminal, non-retryable state");
                    break;
                }
                ConnState::Backoff(_) => {
                    println!("\n  ✗ treated as retryable — the 401 was not recognised");
                    break;
                }
                _ => {}
            }
        }
    }
    sock.shutdown();
}
