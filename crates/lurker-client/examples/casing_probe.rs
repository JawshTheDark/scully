//! Does the wire target's casing change what `history` returns?
//!
//! `messages.target` is a BINARY-collated column holding whatever casing the
//! network handed the server, and the request's casing is only resolved when a
//! `buffers` row exists to resolve it against (`wsHub.ts:3098`). This sends the
//! same `history`/`latest` request twice — once with the folded (lowercased)
//! target a client might naively use as its wire value, once with the canonical
//! casing — and prints how many events each returns.
//!
//!     cargo run -p lurker-client --example casing_probe -- <base-url> <token> '#TestChan'

use lurker_client::socket::{self, ClientEvent, SocketConfig};
use lurker_proto::{ClientVerb, HistoryMode, PageUnit, ServerFrame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base: url::Url = args.next().expect("base url").parse().expect("valid url");
    let token = args.next().expect("token");
    let canonical = args.next().unwrap_or_else(|| "#TestChan".into());
    let folded = canonical.to_ascii_lowercase();

    let (tx, rx) = async_channel::unbounded();
    let sock = socket::spawn(SocketConfig { base, token }, tx, || 0);

    let mut sent_messages = false;
    let mut asked_folded = false;
    let mut asked_canonical = false;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);

    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match event {
            ClientEvent::State(s) => println!("[state] {s}"),
            ClientEvent::Undecodable { raw, error } => {
                println!("\n[UNDECODABLE] error: {error}\n  raw: {raw}\n");
            }
            ClientEvent::Frame(frame) => match *frame {
                ServerFrame::BacklogComplete => {
                    println!("[burst complete]");
                    if !sent_messages {
                        sent_messages = true;
                        for i in 1..=3 {
                            sock.send(ClientVerb::Send {
                                network_id: 1,
                                target: canonical.clone(),
                                text: format!("probe message {i}"),
                                client_id: Some(format!("p{i}")),
                            });
                        }
                        // Let the echoes land and persist.
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                        println!("\n→ history latest, target = {folded:?} (folded)");
                        sock.send(ClientVerb::History {
                            network_id: Some(1),
                            target: folded.clone(),
                            mode: HistoryMode::Latest,
                            limit: 50,
                            token: Some(1),
                            count_by: Some(PageUnit::Renderable),
                            before: None,
                            after_id: None,
                            anchor_id: None,
                        });
                        asked_folded = true;
                    }
                }
                ServerFrame::History(h) => {
                    let which = match h.token {
                        Some(1) => "folded",
                        Some(2) => "canonical",
                        _ => "?",
                    };
                    println!(
                        "← history [{which}] target={:?} events={} hasMoreOlder={}",
                        h.target,
                        h.events.len(),
                        h.has_more_older
                    );
                    if asked_folded && !asked_canonical {
                        asked_canonical = true;
                        println!("\n→ history latest, target = {canonical:?} (canonical)");
                        sock.send(ClientVerb::History {
                            network_id: Some(1),
                            target: canonical.clone(),
                            mode: HistoryMode::Latest,
                            limit: 50,
                            token: Some(2),
                            count_by: Some(PageUnit::Renderable),
                            before: None,
                            after_id: None,
                            anchor_id: None,
                        });
                    } else if h.token == Some(2) {
                        break;
                    }
                }
                ServerFrame::Backlog(b) => {
                    println!(
                        "[backlog] target={:?} mode={:?} events={}",
                        b.target,
                        b.merge_mode(),
                        b.events.len()
                    );
                }
                ServerFrame::Error { text } => println!("[server error] {text:?}"),
                _ => {}
            },
        }
    }
    sock.shutdown();
}
