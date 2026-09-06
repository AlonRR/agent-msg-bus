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
    state: AppState,
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
    let handle_state = state.clone();
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });

    Harness { base: format!("http://127.0.0.1:{port}"), state: handle_state, _dir: dir }
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

/// Reported by machine-a/machine-a.fixes: it was pinned, registered, subscribed and demonstrably receiving,
/// and `peers` did not list it.
///
/// Cause: `promote` is an UPDATE, so subscribing *before* registering promotes nothing, and the
/// `register` that follows inserts the address as provisional. It stays live and invisible.
/// The real error was deriving membership from a stored flag when the hub already knows the truth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_address_that_subscribes_before_registering_still_appears_in_peers() {
    let h = start().await;

    // Subscribe first — nothing is registered yet, so promote has nothing to update.
    let _sock = connect(&h, "machine-a/early").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Register afterwards, which inserts it as provisional.
    let c = h.client();
    blocking(move || c.register("machine-a/early", "s1", "machine-a", "r", "/x", 1).unwrap()).await;

    let c2 = h.client();
    let p = blocking(move || c2.peers(false).unwrap()).await;
    assert!(
        p.known.iter().any(|k| k.addr == "machine-a/early"),
        "an address with a live socket was hidden from peers: {:?}",
        p.known.iter().map(|k| &k.addr).collect::<Vec<_>>()
    );
    assert!(p.live.contains(&"machine-a/early".to_string()));
}

/// `pushed_to: 0` must say WHY. Self-send is what anyone reaches for first to test their own inbox,
/// and it returns the exact signature of a dead subscription — machine-b nearly reported it as a
/// regression while every other indicator said healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_that_pushed_to_nobody_explains_why() {
    let h = start().await;
    let c = h.client();
    blocking(move || c.register("machine-a/solo", "s1", "machine-a", "r", "/x", 1).unwrap()).await;

    let _sock = connect(&h, "machine-a/solo").await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Self-send: live subscriber, but a sender is never echoed its own message.
    let base = h.base.clone();
    let selfsend: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/send"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({
                "from": "machine-a/solo", "to": "machine-a/solo", "subject": "probe", "body": ""
            }))
            .unwrap()
            .into_json()
            .unwrap()
    })
    .await;
    assert_eq!(selfsend["pushed_to"], 0);
    assert_eq!(selfsend["self_addressed"], true);
    assert!(
        selfsend["note"].as_str().unwrap_or("").contains("never sent its own message"),
        "self-send gave no explanation: {selfsend:?}"
    );

    // Known address, not currently subscribed: zero pushed, but it really will be delivered.
    let c2 = h.client();
    blocking(move || c2.register("machine-a/known-away", "s2", "machine-a", "r", "/x", 2).unwrap()).await;
    let base = h.base.clone();
    let offline: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/send"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({
                "from": "machine-a/solo", "to": "machine-a/known-away", "subject": "probe", "body": ""
            }))
            .unwrap()
            .into_json()
            .unwrap()
    })
    .await;
    assert_eq!(offline["pushed_to"], 0);
    assert_eq!(offline["recipient_known"], true);
    assert!(offline["note"].as_str().unwrap_or("").contains("delivered on connect"));

    // Nonexistent address: also zero, and it must NOT be reassuring. Nothing will ever collect it.
    let base = h.base.clone();
    let typo: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/send"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({
                "from": "machine-a/solo", "to": "machine-a/typoed-name", "subject": "probe", "body": ""
            }))
            .unwrap()
            .into_json()
            .unwrap()
    })
    .await;
    assert_eq!(typo["pushed_to"], 0);
    assert_eq!(typo["recipient_known"], false);
    let note = typo["note"].as_str().unwrap_or("");
    assert!(note.contains("WARNING"), "a typo was not warned about: {note}");
    assert!(
        !note.contains("will be delivered"),
        "a typo was told its message would be delivered: {note}"
    );
}

/// `provisional_hours` must be a behaviour, not an intention. It used to be reachable only through
/// `/prune`, a command nobody ran, so entries advertised as "expiring in hours" were still
/// registered 35 hours later. Same shape as `RestartCount=999` never firing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sweeper_forgets_aged_provisional_entries_but_never_a_live_one() {
    let h = start().await;
    let c = h.client();
    blocking(move || {
        c.register("machine-a/aged", "s1", "machine-a", "r", "/x", 1).unwrap();
        c.register("machine-a/aged-but-live", "s2", "machine-a", "r", "/x", 2).unwrap();
    })
    .await;

    // Hold a socket on one of them. A live socket must outrank any clock.
    let _sock = connect(&h, "machine-a/aged-but-live").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // -1h cutoff => everything registered "now" is already past it.
    let removed = agent_msg_bus::server::sweep_provisional(&h.state, -1);

    assert!(removed.contains(&"machine-a/aged".to_string()), "aged entry survived: {removed:?}");
    assert!(
        !removed.contains(&"machine-a/aged-but-live".to_string()),
        "the sweeper removed an address with a live socket"
    );

    let c2 = h.client();
    let p = blocking(move || c2.peers(true).unwrap()).await;
    assert!(!p.known.iter().any(|k| k.addr == "machine-a/aged"), "swept entry still in the registry");
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

/// Mail addressed to a migrated-FROM name must be PUSHED to the successor's live socket, not left
/// to surface whenever that session next happens to reconnect.
///
/// The hub matched `msg.to` against the address a socket holds, and nothing resolved the alias — so
/// a live successor was never pushed to. The message did arrive eventually, on the next reconnect
/// replay, which is why this reads as a merely cosmetic status-line bug from the sending side. It
/// is not: waking an idle session is the entire reason this bus exists, and for every migrated
/// address that guarantee had quietly degraded to polling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_a_migrated_from_name_is_pushed_to_the_live_successor() {
    let h = start().await;

    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/sender", "s0", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/old", "s1", "machine-a", "r", "/x", 2).unwrap();
        c2.register("machine-a/new", "s2", "machine-a", "r", "/x", 3).unwrap();
        c2.migrate("machine-a/old", "machine-a/new").unwrap();
    })
    .await;

    let mut sock = connect(&h, "machine-a/new").await;
    tokio::time::sleep(Duration::from_millis(100)).await; // let the claim land before sending

    let c = h.client();
    let out = blocking(move || {
        c.send("machine-a/sender", "machine-a/old", "fyi", "to the old name", "body", "").unwrap()
    })
    .await;

    let got = next_text(&mut sock, Duration::from_secs(5))
        .await
        .expect("nothing was pushed to the live successor");
    assert_eq!(got["subject"], "to the old name");
    assert!(!out.is_empty());
}

/// ...and the sender is told so. `pushed_to: 0` plus "it will be delivered on connect" is the
/// opposite of the truth when the alias target is subscribed, and that sentence is exactly what a
/// caller reads to decide whether a message landed or is sitting in a queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sender_is_told_a_message_to_an_aliased_name_was_pushed() {
    let h = start().await;

    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/sender", "s0", "machine-a", "r", "/x", 1).unwrap();
        c2.register("machine-a/old", "s1", "machine-a", "r", "/x", 2).unwrap();
        c2.register("machine-a/new", "s2", "machine-a", "r", "/x", 3).unwrap();
        c2.migrate("machine-a/old", "machine-a/new").unwrap();
    })
    .await;

    let _sock = connect(&h, "machine-a/new").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let base = h.base.clone();
    let body = blocking(move || {
        let raw: serde_json::Value = ureq::post(&format!("{base}/send"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({
                "from": "machine-a/sender",
                "to": "machine-a/old",
                "kind": "fyi",
                "subject": "to the old name",
                "body": "b",
                "reply_to": ""
            }))
            .unwrap()
            .into_json()
            .unwrap();
        raw
    })
    .await;

    assert_eq!(
        body["pushed_to"].as_u64().unwrap(),
        1,
        "the live successor was not counted as a push target: {body}"
    );
    assert!(
        body["note"].is_null(),
        "the sender was told the message is queued while it was in fact pushed: {body}"
    );
}

/// A migrated successor must appear in `peers` as the live participant, with the old name shown as
/// one of its aliases — and the old name must not also appear as its own, permanently offline row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_shows_the_successor_live_and_the_predecessor_only_as_an_alias() {
    let h = start().await;

    let c2 = h.client();
    blocking(move || {
        c2.register("machine-a/old", "s1", "machine-a", "r", "/x", 2).unwrap();
        c2.register("machine-a/new", "s2", "machine-a", "r", "/x", 3).unwrap();
        c2.migrate("machine-a/old", "machine-a/new").unwrap();
    })
    .await;

    let _sock = connect(&h, "machine-a/new").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let c = h.client();
    let p = blocking(move || c.peers(true).unwrap()).await;
    let addrs: Vec<String> = p.known.iter().map(|k| k.addr.clone()).collect();
    assert_eq!(addrs, vec!["machine-a/new".to_string()], "unexpected roster: {addrs:?}");

    let new = &p.known[0];
    assert!(new.live, "the successor holds a socket but is not reported live");
    assert_eq!(new.aliases, vec!["machine-a/old".to_string()]);
}

/// Subscribing an address nobody registered must not hand it the whole history.
///
/// `/sub` only ever called `promote`, an UPDATE that does nothing without a row — so such an address
/// had no registry row and no CURSOR, and a missing cursor reads as the empty string, which sorts
/// before every id. Every message it matched replayed on connect. Rare while every address came
/// from the SessionStart hook; the common path as soon as a client can bind a fallback name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribing_an_unregistered_address_does_not_replay_the_whole_history() {
    let h = start().await;

    let c = h.client();
    blocking(move || {
        c.register("machine-a/sender", "s0", "machine-a", "r", "/x", 1).unwrap();
        for i in 0..3 {
            c.send("machine-a/sender", "machine-a/*", "fyi", &format!("old {i}"), "b", "").unwrap();
        }
    })
    .await;

    // Never registered: straight to the socket, the way a fallback name arrives.
    let mut sock = connect(&h, "machine-a/never-registered").await;
    assert!(
        next_text(&mut sock, Duration::from_millis(700)).await.is_none(),
        "a brand-new address was replayed history it was never sent"
    );

    // …and the socket alone was enough to put it on the roster.
    let c2 = h.client();
    let p = blocking(move || c2.peers(true).unwrap()).await;
    let me = p
        .known
        .iter()
        .find(|k| k.addr == "machine-a/never-registered")
        .expect("subscribing did not create a registry row");
    assert!(me.live, "it holds a socket but is not reported live");

    // A message sent AFTER it subscribed must still arrive - the cursor starts at the head, it is
    // not disabled.
    let c3 = h.client();
    blocking(move || {
        c3.send("machine-a/sender", "machine-a/never-registered", "fyi", "new", "b", "").unwrap()
    })
    .await;
    let got = next_text(&mut sock, Duration::from_secs(5)).await.expect("no frame after subscribe");
    assert_eq!(got["subject"], "new");
}

/// A version has to survive the whole round trip, or "which build is that session on?" stays
/// unanswerable without hashing files on each machine — which is what it cost before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_address_reports_the_build_it_registered_with() {
    let h = start().await;
    let c = h.client();
    blocking(move || c.register("machine-a/a", "s1", "machine-a", "r", "/x", 1).unwrap()).await;

    let c2 = h.client();
    let p = blocking(move || c2.peers(true).unwrap()).await;
    let me = p.known.iter().find(|k| k.addr == "machine-a/a").expect("no row");
    assert_eq!(
        me.version,
        agent_msg_bus::VERSION,
        "the client's own build did not reach the registry"
    );
}

/// The broker answers with its build on the one endpoint every client can already reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_broker_reports_its_version_on_health() {
    let h = start().await;
    let c = h.client();
    let v = blocking(move || c.broker_version()).await;
    assert_eq!(v.as_deref(), Some(agent_msg_bus::VERSION), "broker /health carries no version");
}

/// An address registered before versions existed must be distinguishable from one reporting a real
/// version — not silently rendered as though it were current.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_address_that_never_reported_a_version_reads_as_unknown() {
    let h = start().await;
    {
        let store = h.state.store.lock().unwrap();
        store.ensure_registered("machine-a/ancient", "machine-a").unwrap();
    }
    let c = h.client();
    let p = blocking(move || c.peers(true).unwrap()).await;
    let old = p.known.iter().find(|k| k.addr == "machine-a/ancient").expect("no row");
    assert!(old.version.is_empty(), "an unknown version was invented rather than left blank");
}

/// The cursor guard is a deliberate refusal, not a crash. It was surfaced as HTTP 500 with
/// rusqlite's "Invalid parameter name:" glued to the front — so the one caller who most needs to
/// understand it (whoever just passed prose to `ack`) is told the broker broke, and may retry or
/// escalate instead of fixing the call. A refused ack is the correct answer to a bad request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_ack_is_a_client_error_not_a_server_fault() {
    let h = start().await;
    let c = h.client();
    blocking(move || c.register("machine-a/guarded", "s1", "machine-a", "r", "/x", 1).unwrap()).await;

    let base = h.base.clone();
    let (status, body): (u16, serde_json::Value) = blocking(move || {
        match ureq::post(&format!("{base}/ack"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({
                "addr": "machine-a/guarded", "up_to_id": "yes I have read it"
            })) {
            Ok(r) => (r.status(), r.into_json().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_json().unwrap()),
            Err(e) => panic!("transport error, not an HTTP status: {e}"),
        }
    })
    .await;

    assert_eq!(status, 400, "a refused ack must be a client error, not a server fault");
    let msg = body["error"].as_str().unwrap_or("");
    assert!(
        !msg.contains("Invalid parameter name"),
        "the refusal leaked a rusqlite variant name and reads like an internal fault: {msg}"
    );
    assert!(msg.contains("is not a message id"), "the refusal did not explain itself: {msg}");
}
