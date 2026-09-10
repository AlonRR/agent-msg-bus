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
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_BACKOFF: u64 = 30;
/// Announce an outage to the session once the backoff has grown this far (~30s of failure). A
/// relay that cannot reach the broker leaves the session deaf, and a deaf session that believes it
/// is listening is the exact failure this project exists to remove. So it is surfaced - once per
/// outage, not on every retry.
const ANNOUNCE_AFTER_BACKOFF: u64 = 16;

/// **One relay per machine, multiplexing every session on it.**
///
/// The first cut was one relay per address, which forced a port per session and tied a process
/// lifecycle to a session — the exact shape the standing session-limits policy warns against, since
/// anything that must outlive a session belongs in a real service. This version listens once on a
/// fixed loopback port and opens a separate upstream connection per `addr`, so a session only needs
/// to know its own address, not a port allocation.
#[derive(Clone)]
pub struct RelayState {
    pub broker: Arc<String>,
    pub token: Arc<String>,
    /// Addresses currently held by a local subscriber, mirroring the broker's one-socket-per-address
    /// rule so two sessions cannot quietly share one mailbox.
    pub busy: Arc<Mutex<HashSet<String>>>,
}

#[derive(Deserialize)]
pub struct SubParams {
    pub addr: String,
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sub", get(sub))
        .with_state(state)
}

async fn health(State(st): State<RelayState>) -> Response {
    let held: Vec<String> = {
        let b = st.busy.lock().unwrap();
        let mut v: Vec<String> = b.iter().cloned().collect();
        v.sort();
        v
    };
    Json(serde_json::json!({
        "ok": true,
        "role": "relay",
        "version": crate::VERSION,
        "broker": *st.broker,
        "subscribed": held,
    }))
    .into_response()
}

async fn sub(
    State(st): State<RelayState>,
    Query(p): Query<SubParams>,
    ws: WebSocketUpgrade,
) -> Response {
    {
        let mut b = st.busy.lock().unwrap();
        if !b.insert(p.addr.clone()) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{} already has a local subscriber on this relay", p.addr)
                })),
            )
                .into_response();
        }
    }
    let addr = p.addr.clone();
    eprintln!("relay: {addr} subscribed");
    ws.on_upgrade(move |socket| pump(socket, st, addr))
}

/// One line per lifecycle event. The relay used to log a two-line banner and then nothing at all —
/// no connect, no disconnect, no upstream error — so a relay that accepted a handshake and then
/// dropped it after two seconds on an upstream failure looked identical to one working perfectly.
/// That silence cost real diagnostic time, which is the same complaint that retired the file bus.
fn log_event(addr: &str, what: &str) {
    eprintln!("relay: {addr} {what}");
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
async fn pump(local: WebSocket, st: RelayState, addr: String) {
    let release = |st: &RelayState| {
        st.busy.lock().unwrap().remove(&addr);
    };
    let upstream = upstream_url(&st.broker, &addr, &st.token);
    let (mut ltx, mut lrx) = local.split();
    let mut backoff: u64 = 1;
    let mut announced_down = false;
    // Outage bookkeeping. `outage_since` doubles as "is an outage in progress", and both it and
    // `last_announce` are cleared on reconnect so the next outage starts from silence again.
    let mut outage_since: Option<std::time::Instant> = None;
    let mut last_announce: Option<std::time::Instant> = None;
    let mut attempts: u32 = 0;
    // Time this process actually spent working on the outage: measured connect attempts plus the
    // backoff it intended to sleep. Unlike wall clock it does not advance while suspended, so the
    // difference between the two is what a sleeping laptop hides.
    let mut retrying = Duration::ZERO;

    loop {
        let attempt_started = std::time::Instant::now();
        match tokio_tungstenite::connect_async(upstream.as_str()).await {
            Ok((upstream, _)) => {
                log_event(&addr, "upstream connected");
                if announced_down {
                    let _ = ltx
                        .send(Ws::Text(
                            status_frame(&addr, "upstream_restored", "reconnected to the broker")
                                .into(),
                        ))
                        .await;
                    announced_down = false;
                }
                outage_since = None;
                last_announce = None;
                attempts = 0;
                retrying = Duration::ZERO;
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
                                    release(&st);
                                    return;
                                }
                            }
                            Some(Ok(_)) => {}          // pings/pongs/binary: transport noise
                            _ => {
                                log_event(&addr, "upstream closed; reconnecting");
                                break;
                            }
                        },
                        down = lrx.next() => match down {
                            None | Some(Err(_)) | Some(Ok(Ws::Close(_))) => {
                                // The session went away. Drop upstream too, so the broker sees this
                                // address as offline and queues rather than pushing into a void.
                                log_event(&addr, "local subscriber disconnected");
                                release(&st);
                                return;
                            }
                            _ => {}
                        },
                    }
                }
            }
            Err(e) => {
                let now = std::time::Instant::now();
                let since = *outage_since.get_or_insert(now);
                attempts += 1;
                retrying += attempt_started.elapsed();
                // The next sleep is this backoff; the doubling happens after it.
                if due_to_announce(backoff, last_announce.map(|t| now.duration_since(t))) {
                    let detail = if announced_down {
                        outage_detail(
                            now.duration_since(since),
                            attempts,
                            backoff,
                            retrying,
                            &e.to_string(),
                        )
                    } else {
                        format!("cannot reach the broker: {e}")
                    };
                    let _ = ltx
                        .send(Ws::Text(
                            status_frame(&addr, "upstream_unreachable", &detail).into(),
                        ))
                        .await;
                    announced_down = true;
                    last_announce = Some(now);
                }
                log_event(&addr, &format!("upstream connect FAILED ({e}); retrying in {backoff}s"));
            }
        }

        // Check the local side is still there before sleeping, so a session that exits during an
        // outage does not hold the relay busy for the length of the backoff.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            down = lrx.next() => {
                if matches!(down, None | Some(Err(_)) | Some(Ok(Ws::Close(_)))) {
                    release(&st);
                    return;
                }
            }
        }
        retrying += Duration::from_secs(backoff);
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

/// How often a still-unreachable upstream repeats itself. Long enough not to be chatter at a 30s
/// retry cadence, short enough that a session is never left wondering for long.
const RE_ANNOUNCE_EVERY: Duration = Duration::from_secs(300);

/// Whether a continuing outage should be announced now. `since_last` is `None` while nothing has
/// been said about this outage yet.
fn due_to_announce(backoff: u64, since_last: Option<Duration>) -> bool {
    match since_last {
        None => backoff >= ANNOUNCE_AFTER_BACKOFF,
        Some(d) => d >= RE_ANNOUNCE_EVERY,
    }
}

/// What a repeat announcement says. The point of each field is to be the thing silence could not
/// tell you: that time is passing, that the relay is still working, when it will try next, and
/// which failure it is seeing now — a code that changes mid-outage says the path is moving, which
/// is how an exit node coming back is distinguished from a broker that is simply gone.
fn outage_detail(
    elapsed: Duration,
    attempts: u32,
    next_in: u64,
    retrying: Duration,
    err: &str,
) -> String {
    let mut s = format!(
        "still cannot reach the broker: unreachable for {}, {attempts} attempts, next in {next_in}s",
        human(elapsed)
    );
    if let Some(gap) = unexplained_gap(elapsed, retrying) {
        s.push_str(&format!(
            " (only {} of that was spent retrying - this process was suspended or descheduled for \
             the other {}, so the attempt count is low for honest reasons)",
            human(retrying),
            human(gap)
        ));
    }
    s.push_str(&format!(". Current error: {err}"));
    s
}

/// Wall clock the process cannot account for as retry work.
///
/// `elapsed` is wall clock and keeps running while a laptop is asleep; `retrying` only accumulates
/// while this process is executing. Measured in the field, a suspended machine produced 787m12s
/// against 9 attempts — one attempt per 87 minutes, which the 30s cap makes impossible, and which
/// reads as a wedged retry loop to anyone who does not know the machine slept.
///
/// The tolerance is deliberately generous: a gap only counts when it dwarfs the retry work itself,
/// so ordinary scheduling slop stays silent. A qualifier that fires when nothing is wrong is the
/// same trained-to-ignore failure as chatter.
fn unexplained_gap(elapsed: Duration, retrying: Duration) -> Option<Duration> {
    let gap = elapsed.checked_sub(retrying)?;
    if gap > retrying.max(Duration::from_secs(60)) {
        Some(gap)
    } else {
        None
    }
}

fn human(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect: a long outage said one thing and then went quiet, which from inside a session is
    /// byte-identical to a broker that is never coming back. The relay knows the difference — it is
    /// still retrying every 30s — so silence is a choice it should not be making.
    #[test]
    fn a_continuing_outage_keeps_saying_so() {
        assert!(!due_to_announce(8, None), "announced before the threshold");
        assert!(due_to_announce(ANNOUNCE_AFTER_BACKOFF, None), "first announcement never fired");
        assert!(
            !due_to_announce(30, Some(Duration::from_secs(60))),
            "re-announced too soon; at a 30s retry this would be chatter"
        );
        assert!(
            due_to_announce(30, Some(RE_ANNOUNCE_EVERY)),
            "an outage still going after the interval said nothing"
        );
    }

    /// A repeat that said only "still down" would be no better than silence for diagnosis. Each
    /// field here is one the reader cannot get any other way without running commands by hand.
    #[test]
    fn the_repeat_carries_what_silence_could_not_tell_you() {
        let d = outage_detail(Duration::from_secs(752), 27, 30, Duration::from_secs(750), "os error 10065");
        assert!(d.contains("12m32s"), "elapsed time missing or unreadable: {d}");
        assert!(d.contains("27 attempts"), "attempt count missing: {d}");
        assert!(d.contains("next in 30s"), "next retry missing: {d}");
        assert!(d.contains("10065"), "the current error was dropped: {d}");

        // Under a minute reads as seconds rather than "0m07s".
        assert!(outage_detail(Duration::from_secs(7), 3, 4, Duration::from_secs(7), "x")
            .contains("for 7s"));
    }
    /// Measured in the field on a laptop that suspended mid-outage: 787m12s of wall clock against
    /// 9 attempts — one attempt per 87 minutes, which a 30s cap makes impossible. It is not a
    /// counter bug: `elapsed` is wall clock and keeps running while the process is suspended, while
    /// `attempts` counts only attempts that actually executed. Left alone, the frame reads as a
    /// wedged retry loop, which sends an operator debugging — reintroducing, in a narrower case,
    /// exactly the misreading this whole announcement exists to prevent.
    #[test]
    fn a_frame_from_a_suspended_process_does_not_read_as_a_wedged_retry_loop() {
        let d = outage_detail(
            Duration::from_secs(787 * 60 + 12),
            9,
            30,
            Duration::from_secs(8 * 60),
            "os error 10060",
        );
        assert!(d.contains("787m12s"), "the wall-clock outage was dropped: {d}");
        assert!(
            d.contains("8m00s"),
            "the frame never says how little of that was spent retrying: {d}"
        );
        assert!(
            d.contains("suspended"),
            "nothing tells the reader the gap is suspension rather than a stuck loop: {d}"
        );
    }

    /// ...and the ordinary case must stay quiet. A qualifier that fires when nothing is wrong is
    /// the same trained-to-ignore failure as chatter.
    #[test]
    fn an_uninterrupted_outage_carries_no_suspension_qualifier() {
        let d = outage_detail(
            Duration::from_secs(726),
            17,
            30,
            Duration::from_secs(720),
            "os error 10060",
        );
        assert!(d.contains("12m06s"), "elapsed missing: {d}");
        assert!(!d.contains("suspended"), "cried suspension on a healthy retry loop: {d}");
    }
}
