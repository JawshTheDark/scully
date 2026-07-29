//! Create a pinned channel, a DM, and a DCC-named buffer for UI testing.
//!
//!     cargo run -p lurker-client --example setup_buffers -- <base> <token>

use lurker_client::socket::{self, ClientEvent, SocketConfig};
use lurker_proto::{ClientVerb, ServerFrame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base: url::Url = args.next().expect("base").parse().expect("url");
    let token = args.next().expect("token");

    let (tx, rx) = async_channel::unbounded();
    let sock = socket::spawn(SocketConfig { base, token }, tx, || 0);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut done = false;

    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let ClientEvent::Frame(frame) = event {
            if matches!(*frame, ServerFrame::BacklogComplete) && !done {
                done = true;
                // Join and pin a channel.
                // #TestChan already exists (network default channel); pin it.
                sock.send(ClientVerb::PinBuffer {
                    network_id: Some(1),
                    target: "#TestChan".into(),
                });
                // A DM (bare nick mints a DM row).
                sock.send(ClientVerb::OpenBuffer {
                    network_id: Some(1),
                    target: "buddy".into(),
                    count_by: None,
                });
                // A DCC-style buffer (=nick).
                sock.send(ClientVerb::OpenBuffer {
                    network_id: Some(1),
                    target: "=dccpal".into(),
                    count_by: None,
                });
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                println!("setup sent");
                break;
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    sock.shutdown();
}
