//! End-to-end tests against a real server on a real socket.
//!
//! These exercise the path a session actually uses — HTTP in, WebSocket out — rather than calling
//! the store directly. The store's own invariants are unit-tested in `src/store.rs`; what is tested
//! here is everything that sits between the store and a session, which is where the old bus failed.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite;

const TOKEN: &str = "test-token-abc";
static N: AtomicU32 = AtomicU32::new(0);

struct Harness {
    base: String,
    _dir: std::path::PathBuf,
}

impl Harness {
    fn client(&self) -> Client {
        Client::new(&self.base, TOKEN)
    }
    fn client_with(&self, token: &str) -> Client {
        Client::new(&self.base, token)
    }
    fn ws_url(&self, addr: &str, token: &str) -> String {
        let host = self.base.strip_prefix("http://").unwrap();
        format!(
            "ws://{}/sub?addr={}&token={}",
            host,
            agent_msg_bus::client::urlencode(addr),
            agent_msg_bus::client::urlencode(token)
        )
    }
}

async fn start() -> Harness {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-it-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();

    let tokens_path = dir.join("tokens.json");
    std::fs::write(&tokens_path, format!(r#"{{"test-machine":"{TOKEN}"}}"#)).unwrap();

    let db = dir.join("bus.db");
    let state = AppState {
        store: Arc::new(Mutex::new(Store::open(db.to_str().unwrap()).unwrap())),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(Auth::from_file(tokens_path.to_str().unwrap()).unwrap()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });

    Harness { base: format!("http://127.0.0.1:{port}"), _dir: dir }
}

/// ureq is blocking; keep it off the runtime threads driving the server.
async fn blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap()
}

type Sock = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn connect(h: &Harness, addr: &str) -> Sock {
    let (s, _) = tokio_tungstenite::connect_async(h.ws_url(addr, TOKEN)).await.unwrap();
    s
}

/// Next text frame, or None if nothing arrives in time. Pings are skipped: they are transport
/// keepalive, not messages.
async fn next_text(sock: &mut Sock, within: Duration) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => {
                return Some(serde_json::from_str(&t).unwrap())
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_subscriber_receives_a_message_pushed_to_it() {
    let h = start().await;
    let c = h.client();
    let (a, b) = ("machine-a/a".to_string(), "machine-a/b".to_string());

    let (ca, cb) = (a.clone(), b.clone());
    let c2 = h.client();
    blocking(move || {
        c2.register(&ca, "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register(&cb, "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    let mut sock = connect(&h, &b).await;
    tokio::time::sleep(Duration::from_millis(100)).await; // let the claim land before sending

    blocking(move || c.send("machine-a/a", "machine-a/b", "fyi", "hello", "body", "").unwrap()).await;

    let got = next_text(&mut sock, Duration::from_secs(5)).await.expect("no frame arrived");
    assert_eq!(got["subject"], "hello");
    assert_eq!(got["from"], a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_message_arrives_as_its_own_frame() {
    // Monitor turns each *frame* into one notification. Batching would collapse several messages
    // into a single event and destroy per-message granularity.
    let h = start().await;
    let c = h.client();
    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/b", "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    let mut sock = connect(&h, "machine-a/b").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    blocking(move || {
        for i in 0..3 {
            c.send("machine-a/a", "machine-a/b", "fyi", &format!("m{i}"), "", "").unwrap();
        }
    })
    .await;

    for i in 0..3 {
        let got = next_text(&mut sock, Duration::from_secs(5)).await.expect("missing frame");
        assert_eq!(got["subject"], format!("m{i}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_sent_while_offline_is_replayed_on_connect() {
    // This is what makes a LAN-only bus workable for machine-b: being away is not being unreachable.
    let h = start().await;
    let c = h.client();
    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-b/notes", "s2", "machine-b", "r", "/x", 2).unwrap();
    })
    .await;

    // nobody connected
    blocking(move || c.send("machine-a/a", "machine-b/notes", "fyi", "while you were out", "", "").unwrap())
        .await;

    let mut sock = connect(&h, "machine-b/notes").await;
    let got = next_text(&mut sock, Duration::from_secs(5)).await.expect("backlog was not replayed");
    assert_eq!(got["subject"], "while you were out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unacked_message_replays_on_reconnect_but_an_acked_one_does_not() {
    // The old bus consumed on read, so a crash between receiving and acting destroyed the message.
    // At-least-once: a duplicate is recoverable, a lost message is not.
    let h = start().await;
    let c = h.client();
    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/b", "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    blocking(move || c.send("machine-a/a", "machine-a/b", "fyi", "one", "", "").unwrap()).await;

    // receive, then "crash" without acking
    let mut sock = connect(&h, "machine-a/b").await;
    let first = next_text(&mut sock, Duration::from_secs(5)).await.expect("no first delivery");
    let id = first["id"].as_str().unwrap().to_string();
    drop(sock);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = connect(&h, "machine-a/b").await;
    let again = next_text(&mut sock, Duration::from_secs(5)).await;
    assert!(again.is_some(), "unacked message was lost on reconnect");
    assert_eq!(again.unwrap()["id"], id);

    // now ack and reconnect: it must not come back
    let c3 = h.client();
    let idc = id.clone();
    blocking(move || c3.ack("machine-a/b", &idc).unwrap()).await;
    drop(sock);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = connect(&h, "machine-a/b").await;
    let third = next_text(&mut sock, Duration::from_millis(1500)).await;
    assert!(third.is_none(), "an acked message was replayed: {third:?}");
}

/// A replayed message says so. Acting on a message feels like handling it, so the separate `ack`
/// step gets skipped — and the message then arrives again on every reconnect looking exactly like a
/// fresh duplicate. The flag turns a silent repeat into a signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_message_is_marked_as_a_replay_but_a_live_one_is_not() {
    let h = start().await;
    let c = h.client();
    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/b", "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    // Delivered live to an attached subscriber: not a replay.
    let mut sock = connect(&h, "machine-a/b").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    blocking(move || c.send("machine-a/a", "machine-a/b", "fyi", "first delivery", "", "").unwrap()).await;
    let live = next_text(&mut sock, Duration::from_secs(5)).await.expect("no live delivery");
    assert!(live.get("replay").is_none(), "a first delivery was marked as a replay");

    // Reconnect without acking: the same message comes back, and now it admits it.
    drop(sock);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut sock = connect(&h, "machine-a/b").await;
    let again = next_text(&mut sock, Duration::from_secs(5)).await.expect("unacked message lost");
    assert_eq!(again["id"], live["id"]);
    assert_eq!(again["replay"], true, "a replayed message was not marked");
    assert!(
        again["replay_note"].as_str().unwrap_or("").contains("never acked"),
        "the replay note should say why it is being sent again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sender_never_receives_its_own_broadcast() {
    let h = start().await;
    let c = h.client();
    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/b", "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    let mut sock_a = connect(&h, "machine-a/a").await;
    let mut sock_b = connect(&h, "machine-a/b").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    blocking(move || c.send("machine-a/a", "machine-a/*", "fyi", "broadcast", "", "").unwrap()).await;

    assert!(next_text(&mut sock_b, Duration::from_secs(5)).await.is_some(), "peer missed broadcast");
    assert!(
        next_text(&mut sock_a, Duration::from_millis(1500)).await.is_none(),
        "sender received its own broadcast"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_token_is_rejected_visibly_on_both_surfaces() {
    // A rejection must never be indistinguishable from an idle bus. That ambiguity is exactly what
    // let the old system sit broken for five days looking healthy.
    let h = start().await;

    let bad = h.client_with("not-the-token");
    let err = blocking(move || bad.peers(false).map(|_| ())).await.unwrap_err();
    assert!(format!("{err}").contains("401"), "expected a visible 401, got: {err}");

    let res = tokio_tungstenite::connect_async(h.ws_url("machine-a/x", "not-the-token")).await;
    assert!(res.is_err(), "websocket accepted a bad token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_address_held_by_a_live_socket_cannot_be_stolen_without_force() {
    // Two sockets on one address would each consume part of the other's mail - the same class of
    // bug as the old machine-wide whoami file, where the second session silently took over the
    // first one's identity.
    let h = start().await;
    let _first = connect(&h, "machine-a/a").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let second = tokio_tungstenite::connect_async(h.ws_url("machine-a/a", TOKEN)).await;
    assert!(second.is_err(), "a second socket claimed a live address");

    let host = h.base.strip_prefix("http://").unwrap();
    let forced = tokio_tungstenite::connect_async(format!(
        "ws://{host}/sub?addr=machine-a%2Fa&token={TOKEN}&force=1"
    ))
    .await;
    assert!(forced.is_ok(), "force=1 could not reclaim after a crash");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_reports_live_state_from_the_socket_not_a_guess() {
    let h = start().await;
    let c = h.client();
    blocking(move || {
        c.register("machine-a/a", "s1", "machine-a", "homelab", "/x", 1).unwrap();
        c.register("machine-a/b", "s2", "machine-a", "homelab", "/x", 2).unwrap();
    })
    .await;

    let _sock = connect(&h, "machine-a/a").await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let c2 = h.client();
    let p = blocking(move || c2.peers(true).unwrap()).await;
    assert_eq!(p.live, vec!["machine-a/a".to_string()]);
    let b = p.known.iter().find(|k| k.addr == "machine-a/b").unwrap();
    assert!(!b.live, "an address with no socket was reported live");
}
