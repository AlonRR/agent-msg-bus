//! `agent-msg-bus watch <addr>` — a subscription that reconnects itself.
//!
//! **Why this exists.** Monitor's `ws:` source ENDS THE WATCH when the socket closes; it does not
//! retry. So a relay restart closes every subscription and each session is deaf from then on until a
//! human notices and re-arms. Mail is not lost — it queues — but for an idle session "queued" and
//! "lost" have the same effect, which is precisely the failure the file bus was retired for.
//!
//! Phase 0 named this as requirement 1 ("the client must re-arm on close"). The relay solved half of
//! it: it absorbs *upstream* outages so the local socket survives a broker blip. It cannot solve its
//! own restart, because that is the socket it is holding.
//!
//! Used with Monitor's `command:` form instead of `ws:`:
//!
//! ```text
//! Monitor({command: "agent-msg-bus watch machine-a/homelab.x", persistent: true, ...})
//! ```
//!
//! Monitor keeps a `command:` watch alive for as long as the process runs, and this process never
//! exits — so reconnection happens inside it and the watch is never torn down. That keeps push
//! delivery (sub-second) rather than falling back to polling, which was the obvious workaround and
//! costs up to a minute of latency.
//!
//! **One message per stdout line**, because Monitor turns each line into one notification — the same
//! rule as one-message-per-frame on the wire.

use futures_util::StreamExt;
use std::time::Duration;

const MAX_BACKOFF: u64 = 30;

/// Emit a line describing the watcher's own connectivity. These are deliberately visible: a watcher
/// that cannot reach its relay must not look like a quiet bus.
fn status(state: &str, detail: &str) {
    println!(
        "{}",
        serde_json::json!({
            "_watch": state,
            "detail": detail,
            "note": "agent-msg-bus watch reporting its own connectivity, not a message from another session.",
        })
    );
}

pub async fn run(relay: &str, addr: &str) -> ! {
    let url =
        format!("ws://{}/sub?addr={}", relay.trim_end_matches('/'), crate::client::urlencode(addr));
    let mut backoff: u64 = 1;
    let mut announced_down = false;
    let mut connected_once = false;

    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((sock, _)) => {
                if announced_down || !connected_once {
                    status("connected", &format!("subscribed as {addr} via {relay}"));
                }
                announced_down = false;
                connected_once = true;
                backoff = 1;

                let (_tx, mut rx) = sock.split();
                while let Some(msg) = rx.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => {
                            // Verbatim, one line per frame. Never merged: Monitor turns each line
                            // into one notification, so combining two messages would hide one.
                            println!("{}", t.to_string().replace('\n', "\\n"));
                        }
                        Ok(_) => {} // pings/pongs: transport noise
                        Err(_) => break,
                    }
                }
                // Reaching here means the socket closed — almost always the relay restarting.
                // Reconnect rather than exiting, which is the entire point of this subcommand.
                status("reconnecting", "subscription closed (relay restart?); retrying");
            }
            Err(e) => {
                // Announce a sustained outage once, not on every retry. A watcher that cannot reach
                // its relay is a deaf session, and that must be visible rather than quiet.
                if !announced_down && backoff >= 16 {
                    status("unreachable", &format!("cannot reach relay at {relay}: {e}"));
                    announced_down = true;
                }
                eprintln!("watch: connect failed ({e}); retrying in {backoff}s");
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
