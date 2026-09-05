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

/// Give the fallback name real metadata, best-effort.
///
/// `/sub` already guarantees a row and a cursor exist, so this is not what keeps the session
/// visible — it is what stops the fallback appearing on the roster as a bare name with no repo,
/// indistinguishable from a stray. Never fatal: a watcher that cannot register is still a watcher
/// that is receiving, and refusing to run would trade a cosmetic gap for a deaf session.
fn register_bound(addr: &str) {
    let Some(cfg) = crate::hook::load_config() else { return };
    let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
    let repo = addr.rsplit('/').next().unwrap_or_default().split('.').next().unwrap_or_default();
    let client = crate::client::Client::new(&cfg.url, &cfg.token);
    if let Err(e) = client.register(
        addr,
        &std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default(),
        &cfg.machine,
        repo,
        &cwd,
        std::process::id() as i64,
    ) {
        eprintln!("watch: bound {addr} but could not register it ({e})");
    }
}

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

type Sock = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn sub_url(relay: &str, addr: &str) -> String {
    format!("ws://{}/sub?addr={}", relay.trim_end_matches('/'), crate::client::urlencode(addr))
}

/// Is this the relay saying "somebody else already holds that address"?
///
/// Matched on the typed HTTP status rather than on the text of the error, which is a rendering and
/// can change without notice.
pub fn is_conflict(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    matches!(e, tokio_tungstenite::tungstenite::Error::Http(r) if r.status().as_u16() == 409)
}

/// Claim `primary`, or `fallback` if `primary` is already held by a live socket. Returns the socket
/// and **the name actually bound**.
///
/// This is where repo-scoped addressing is actually decided. The repo name is claimed
/// optimistically, and a collision is discovered ATOMICALLY, as a 409 at claim time — never guessed
/// in advance from a session id. The relay answers that 409 out of its own local `busy` set before
/// it opens anything upstream, so two sessions on one machine are resolved machine-locally and the
/// broker needs no change at all: this ships to a client without touching the server.
///
/// `fallback` is `None` for a PINNED address, and that is deliberate — a pin is an explicit
/// declaration of identity, so a collision on one must surface as a failure rather than quietly
/// answering to some other name.
pub async fn bind(
    relay: &str,
    primary: &str,
    fallback: Option<&str>,
) -> Result<(Sock, String), tokio_tungstenite::tungstenite::Error> {
    match tokio_tungstenite::connect_async(sub_url(relay, primary)).await {
        Ok((sock, _)) => Ok((sock, primary.to_string())),
        Err(e) => match fallback {
            Some(f) if is_conflict(&e) => {
                let (sock, _) = tokio_tungstenite::connect_async(sub_url(relay, f)).await?;
                Ok((sock, f.to_string()))
            }
            _ => Err(e),
        },
    }
}

pub async fn run(relay: &str, addr: &str, fallback: Option<&str>) -> ! {
    let mut backoff: u64 = 1;
    let mut announced_down = false;
    let mut connected_once = false;
    // The bound name is STICKY. Fallback is a first-connect decision only: after that, a 409 on
    // reconnect is almost always this watcher's own socket not yet released by the relay, and
    // falling back again would change the session's identity mid-life — so every peer's `peers`
    // output, and every reply already in flight, would be addressed to a name it had stopped
    // answering to.
    let mut bound = addr.to_string();

    loop {
        // Fallback is a FIRST-CONNECT decision only. Once a name is bound, reconnects go straight
        // back to it: a 409 then is almost always this watcher's own socket not yet released.
        let attempt = if connected_once {
            tokio_tungstenite::connect_async(sub_url(relay, &bound))
                .await
                .map(|(s, _)| (s, bound.clone()))
        } else {
            bind(relay, addr, fallback).await
        };
        match attempt {
            Ok((sock, name)) => {
                if name != addr {
                    // A notification, not a log line. A session quietly answering to a different
                    // name than the one it was handed is precisely the "healthy-looking and
                    // disconnected" shape this project exists to remove — and the sender who is
                    // about to write to the other name has no way to find out.
                    status(
                        "fallback",
                        &format!(
                            "{addr} is already held by another live session on this machine, so \
                             this session bound {name} instead. Mail sent to {addr} will NOT reach \
                             it. Tell peers this address, or pin a role name for a stable one."
                        ),
                    );
                    register_bound(&name);
                }
                bound = name;
                if announced_down || !connected_once {
                    status("connected", &format!("subscribed as {bound} via {relay}"));
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
            Err(e) if !connected_once && is_conflict(&e) => {
                // BOTH names are held: the fallback cannot help, and this session is receiving
                // nothing. Announced IMMEDIATELY rather than after the backoff grows, and on stdout
                // rather than stderr, because Monitor turns stdout lines into notifications and
                // stderr into nothing — so the previous version of this branch left a session deaf
                // and silent at the same time, which is the fallback wearing the exact disguise
                // this project exists to strip off.
                if !announced_down {
                    status(
                        "unreachable",
                        &format!(
                            "{addr} and {} are BOTH already held by live sockets on this machine, \
                             so this session is receiving nothing. Retrying; it will connect when \
                             one of them is released. Check `agent-msg-bus peers` to see who holds \
                             them, or pin a role name to claim a mailbox of your own.",
                            fallback.unwrap_or("(no fallback offered)")
                        ),
                    );
                    announced_down = true;
                }
                eprintln!("watch: both names held ({e}); retrying in {backoff}s");
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
