//! Relay tests: broker + relay + a local subscriber, all on loopback.
//!
//! The relay exists because Monitor refuses to open a WebSocket to a private IP, so in production
//! the broker is across the LAN. These tests keep everything on loopback - they cover the relay's
//! logic (multiplexing, lazy upstream, one-subscriber-per-address), not the guard itself, which is
//! Monitor's behaviour and not ours to test.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::relay::RelayState;
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use futures_util::StreamExt;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite;

const TOKEN: &str = "relay-test-token";
static N: AtomicU32 = AtomicU32::new(0);

struct Rig {
    broker: String,
    relay_port: u16,
}

async fn start() -> Rig {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-relay-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    let tokens = dir.join("tokens.json");
    std::fs::write(&tokens, format!(r#"{{"test":"{TOKEN}"}}"#)).unwrap();
    let db = dir.join("bus.db");

    let state = AppState {
        store: Arc::new(Mutex::new(Store::open(db.to_str().unwrap()).unwrap())),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(Auth::from_file(tokens.to_str().unwrap()).unwrap()),
    };
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bport = bl.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(bl, app(state)).await.unwrap() });

    let rstate = RelayState {
        broker: Arc::new(format!("http://127.0.0.1:{bport}")),
        token: Arc::new(TOKEN.to_string()),
        busy: Arc::new(Mutex::new(HashSet::new())),
    };
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rport = rl.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(rl, agent_msg_bus::relay::app(rstate)).await.unwrap() });

    Rig { broker: format!("http://127.0.0.1:{bport}"), relay_port: rport }
}

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

async fn sub_via_relay(rig: &Rig, addr: &str) -> Result<Sock, String> {
    let url = format!(
        "ws://127.0.0.1:{}/sub?addr={}",
        rig.relay_port,
        agent_msg_bus::client::urlencode(addr)
    );
    tokio_tungstenite::connect_async(url).await.map(|(s, _)| s).map_err(|e| e.to_string())
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_reaches_a_subscriber_through_the_relay() {
    let rig = start().await;
    let c = Client::new(&rig.broker, TOKEN);
    let c2 = Client::new(&rig.broker, TOKEN);
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/b", "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    let mut sock = sub_via_relay(&rig, "machine-a/b").await.expect("relay refused");
    tokio::time::sleep(Duration::from_millis(250)).await;

    blocking(move || c.send("machine-a/a", "machine-a/b", "fyi", "through the relay", "", "").unwrap()).await;

    let got = next_text(&mut sock, Duration::from_secs(5)).await.expect("nothing arrived");
    assert_eq!(got["subject"], "through the relay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_relay_multiplexes_several_addresses() {
    // The whole reason the relay is per-machine rather than per-session: one process, one port,
    // every session on the box.
    let rig = start().await;
    let c = Client::new(&rig.broker, TOKEN);
    let c2 = Client::new(&rig.broker, TOKEN);
    blocking(move || {
        c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/b", "s2", "machine-a", "r", "/x", 2).unwrap();
        c2.register("machine-a/c", "s3", "machine-a", "r", "/x", 3).unwrap();
    })
    .await;

    let mut sb = sub_via_relay(&rig, "machine-a/b").await.expect("b refused");
    let mut sc = sub_via_relay(&rig, "machine-a/c").await.expect("c refused");
    tokio::time::sleep(Duration::from_millis(250)).await;

    blocking(move || {
        c.send("machine-a/a", "machine-a/b", "fyi", "for-b", "", "").unwrap();
        c.send("machine-a/a", "machine-a/c", "fyi", "for-c", "", "").unwrap();
    })
    .await;

    assert_eq!(next_text(&mut sb, Duration::from_secs(5)).await.unwrap()["subject"], "for-b");
    assert_eq!(next_text(&mut sc, Duration::from_secs(5)).await.unwrap()["subject"], "for-c");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_refuses_a_second_subscriber_for_one_address() {
    // Mirrors the broker's rule. Two local subscribers on one address would each consume part of
    // the other's mail.
    let rig = start().await;
    let _first = sub_via_relay(&rig, "machine-a/a").await.expect("first refused");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(sub_via_relay(&rig, "machine-a/a").await.is_err(), "relay allowed a second subscriber");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_is_opened_only_while_a_local_subscriber_is_attached() {
    // This is what keeps the broker's queue semantics honest: no local subscriber means no socket
    // at the broker, which means the address is offline and mail queues rather than being pushed
    // into a void.
    let rig = start().await;
    let c = Client::new(&rig.broker, TOKEN);
    let c2 = Client::new(&rig.broker, TOKEN);
    blocking(move || c2.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap()).await;

    let before = blocking({
        let c3 = Client::new(&rig.broker, TOKEN);
        move || c3.peers(true).unwrap()
    })
    .await;
    assert!(before.live.is_empty(), "broker saw a live socket with no subscriber attached");

    let _sock = sub_via_relay(&rig, "machine-a/a").await.expect("relay refused");
    tokio::time::sleep(Duration::from_millis(400)).await;

    let after = blocking(move || c.peers(true).unwrap()).await;
    assert_eq!(after.live, vec!["machine-a/a".to_string()], "subscribing did not open upstream");
}

/// The second live session in one repo must land on the fallback name, not go deaf.
///
/// This is the whole mechanism behind repo-scoped addressing: the repo name is claimed
/// optimistically, and the collision is discovered ATOMICALLY as a 409 at claim time rather than
/// guessed in advance from a session id. The relay answers 409 out of its own local `busy` set
/// before any upstream connection, so the decision is machine-local and needs no broker change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_session_in_one_repo_binds_the_fallback_name() {
    let rig = start().await;
    let relay = format!("127.0.0.1:{}", rig.relay_port);

    // First session takes the repo address and holds it.
    let _held = sub_via_relay(&rig, "machine-a/thing").await.expect("first subscriber refused");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (_sock, bound) =
        agent_msg_bus::watch::bind(&relay, "machine-a/thing", Some("machine-a/thing.abc12345"))
            .await
            .expect("second session got no socket at all");
    assert_eq!(
        bound, "machine-a/thing.abc12345",
        "the second session did not fall back; it would be deaf"
    );
}

/// …and when the repo address is free, that is what gets bound. The fallback is an exception, not a
/// default: if it were taken every time, every session would be back to a session-scoped mailbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_repo_address_is_preferred_whenever_it_is_free() {
    let rig = start().await;
    let relay = format!("127.0.0.1:{}", rig.relay_port);

    let (_sock, bound) =
        agent_msg_bus::watch::bind(&relay, "machine-a/thing", Some("machine-a/thing.abc12345"))
            .await
            .expect("no socket");
    assert_eq!(bound, "machine-a/thing", "took the fallback while the repo address was free");
}

/// A PIN is an explicit declaration of identity, so it is offered no fallback — and a collision on
/// one must surface as a failure rather than silently answering to some other name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_address_with_no_fallback_offered_is_an_error_not_a_rename() {
    let rig = start().await;
    let relay = format!("127.0.0.1:{}", rig.relay_port);

    let _held = sub_via_relay(&rig, "machine-a/pinned").await.expect("first subscriber refused");
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        agent_msg_bus::watch::bind(&relay, "machine-a/pinned", None).await.is_err(),
        "a pinned address quietly bound something else"
    );
}
