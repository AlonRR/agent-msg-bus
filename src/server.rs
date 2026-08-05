//! HTTP + WebSocket surface. This is the frozen wire contract from `docs/plan.md`.
//!
//! Clients bind to `/register`, `/send`, `/ack`, `/peers`, `/sub` — never to the storage behind
//! them. That is what makes the core swappable: if hand-rolled durability disappoints, this surface
//! can sit in front of NATS instead and no client changes.

use crate::hub::{Auth, Hub};
use crate::store::{Message, NewMessage, Registration, Store};
use axum::extract::ws::{CloseCode, CloseFrame, Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// WebSocket close code 1001: the endpoint is going away.
///
/// The Phase 0 probe simply exited, which the client saw as 1006 (abnormal, no close handshake).
/// A client cannot tell that apart from a network fault, so an orderly shutdown must say so.
const CLOSE_GOING_AWAY: CloseCode = 1001;

/// Idle sockets survived 60 minutes of total silence on loopback during Phase 0, so this is a
/// precaution against the *untested* LAN path through the lab's firewall — not a fix for an observed
/// failure. Do not let this comment drift into claiming otherwise.
const PING_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub hub: Arc<Hub>,
    pub auth: Arc<Auth>,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/register", post(register))
        .route("/send", post(send))
        .route("/ack", post(ack))
        .route("/peers", get(peers))
        .route("/sub", get(sub))
        .with_state(state)
}

// ---- auth ------------------------------------------------------------------

fn token_from(headers: &HeaderMap, q: &HashMap<String, String>) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(rest) = v.strip_prefix("Bearer ") {
            return Some(rest.to_string());
        }
    }
    q.get("token").cloned()
}

/// A rejection is always an explicit, visible 401 with a reason. The failure mode this project
/// exists to eliminate is the silent one: the old bus's watch registration failed quietly and
/// looked exactly like an idle bus for five days.
fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid or missing token"})))
        .into_response()
}

// ---- HTTP ------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
pub struct RegisterBody {
    pub addr: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub machine: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub pid: i64,
}

async fn register(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Json(b): Json<RegisterBody>,
) -> Response {
    if !st.auth.check(token_from(&headers, &q).as_deref()) {
        return unauthorized();
    }
    let reg = Registration {
        addr: b.addr.clone(),
        session_id: b.session_id,
        machine: b.machine,
        repo: b.repo,
        cwd: b.cwd,
        pid: b.pid,
    };
    match st.store.lock().unwrap().register(&reg) {
        Ok(()) => Json(serde_json::json!({"addr": b.addr})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
pub struct SendBody {
    pub from: String,
    pub to: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub reply_to: String,
}

fn default_kind() -> String {
    "fyi".into()
}

async fn send(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Json(b): Json<SendBody>,
) -> Response {
    if !st.auth.check(token_from(&headers, &q).as_deref()) {
        return unauthorized();
    }
    let nm = NewMessage {
        from: b.from,
        to: b.to,
        kind: b.kind,
        subject: b.subject,
        body: b.body,
        reply_to: b.reply_to,
    };

    // Store first, then push. If the process died between the two, the message is still on disk and
    // replays on the recipient's next connect. Pushing first would risk the opposite: a delivery
    // that no longer exists anywhere if the write then failed.
    let stored = {
        let store = st.store.lock().unwrap();
        match store.send(&nm) {
            Ok(id) => match store.by_id(&id) {
                Ok(Some(m)) => m,
                Ok(None) => return server_error_msg("message vanished immediately after insert"),
                Err(e) => return server_error(e),
            },
            Err(e) => return server_error(e),
        }
    };

    let pushed = st.hub.deliver(&stored);
    Json(serde_json::json!({"id": stored.id, "pushed_to": pushed})).into_response()
}

#[derive(Deserialize)]
pub struct AckBody {
    pub addr: String,
    pub up_to_id: String,
}

async fn ack(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Json(b): Json<AckBody>,
) -> Response {
    if !st.auth.check(token_from(&headers, &q).as_deref()) {
        return unauthorized();
    }
    match st.store.lock().unwrap().ack(&b.addr, &b.up_to_id) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Serialize)]
struct PeersOut {
    live: Vec<String>,
    known: Vec<KnownPeer>,
}

#[derive(Serialize)]
struct KnownPeer {
    addr: String,
    machine: String,
    repo: String,
    cwd: String,
    live: bool,
    pending: usize,
}

async fn peers(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !st.auth.check(token_from(&headers, &q).as_deref()) {
        return unauthorized();
    }
    let store = st.store.lock().unwrap();
    let regs = match store.peers() {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let known = regs
        .into_iter()
        .map(|r| KnownPeer {
            live: st.hub.is_live(&r.addr),
            pending: store.pending_for(&r.addr).map(|v| v.len()).unwrap_or(0),
            addr: r.addr,
            machine: r.machine,
            repo: r.repo,
            cwd: r.cwd,
        })
        .collect();
    Json(PeersOut { live: st.hub.live(), known }).into_response()
}

fn server_error(e: rusqlite::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()})))
        .into_response()
}

fn server_error_msg(m: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": m}))).into_response()
}

// ---- WebSocket -------------------------------------------------------------

#[derive(Deserialize)]
pub struct SubParams {
    pub addr: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub force: Option<String>,
}

async fn sub(
    State(st): State<AppState>,
    Query(p): Query<SubParams>,
    ws: WebSocketUpgrade,
) -> Response {
    if !st.auth.check(p.token.as_deref()) {
        return unauthorized();
    }
    let force = matches!(p.force.as_deref(), Some("1") | Some("true"));
    let (conn_id, rx) = match st.hub.claim(&p.addr, force) {
        Some(v) => v,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("address {} is already held by a live socket", p.addr),
                    "hint": "pass force=1 to take it over after a crash",
                })),
            )
                .into_response()
        }
    };
    let addr = p.addr.clone();
    ws.on_upgrade(move |socket| drive(socket, st, addr, conn_id, rx))
}

/// One socket's lifetime: replay whatever is unacked, then stream new messages, pinging so an idle
/// connection is not mistaken for a dead one.
///
/// **One message per text frame, always.** Monitor turns each *frame* into one notification, so
/// batching several messages into one frame would collapse them into a single event and lose the
/// per-message granularity the whole design depends on.
async fn drive(
    socket: WebSocket,
    st: AppState,
    addr: String,
    conn_id: crate::hub::ConnId,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
) {
    let (mut tx, mut incoming) = socket.split();

    // Replay first. These are messages the recipient has not acked - either it was offline when
    // they were sent, or it received them and died before acking. At-least-once: a duplicate is
    // recoverable, a lost message is not.
    let backlog = st.store.lock().unwrap().pending_for(&addr).unwrap_or_default();
    for m in backlog {
        if send_msg(&mut tx, &m).await.is_err() {
            st.hub.release(&addr, conn_id);
            return;
        }
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick completes immediately

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => {
                    if send_msg(&mut tx, &m).await.is_err() { break; }
                }
                None => break, // sender dropped: this address was force-claimed by another socket
            },
            _ = ping.tick() => {
                if tx.send(Ws::Ping(Vec::new().into())).await.is_err() { break; }
            },
            frame = incoming.next() => match frame {
                Some(Ok(Ws::Close(_))) | None => break,
                Some(Err(_)) => break,
                Some(Ok(_)) => {} // clients are receive-only here; pongs and stray frames are ignored
            },
        }
    }

    let _ = tx
        .send(Ws::Close(Some(CloseFrame {
            code: CLOSE_GOING_AWAY,
            reason: "going away".into(),
        })))
        .await;
    st.hub.release(&addr, conn_id);
}

async fn send_msg(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Ws>,
    m: &Message,
) -> Result<(), axum::Error> {
    let json = serde_json::to_string(m).unwrap_or_else(|_| "{}".into());
    tx.send(Ws::Text(json.into())).await
}
