//! The waking-mail feed: `GET /pending/sub` and `watch --pending` (Phase 11 piece B).
//!
//! A launcher that starts ended sessions holds one socket per machine and is told, as it happens,
//! that mail needing a session arrived for one of the machine's addresses. It never reads the mail,
//! so the feed carries metadata only, and holding it must change nothing for the mailbox's owner.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use futures_util::StreamExt;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite;

const TOKEN: &str = "pending-feed-test-token"; // secret-gate: allow (a test fixture)
static N: AtomicU32 = AtomicU32::new(0);

struct Broker {
    base: String,
    home: std::path::PathBuf,
}

async fn broker() -> Broker {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-feed-{}-{}", std::process::id(), n));
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let tokens = dir.join("tokens.json");
    std::fs::write(&tokens, format!(r#"{{"machine-a":"{TOKEN}"}}"#)).unwrap();
    let state = AppState {
        store: Arc::new(Mutex::new(Store::open(dir.join("bus.db").to_str().unwrap()).unwrap())),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(Auth::from_file(tokens.to_str().unwrap()).unwrap()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
    Broker { base: format!("http://127.0.0.1:{port}"), home }
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

type Sock = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn feed(b: &Broker, query: &str) -> Sock {
    let host = b.base.strip_prefix("http://").unwrap();
    let url = format!("ws://{host}/pending/sub?token={TOKEN}{query}"); // secret-gate: allow (the test fixture)
    let (s, _) = tokio_tungstenite::connect_async(url).await.expect("the feed connects");
    s
}

async fn next_text(sock: &mut Sock, within: Duration) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => return Some(serde_json::from_str(&t).unwrap()),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// Registers the addresses on both machines that the tests send to, and returns the broker's URL.
async fn registered(b: &Broker) -> String {
    let base = b.base.clone();
    blocking(move || {
        let c = Client::new(&base, TOKEN);
        c.register("machine-a/one", "s-one", "machine-a", "one", "/w/one", 1).unwrap();
        c.register("machine-a/two", "s-two", "machine-a", "two", "/w/two", 1).unwrap();
        c.register("machine-b/far", "s-far", "machine-b", "far", "/w/far", 1).unwrap();
    })
    .await;
    b.base.clone()
}

async fn send(base: &str, from: &str, to: &str, kind: &str) -> String {
    let (base, from, to, kind) = (base.to_string(), from.to_string(), to.to_string(), kind.to_string());
    blocking(move || Client::new(&base, TOKEN).send(&from, &to, &kind, "the subject", "the body", "").unwrap()).await
}

const SOON: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(400);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waking_message_to_the_machine_is_one_line_of_metadata() {
    let b = broker().await;
    let c = registered(&b).await;
    let mut s = feed(&b, "").await;
    let id = send(&c, "machine-a/peer", "machine-a/one", "request").await;
    let v = next_text(&mut s, SOON).await.expect("no frame for a request");
    assert_eq!(v["address"], "machine-a/one", "{v}");
    assert_eq!(v["id"], id.as_str(), "{v}");
    assert_eq!(v["kind"], "request", "{v}");
    assert!(v["ts"].as_str().is_some_and(|t| !t.is_empty()), "{v}");
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "the feed carries more than metadata: {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_fyi_another_machine_and_a_self_send_give_no_line() {
    let b = broker().await;
    let c = registered(&b).await;
    let mut s = feed(&b, "").await;
    send(&c, "machine-a/peer", "machine-a/one", "fyi").await;
    send(&c, "machine-a/peer", "machine-b/far", "request").await;
    send(&c, "machine-a/one", "machine-a/one", "request").await;
    assert!(next_text(&mut s, QUIET).await.is_none(), "one of the three gave a line");
    // A later request still comes through: the three were skipped, not held up.
    send(&c, "machine-a/peer", "machine-a/two", "blocking").await;
    let v = next_text(&mut s, SOON).await.expect("no frame after the skipped ones");
    assert_eq!(v["address"], "machine-a/two", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_pattern_is_a_line_per_matching_address_but_the_sender() {
    let b = broker().await;
    let c = registered(&b).await;
    let mut s = feed(&b, "").await;
    send(&c, "machine-a/one", "machine-a/*", "request").await;
    let v = next_text(&mut s, SOON).await.expect("no frame for the pattern");
    assert_eq!(v["address"], "machine-a/two", "{v}");
    assert!(next_text(&mut s, QUIET).await.is_none(), "the sender was told of its own message");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_machine_and_other_kinds_can_be_asked_for() {
    let b = broker().await;
    let c = registered(&b).await;
    let mut s = feed(&b, "&machine=machine-b&kinds=fyi").await;
    send(&c, "machine-a/peer", "machine-a/one", "fyi").await;
    send(&c, "machine-a/peer", "machine-b/far", "request").await;
    send(&c, "machine-a/peer", "machine-b/far", "fyi").await;
    let v = next_text(&mut s, SOON).await.expect("no frame for machine-b's fyi");
    assert_eq!((v["address"].as_str(), v["kind"].as_str()), (Some("machine-b/far"), Some("fyi")), "{v}");
    assert!(next_text(&mut s, QUIET).await.is_none());
}

/// The owner of the mailbox must not notice the feed at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn holding_the_feed_claims_nothing_and_consumes_nothing() {
    let b = broker().await;
    let c = registered(&b).await;
    let mut s = feed(&b, "").await;
    send(&c, "machine-a/peer", "machine-a/one", "request").await;
    next_text(&mut s, SOON).await.expect("no frame");

    let (peers, unread) = blocking(move || {
        let c = Client::new(&c, TOKEN);
        (c.peers(true).unwrap(), c.read_unread("machine-a/one", 20).unwrap())
    })
    .await;
    assert!(peers.known.iter().all(|k| !k.live), "the feed made an address look live");
    assert_eq!(unread.1, 1, "the feed consumed the message");

    let host = b.base.strip_prefix("http://").unwrap();
    let owner = tokio_tungstenite::connect_async(format!("ws://{host}/sub?addr=machine-a%2Fone&token={TOKEN}")).await; // secret-gate: allow (the test fixture)
    assert!(owner.is_ok(), "the owner could not subscribe while the feed was held");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_token_gets_no_feed() {
    let b = broker().await;
    let host = b.base.strip_prefix("http://").unwrap();
    let res = tokio_tungstenite::connect_async(format!("ws://{host}/pending/sub?token=not-it")).await; // secret-gate: allow (a deliberately wrong test token)
    assert!(res.is_err(), "the feed accepted a bad token");
}

/// `watch --pending` as a launcher runs it: a `connected` line, then the broker's lines as they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_pending_prints_connected_then_one_line_per_message() {
    let b = broker().await;
    let c = registered(&b).await;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"))
        .args(["--url", &b.base, "--token", TOKEN, "watch", "--pending"])
        .env("USERPROFILE", &b.home)
        .env("HOME", &b.home)
        .env_remove("AMB_URL")
        .env_remove("AMB_TOKEN")
        .env_remove("AMB_CA_FILE")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let first = rx.recv_timeout(Duration::from_secs(10)).expect("no first line");
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["_watch"], "connected", "{first}");

    let id = send(&c, "machine-a/peer", "machine-a/two", "job").await;
    let line = rx.recv_timeout(Duration::from_secs(10)).expect("no line for the message");
    let _ = child.kill();
    let _ = child.wait();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["address"], "machine-a/two", "{v}");
    assert_eq!(v["id"], id.as_str(), "{v}");
}

/// A broker without the route answers the handshake 404: said as the version needed, and not
/// retried for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_pending_against_a_broker_without_the_route_names_the_version() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new()).await.unwrap();
    });
    let home = std::env::temp_dir().join(format!("amb-feed-old-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"))
            .args(["--url", &format!("http://127.0.0.1:{port}"), "--token", TOKEN, "watch", "--pending"])
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env_remove("AMB_URL")
            .env_remove("AMB_TOKEN")
            .env_remove("AMB_CA_FILE")
            .output()
            .unwrap()
    })
    .await;
    assert!(!out.status.success(), "it went on as if the broker had the route");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("0.4.35"), "does not name the version needed: {err}");
}
