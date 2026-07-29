//! Set a couple of bans on a channel so the ban list has entries to show.
//!
//!     cargo run -p lurker-client --example setban -- <base> <token> '#chan'

use lurker_client::socket::{self, ClientEvent, SocketConfig};
use lurker_proto::{ClientVerb, ServerFrame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base: url::Url = args.next().expect("base").parse().expect("url");
    let token = args.next().expect("token");
    let channel = args.next().expect("channel");

    let (tx, rx) = async_channel::unbounded();
    let sock = socket::spawn(SocketConfig { base, token }, tx, || 0);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut done = false;

    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let ClientEvent::Frame(frame) = event {
            if matches!(*frame, ServerFrame::BacklogComplete) && !done {
                done = true;
                for mask in ["*!*@evil.example.net", "spammer!*@*", "*!bad@*.test"] {
                    sock.send(ClientVerb::Raw {
                        network_id: 1,
                        line: format!("MODE {channel} +b {mask}"),
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
                println!("bans set");
                break;
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    sock.shutdown();
}
