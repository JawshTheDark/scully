//! Fill a channel with history on a throwaway instance, so a fresh client
//! start has a real shell to hydrate.
//!
//!     cargo run -p lurker-client --example seed -- <base> <token> '#TestChan' 40

use lurker_client::socket::{self, ClientEvent, SocketConfig};
use lurker_proto::{ClientVerb, ServerFrame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base: url::Url = args.next().expect("base").parse().expect("url");
    let token = args.next().expect("token");
    let target = args.next().unwrap_or_else(|| "#TestChan".into());
    let count: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(40);

    let (tx, rx) = async_channel::unbounded();
    let sock = socket::spawn(SocketConfig { base, token }, tx, || 0);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut sent = false;
    let mut seen = 0usize;

    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let ClientEvent::Frame(frame) = event {
            match *frame {
                ServerFrame::BacklogComplete if !sent => {
                    sent = true;
                    for i in 1..=count {
                        sock.send(ClientVerb::Send {
                            network_id: 1,
                            target: target.clone(),
                            text: format!("history line {i} — the quick brown fox jumps over the lazy dog"),
                            client_id: Some(format!("s{i}")),
                        });
                        // Stay well inside the server's token bucket.
                        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    }
                }
                ServerFrame::Irc(e) if e.is_self => {
                    seen += 1;
                    if seen >= count {
                        println!("seeded {seen} messages into {target}");
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    sock.shutdown();
}
