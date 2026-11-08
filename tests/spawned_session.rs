//! A session started by another session gets the mailbox its launcher named.
//!
//! Sessions in one repo derive one address, and with nothing subscribed no 409 ever separates them:
//! they share a mailbox and a cursor, so one session's `ack` empties it for the others. A launcher
//! closes that by setting `AMB_ADDRESS`, and the check that matters is at SessionStart: the child
//! must never be registered on the repo address at all, not even briefly. These run the real hook
//! against a real broker for that reason.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "spawned-session-test-token";
const SESSION: &str = "0123abcd-0000-0000-0000-000000000000";
const REPO_ADDR: &str = "machine-a/admin";
const CHILD: &str = "machine-a/admin.fixes.dns-cleanup";
static N: AtomicU32 = AtomicU32::new(0);

struct Rig {
    base: String,
    home: PathBuf,
    cwd: PathBuf,
}

/// A broker, a scratch home whose config points at it, and a working folder named `admin`.
///
/// The config's relay is the broker's own port: the hook only asks the relay for `/health`, and
/// the broker answers that too.
async fn rig() -> Rig {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-spawn-{}-{}", std::process::id(), n));
    let home = dir.join("home");
    let cwd = dir.join("admin");
    std::fs::create_dir_all(home.join(".agent-msg-bus")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
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
    let base = format!("http://127.0.0.1:{port}");
    let config = serde_json::json!({
        "url": base,
        "machine": "machine-a",
        "token": TOKEN,
        "relay": format!("127.0.0.1:{port}"),
    });
    std::fs::write(home.join(".agent-msg-bus").join("config.json"), config.to_string()).unwrap();
    Rig { base, home, cwd }
}

fn command(rig: &Rig, requested: Option<&str>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"));
    c.current_dir(&rig.cwd)
        .env("USERPROFILE", &rig.home)
        .env("HOME", &rig.home)
        .env("CLAUDE_CODE_SESSION_ID", SESSION)
        .env_remove("AMB_URL")
        .env_remove("AMB_TOKEN")
        .env_remove("AMB_CA_FILE")
        .env_remove("AMB_ADDRESS");
    if let Some(a) = requested {
        c.env("AMB_ADDRESS", a);
    }
    c
}

/// Runs the SessionStart hook and returns what it would tell the session.
async fn session_start(rig: &Rig, requested: Option<&str>) -> String {
    let mut c = command(rig, requested);
    let payload = serde_json::json!({"session_id": SESSION, "cwd": rig.cwd}).to_string();
    tokio::task::spawn_blocking(move || {
        let mut child = c
            .arg("session-start")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    })
    .await
    .unwrap()
}

async fn whoami(rig: &Rig) -> serde_json::Value {
    let mut c = command(rig, None);
    let base = rig.base.clone();
    tokio::task::spawn_blocking(move || {
        let out = c.args(["--url", &base, "--token", TOKEN, "whoami", "--json"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    })
    .await
    .unwrap()
}

async fn known(rig: &Rig) -> Vec<String> {
    let client = Client::new(&rig.base, TOKEN);
    tokio::task::spawn_blocking(move || {
        client.peers(true).unwrap().known.into_iter().map(|k| k.addr).collect()
    })
    .await
    .unwrap()
}

fn pin_file(rig: &Rig) -> PathBuf {
    rig.home.join(".agent-msg-bus").join("pins").join(SESSION)
}

/// The whole feature. Registered under the name it was given, and never under the shared one.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_child_registers_only_under_its_own_address() {
    let r = rig().await;
    let banner = session_start(&r, Some(CHILD)).await;
    let addrs = known(&r).await;
    assert!(addrs.iter().any(|a| a == CHILD), "not registered under its own name: {addrs:?}");
    assert!(
        !addrs.iter().any(|a| a == REPO_ADDR),
        "touched the shared repo address, so it was exposed to that mailbox: {addrs:?}"
    );
    assert!(banner.contains(CHILD), "the session was not told its address: {banner}");
}

/// The variable reaches only the hook. Later commands in the session may not carry it, so what the
/// hook decided has to outlive the hook.
#[tokio::test(flavor = "multi_thread")]
async fn the_named_address_outlives_the_hook_as_a_pin() {
    let r = rig().await;
    session_start(&r, Some(CHILD)).await;
    let me = whoami(&r).await;
    assert_eq!(me["inbox"], CHILD, "{me}");
    assert_eq!(me["pinned"], true, "{me}");
}

/// Opt-in: a session nobody named behaves exactly as before.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_variable_nothing_changes() {
    let r = rig().await;
    session_start(&r, None).await;
    let addrs = known(&r).await;
    assert!(addrs.iter().any(|a| a == REPO_ADDR), "{addrs:?}");
    assert!(!pin_file(&r).exists(), "pinned a session nobody named");
}

/// A bad name must be seen, and the session must still start. Silence here would leave the child on
/// the shared mailbox while its launcher believed otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_name_is_said_out_loud_and_not_pinned() {
    for bad in ["machine-a/admin.test", "machine-a/admin.*", "no-slash"] {
        let r = rig().await;
        let banner = session_start(&r, Some(bad)).await;
        assert!(banner.contains("AMB_ADDRESS"), "{bad} was refused silently: {banner}");
        assert!(banner.contains("REFUSED"), "{bad}: {banner}");
        assert!(!pin_file(&r).exists(), "{bad} was pinned anyway");
    }
}

/// A pin is a claim the session itself made, so it stands. A resumed session still carrying the
/// launcher's variable must not be renamed, and a mismatch is worth one line.
#[tokio::test(flavor = "multi_thread")]
async fn an_existing_pin_is_kept_and_the_mismatch_is_named() {
    let r = rig().await;
    std::fs::create_dir_all(pin_file(&r).parent().unwrap()).unwrap();
    std::fs::write(pin_file(&r), "machine-a/admin.review").unwrap();
    let banner = session_start(&r, Some(CHILD)).await;
    assert_eq!(std::fs::read_to_string(pin_file(&r)).unwrap(), "machine-a/admin.review");
    let addrs = known(&r).await;
    assert!(!addrs.iter().any(|a| a == CHILD), "{addrs:?}");
    assert!(banner.contains("AMB_ADDRESS"), "the mismatch went unsaid: {banner}");
}
