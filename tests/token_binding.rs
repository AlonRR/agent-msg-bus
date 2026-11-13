//! A token acts only for its own machine's addresses.
//!
//! `tokens.json` names a machine for every token, and every address starts with a machine, but no
//! route compared the two: any machine's token could send under another session's name, ack (and
//! so empty) its mailbox, register, migrate or forget it, or subscribe to its live mail. These run a
//! real broker with two machines' tokens and try each route across the line.
//!
//! In `warn` mode, the default, nothing is refused: the request does what it always did, and the
//! reply carries `binding_warning` so the sender sees it. In `enforce` mode it is a 403 naming both
//! machines, and nothing happens. Reads stay unbound: a machine may read any mailbox, as before.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Binding, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN_A: &str = "binding-test-token-a"; // secret-gate: allow
const TOKEN_B: &str = "binding-test-token-b"; // secret-gate: allow
static N: AtomicU32 = AtomicU32::new(0);

struct Rig {
    base: String,
}

fn tokens_file() -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-bind-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    let tokens = dir.join("tokens.json");
    std::fs::write(&tokens, format!(r#"{{"machine-a":"{TOKEN_A}","machine-b":"{TOKEN_B}"}}"#))
        .unwrap();
    tokens
}

async fn serve(auth: Auth, db: std::path::PathBuf) -> Rig {
    let state = AppState {
        store: Arc::new(Mutex::new(Store::open(db.to_str().unwrap()).unwrap())),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(auth),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
    Rig { base: format!("http://127.0.0.1:{port}") }
}

/// A broker in `mode`, or in whatever `Auth::from_file` defaults to when `mode` is `None`.
async fn rig(mode: Option<Binding>) -> Rig {
    let tokens = tokens_file();
    let mut auth = Auth::from_file(tokens.to_str().unwrap()).unwrap();
    if let Some(m) = mode {
        auth = auth.with_binding(m);
    }
    serve(auth, tokens.with_file_name("bus.db")).await
}

async fn blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// The raw reply, status and body, so a test sees exactly what a client would.
async fn post(r: &Rig, token: &str, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let url = format!("{}{}", r.base, path);
    let auth = format!("Bearer {token}");
    blocking(move || {
        let resp = ureq::post(&url).set("Authorization", &auth).send_json(body);
        let (code, text) = match resp {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap()),
            Err(e) => panic!("transport error: {e}"),
        };
        (code, serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
    })
    .await
}

fn send_body(from: &str, to: &str) -> serde_json::Value {
    serde_json::json!({"from": from, "to": to, "kind": "fyi", "subject": "s", "body": "b"})
}

async fn register(r: &Rig, token: &str, addr: &str) {
    let (c, v) = post(r, token, "/register", serde_json::json!({"addr": addr})).await;
    assert_eq!(c, 200, "{v}");
}

async fn known(r: &Rig) -> Vec<String> {
    let c = Client::new(&r.base, TOKEN_A);
    blocking(move || c.peers(true).unwrap().known.into_iter().map(|k| k.addr).collect()).await
}

async fn unread(r: &Rig, addr: &str) -> usize {
    let c = Client::new(&r.base, TOKEN_A);
    let addr = addr.to_string();
    blocking(move || c.read_unread(&addr, 100).unwrap().0.len()).await
}

fn names_both(v: &serde_json::Value, field: &str) -> bool {
    let s = v[field].as_str().unwrap_or_default();
    s.contains("machine-a") && s.contains("machine-b")
}

// ---- warn, the default -------------------------------------------------------------------------

/// Today's deployed clients must keep working the day this broker goes in, so the default refuses
/// nothing. Pinned on `from_file` itself, not on an explicit mode.
#[tokio::test(flavor = "multi_thread")]
async fn by_default_a_cross_machine_send_goes_through_and_says_so() {
    let r = rig(None).await;
    register(&r, TOKEN_B, "machine-b/inbox").await;
    let (c, v) = post(&r, TOKEN_A, "/send", send_body("machine-b/impostor", "machine-b/inbox")).await;
    assert_eq!(c, 200, "the default refused a send: {v}");
    assert!(names_both(&v, "binding_warning"), "the sender was not told: {v}");
    assert_eq!(unread(&r, "machine-b/inbox").await, 1, "the message was not stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_under_your_own_machine_carries_no_warning() {
    let r = rig(None).await;
    register(&r, TOKEN_B, "machine-b/inbox").await;
    let (c, v) = post(&r, TOKEN_A, "/send", send_body("machine-a/me", "machine-b/inbox")).await;
    assert_eq!(c, 200, "{v}");
    assert!(v.get("binding_warning").is_none(), "warned about a legitimate send: {v}");
}

/// Warn changes nothing but the reply: the ack still moves the other machine's cursor.
#[tokio::test(flavor = "multi_thread")]
async fn in_warn_mode_a_cross_machine_ack_still_acks_and_says_so() {
    let r = rig(Some(Binding::Warn)).await;
    register(&r, TOKEN_B, "machine-b/inbox").await;
    let (_, sent) = post(&r, TOKEN_B, "/send", send_body("machine-b/peer", "machine-b/inbox")).await;
    let id = sent["id"].as_str().unwrap().to_string();
    let (c, v) =
        post(&r, TOKEN_A, "/ack", serde_json::json!({"addr": "machine-b/inbox", "up_to_id": id}))
            .await;
    assert_eq!(c, 200, "{v}");
    assert!(names_both(&v, "binding_warning"), "{v}");
    assert_eq!(unread(&r, "machine-b/inbox").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn in_warn_mode_a_cross_machine_subscribe_still_connects() {
    let r = rig(Some(Binding::Warn)).await;
    let url = format!(
        "{}/sub?addr=machine-b%2Finbox&token={TOKEN_A}", // secret-gate: allow
        r.base.replace("http://", "ws://")
    );
    assert!(tokio_tungstenite::connect_async(url).await.is_ok(), "warn refused a subscribe");
}

// ---- enforce -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn enforced_a_send_under_another_machines_name_is_refused_and_not_stored() {
    let r = rig(Some(Binding::Enforce)).await;
    register(&r, TOKEN_B, "machine-b/inbox").await;
    let (c, v) = post(&r, TOKEN_A, "/send", send_body("machine-b/impostor", "machine-b/inbox")).await;
    assert_eq!(c, 403, "{v}");
    assert!(names_both(&v, "error"), "the refusal does not say whose token it was: {v}");
    assert_eq!(unread(&r, "machine-b/inbox").await, 0, "a refused send was stored");
}

/// Sending TO another machine is the point of a bus, and stays open.
#[tokio::test(flavor = "multi_thread")]
async fn enforced_a_send_to_another_machine_is_still_allowed() {
    let r = rig(Some(Binding::Enforce)).await;
    register(&r, TOKEN_B, "machine-b/inbox").await;
    let (c, v) = post(&r, TOKEN_A, "/send", send_body("machine-a/me", "machine-b/inbox")).await;
    assert_eq!(c, 200, "{v}");
    assert!(v.get("binding_warning").is_none(), "{v}");
    assert_eq!(unread(&r, "machine-b/inbox").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn enforced_registering_another_machines_address_is_refused() {
    let r = rig(Some(Binding::Enforce)).await;
    let (c, v) = post(&r, TOKEN_A, "/register", serde_json::json!({"addr": "machine-b/x"})).await;
    assert_eq!(c, 403, "{v}");
    assert!(!known(&r).await.iter().any(|a| a == "machine-b/x"), "registered anyway");
}

/// The one that empties someone else's mailbox.
#[tokio::test(flavor = "multi_thread")]
async fn enforced_acking_another_machines_mailbox_is_refused_and_the_mail_stays() {
    let r = rig(Some(Binding::Enforce)).await;
    register(&r, TOKEN_B, "machine-b/inbox").await;
    let (_, sent) = post(&r, TOKEN_B, "/send", send_body("machine-b/peer", "machine-b/inbox")).await;
    let id = sent["id"].as_str().unwrap().to_string();
    let (c, v) =
        post(&r, TOKEN_A, "/ack", serde_json::json!({"addr": "machine-b/inbox", "up_to_id": id}))
            .await;
    assert_eq!(c, 403, "{v}");
    assert_eq!(unread(&r, "machine-b/inbox").await, 1, "the mail was acked away anyway");
}

/// Both ends: migrating out of another machine's mailbox takes its mail, and migrating your own
/// into another machine's name plants a mailbox and cursor there.
#[tokio::test(flavor = "multi_thread")]
async fn enforced_a_migrate_across_machines_is_refused_in_either_direction() {
    let r = rig(Some(Binding::Enforce)).await;
    register(&r, TOKEN_A, "machine-a/old").await;
    register(&r, TOKEN_B, "machine-b/old").await;
    for (from, to) in [("machine-b/old", "machine-a/new"), ("machine-a/old", "machine-b/new")] {
        let (c, v) = post(&r, TOKEN_A, "/migrate", serde_json::json!({"from": from, "to": to})).await;
        assert_eq!(c, 403, "{from} -> {to}: {v}");
    }
    let (c, v) = post(
        &r,
        TOKEN_A,
        "/migrate",
        serde_json::json!({"from": "machine-a/old", "to": "machine-a/new"}),
    )
    .await;
    assert_eq!(c, 200, "a migrate within the token's own machine was refused: {v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn enforced_forgetting_another_machines_address_is_refused() {
    let r = rig(Some(Binding::Enforce)).await;
    register(&r, TOKEN_B, "machine-b/x").await;
    let (c, v) = post(&r, TOKEN_A, "/forget", serde_json::json!({"addr": "machine-b/x"})).await;
    assert_eq!(c, 403, "{v}");
    assert!(known(&r).await.iter().any(|a| a == "machine-b/x"), "forgotten anyway");
}

#[tokio::test(flavor = "multi_thread")]
async fn enforced_deleting_another_machines_orphan_is_refused() {
    let r = rig(Some(Binding::Enforce)).await;
    let (_, sent) =
        post(&r, TOKEN_B, "/send", send_body("machine-b/peer", "machine-b/nobody")).await;
    let id = sent["id"].as_str().unwrap().to_string();
    let (c, v) = post(&r, TOKEN_A, "/orphans/delete", serde_json::json!({"id": id})).await;
    assert_eq!(c, 403, "{v}");
    let (c, v) = post(&r, TOKEN_B, "/orphans/delete", serde_json::json!({"id": id})).await;
    assert_eq!(c, 200, "its own machine could not delete it: {v}");
}

/// A subscriber is pushed the mailbox's live mail, so this is the widest door of all.
#[tokio::test(flavor = "multi_thread")]
async fn enforced_subscribing_to_another_machines_address_is_refused() {
    let r = rig(Some(Binding::Enforce)).await;
    let ws = r.base.replace("http://", "ws://");
    let theirs = format!("{ws}/sub?addr=machine-b%2Finbox&token={TOKEN_A}"); // secret-gate: allow
    assert!(tokio_tungstenite::connect_async(theirs).await.is_err(), "subscribed anyway");
    let mine = format!("{ws}/sub?addr=machine-a%2Finbox&token={TOKEN_A}"); // secret-gate: allow
    assert!(tokio_tungstenite::connect_async(mine).await.is_ok(), "its own address was refused");
}

// ---- no auth -----------------------------------------------------------------------------------

/// `--insecure-no-auth` has no tokens, so there is no machine to bind to and nothing is checked.
#[tokio::test(flavor = "multi_thread")]
async fn with_auth_disabled_nothing_is_bound() {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-bind-open-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    let r = serve(Auth::disabled().with_binding(Binding::Enforce), dir.join("bus.db")).await;
    let (c, v) = post(&r, "anything", "/send", send_body("machine-b/x", "machine-a/y")).await;
    assert_eq!(c, 200, "{v}");
    assert!(v.get("binding_warning").is_none(), "{v}");
}
