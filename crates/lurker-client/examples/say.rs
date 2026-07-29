//! Send one line into a channel and set a colourful topic — repro seeding.
//!
//!     cargo run -p lurker-client --example say -- <base> <token> '#chan' 'text'

use lurker_client::socket::{self, ClientEvent, SocketConfig};
use lurker_proto::{ClientVerb, ServerFrame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base: url::Url = args.next().expect("base").parse().expect("url");
    let token = args.next().expect("token");
    let target = args.next().expect("channel");
    let text = args.next().expect("text");

    let (tx, rx) = async_channel::unbounded();
    let sock = socket::spawn(SocketConfig { base, token }, tx, || 0);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let ClientEvent::Frame(frame) = event {
            match *frame {
                ServerFrame::BacklogComplete => {
                    // A topic with mIRC colours, to exercise the dialog's
                    // rendered preview.
                    sock.send(ClientVerb::Raw {
                        network_id: 1,
                        line: format!(
                            "TOPIC {target} :\u{03}8welcome\u{03} to the \u{02}test\u{02} channel \u{03}12— now in colour\u{03}"
                        ),
                    });
                    sock.send(ClientVerb::Send {
                        network_id: 1,
                        target: target.clone(),
                        text: text.clone(),
                        client_id: Some("say1".into()),
                    });
                }
                ServerFrame::Irc(e) if e.is_self && e.event_type.is_chat() => {
                    println!("sent");
                    break;
                }
                _ => {}
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    sock.shutdown();
}
