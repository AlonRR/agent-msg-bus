//! Loopback relay — the last hop, and the reason it exists.
//!
//! **Monitor refuses to open a WebSocket to any RFC1918, link-local or cloud-metadata address.** It
//! rejects client-side, before any network traffic:
//!
//! ```text
//! ws://<broker-ip>:9450 -> "the address is in a private, link-local, or cloud-metadata range"
//! ```
//!
//! Loopback is permitted. So a session cannot subscribe to the broker on the LAN directly, but it
//! can subscribe to a process on `127.0.0.1` that holds the LAN connection on its behalf. The relay
//! is an ordinary client with no such guard.
//!
//! It earns its keep twice over, because it also absorbs reconnection. Phase 0 established that a
//! `persistent: true` Monitor ends when its socket ends, and that something must re-arm or the
//! session is silently deaf. Here the *local* socket stays up across upstream outages: the relay
//! reconnects underneath, and Monitor never notices.
//!
//! Upstream is opened lazily, only while a local subscriber is attached. That keeps the broker's
//! queue semantics honest — no local subscriber means no socket at the broker, which means
//! "offline", which means mail queues rather than being pushed into a void.

use axum::extract::ws::{Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MAX_BACKOFF: u64 = 30;
/// Announce an outage to the session once the backoff has grown this far (~30s of failure). A
/// relay that cannot reach the broker leaves the session deaf, and a deaf session that believes it
/// is listening is the exact failure this project exists to remove. So it is surfaced - once per
/// outage, not on every retry.
const ANNOUNCE_AFTER_BACKOFF: u64 = 16;

#[derive(Clone)]
pub struct RelayState {
    pub upstream_url: Arc<String>,
    pub addr: Arc<String>,
    /// One local subscriber at a time, mirroring the broker's one-socket-per-address rule.
    pub busy: Arc<AtomicBool>,
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(serde_json::json!({"ok": true, "role": "relay"})) }))
        .route("/sub", get(sub))
        .with_state(state)
}

async fn sub(State(st): State<RelayState>, ws: WebSocketUpgrade) -> Response {
    if st.busy.swap(true, Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "relay already has a local subscriber"})),
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| pump(socket, st))
}

fn status_frame(addr: &str, state: &str, detail: &str) -> String {
    serde_json::json!({
        "_relay": state,
        "addr": addr,
        "detail": detail,
        "note": "This is the relay reporting its own connectivity, not a message from another session.",
    })
    .to_string()
}

/// Hold the local socket; keep an upstream connection under it for as long as the local one lives.
async fn pump(local: WebSocket, st: RelayState) {
    let (mut ltx, mut lrx) = local.split();
    let mut backoff: u64 = 1;
    let mut announced_down = false;

    loop {
        match tokio_tungstenite::connect_async(st.upstream_url.as_str()).await {
            Ok((upstream, _)) => {
                if announced_down {
                    let _ = ltx
                        .send(Ws::Text(
                            status_frame(&st.addr, "upstream_restored", "reconnected to the broker")
                                .into(),
                        ))
                        .await;
                    announced_down = false;
                }
                backoff = 1;
                let (_utx, mut urx) = upstream.split();

                loop {
                    tokio::select! {
                        up = urx.next() => match up {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                                // Forwarded verbatim, one frame in -> one frame out. Monitor turns
                                // each frame into one notification; merging here would collapse
                                // separate messages into a single event.
                                if ltx.send(Ws::Text(t.to_string().into())).await.is_err() {
                                    st.busy.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                            Some(Ok(_)) => {}          // pings/pongs/binary: transport noise
                            _ => break,                 // upstream gone -> reconnect below
                        },
                        down = lrx.next() => match down {
                            None | Some(Err(_)) | Some(Ok(Ws::Close(_))) => {
                                // The session went away. Drop upstream too, so the broker sees this
                                // address as offline and queues rather than pushing into a void.
                                st.busy.store(false, Ordering::SeqCst);
                                return;
                            }
                            _ => {}
                        },
                    }
                }
            }
            Err(e) => {
                if !announced_down && backoff >= ANNOUNCE_AFTER_BACKOFF {
                    let _ = ltx
                        .send(Ws::Text(
                            status_frame(
                                &st.addr,
                                "upstream_unreachable",
                                &format!("cannot reach the broker: {e}"),
                            )
                            .into(),
                        ))
                        .await;
                    announced_down = true;
                }
                eprintln!("relay: upstream connect failed ({e}); retrying in {backoff}s");
            }
        }

        // Check the local side is still there before sleeping, so a session that exits during an
        // outage does not hold the relay busy for the length of the backoff.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            down = lrx.next() => {
                if matches!(down, None | Some(Err(_)) | Some(Ok(Ws::Close(_)))) {
                    st.busy.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Build the upstream `/sub` URL. Kept here so the token is assembled in one place.
pub fn upstream_url(broker_base: &str, addr: &str, token: &str) -> String {
    let host = broker_base
        .strip_prefix("http://")
        .or_else(|| broker_base.strip_prefix("https://"))
        .unwrap_or(broker_base)
        .trim_end_matches('/');
    format!(
        "ws://{}/sub?addr={}&token={}",
        host,
        crate::client::urlencode(addr),
        crate::client::urlencode(token)
    )
}
