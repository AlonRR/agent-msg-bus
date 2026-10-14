//! Machine-readable output for a program that watches a session's inbox rather than a person.
//!
//! A status-line notifier asks two things: which address is this session's, and how much is
//! waiting there. Both answers used to exist only as text written for a person to read, and a
//! program parsing that text breaks the first time a line is reworded. These run the real binary
//! against a real broker, because the output is the contract.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "json-output-test-token";
static N: AtomicU32 = AtomicU32::new(0);

/// A broker on a free port, and a scratch folder that stands in for the user's home.
struct Broker {
    base: String,
    home: PathBuf,
}

async fn broker() -> Broker {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-json-{}-{}", std::process::id(), n));
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

/// Runs the binary as a session would: its own home, its own working directory, and a session id.
async fn run(b: &Broker, cwd: &Path, args: &[&str]) -> Output {
    let (base, home, cwd) = (b.base.clone(), b.home.clone(), cwd.to_path_buf());
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"))
            .args(["--url", &base, "--token", TOKEN])
            .args(&args)
            .current_dir(cwd)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("CLAUDE_CODE_SESSION_ID", "0123abcd-0000-0000-0000-000000000000")
            .env_remove("AMB_URL")
            .env_remove("AMB_TOKEN")
            .env_remove("AMB_CA_FILE")
            .output()
            .expect("the binary runs")
    })
    .await
    .unwrap()
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// `machine-a/inbox` registered, with `count` messages waiting from someone else.
async fn inbox_with(b: &Broker, count: usize) {
    let c = Client::new(&b.base, TOKEN);
    blocking(move || {
        c.register("machine-a/inbox", "s1", "machine-a", "inbox", "/x", 1).unwrap();
        for i in 0..count {
            c.send("machine-a/peer", "machine-a/inbox", "fyi", &format!("subject {i}"), "body", "")
                .unwrap();
        }
    })
    .await;
}

fn json(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit {:?}: {stdout}{}", out.status, String::from_utf8_lossy(&out.stderr));
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not one JSON document ({e}): {stdout}"))
}

#[tokio::test]
async fn read_unread_json_gives_the_total_and_the_messages_oldest_first() {
    let b = broker().await;
    inbox_with(&b, 3).await;
    let v = json(&run(&b, &b.home, &["read", "machine-a/inbox", "--unread", "--json", "--limit", "2"]).await);
    assert_eq!(v["addr"], "machine-a/inbox");
    assert_eq!(v["total"], 3, "the total counts what was not shown too: {v}");
    let msgs = v["messages"].as_array().expect("messages is a list");
    assert_eq!(msgs.len(), 2, "{v}");
    assert_eq!(msgs[0]["subject"], "subject 0", "not oldest first: {v}");
    assert_eq!(msgs[0]["from"], "machine-a/peer");
    assert!(msgs[0]["id"].as_str().is_some_and(|s| !s.is_empty()), "no id to ack by: {v}");
}

#[tokio::test]
async fn read_unread_json_with_limit_zero_is_a_count_alone() {
    let b = broker().await;
    inbox_with(&b, 2).await;
    let v = json(&run(&b, &b.home, &["read", "machine-a/inbox", "--unread", "--json", "--limit", "0"]).await);
    assert_eq!(v["total"], 2, "{v}");
    assert_eq!(v["messages"].as_array().map(|a| a.len()), Some(0), "{v}");
}

#[tokio::test]
async fn read_unread_json_with_nothing_waiting_is_zero_not_an_error() {
    let b = broker().await;
    inbox_with(&b, 0).await;
    let v = json(&run(&b, &b.home, &["read", "machine-a/inbox", "--unread", "--json"]).await);
    assert_eq!(v["total"], 0, "{v}");
    assert_eq!(v["messages"].as_array().map(|a| a.len()), Some(0), "{v}");
}

/// `--limit 0` shows nothing, and the text form used to take "nothing shown" for "nothing waiting".
#[tokio::test]
async fn read_unread_text_with_limit_zero_still_says_how_many_are_waiting() {
    let b = broker().await;
    inbox_with(&b, 2).await;
    let out = run(&b, &b.home, &["read", "machine-a/inbox", "--unread", "--limit", "0"]).await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!stdout.contains("nothing unread"), "says nothing is waiting while 2 are: {stdout}");
    assert!(stdout.contains("2 unread"), "does not give the count: {stdout}");
}

#[tokio::test]
async fn whoami_json_names_the_address_to_read_and_whether_the_broker_knows_it() {
    let b = broker().await;
    let repo = b.home.join("thing");
    std::fs::create_dir_all(&repo).unwrap();
    // No relay listens on this port, so nothing is bound: the inbox is the derived address.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    std::fs::create_dir_all(b.home.join(".agent-msg-bus")).unwrap();
    std::fs::write(
        b.home.join(".agent-msg-bus").join("config.json"),
        serde_json::json!({"url": b.base, "machine": "machine-a", "token": TOKEN, "relay": closed.to_string()})
            .to_string(),
    )
    .unwrap();
    let addr = agent_msg_bus::hook::repo_address("machine-a", &repo.to_string_lossy());

    let before = json(&run(&b, &repo, &["whoami", "--json"]).await);
    assert_eq!(before["address"], addr.as_str(), "{before}");
    assert_eq!(before["inbox"], addr.as_str(), "with nothing bound, the inbox is the address: {before}");
    assert_eq!(before["pinned"], false);
    assert_eq!(before["bound"], serde_json::Value::Null, "{before}");
    assert_eq!(before["fallback"], format!("{addr}.0123abcd").as_str(), "{before}");
    assert_eq!(before["registered"], false, "{before}");

    let c = Client::new(&b.base, TOKEN);
    let a = addr.clone();
    blocking(move || c.register(&a, "s1", "machine-a", "thing", "/x", 1).unwrap()).await;
    let after = json(&run(&b, &repo, &["whoami", "--json"]).await);
    assert_eq!(after["registered"], true, "{after}");
}
