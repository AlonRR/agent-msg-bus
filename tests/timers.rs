//! Timers (Phase 11 piece C): a message the broker holds until a time, then delivers like any
//! other, and `remind`, a session's message to itself.
//!
//! The deadlines here are generous and the tests never assert how soon a timer fired, only that a
//! far-future one has not and a near one has: a broker upgrade waits for every test passing, and a
//! timing-tight test on a loaded build machine would block it for nothing.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, fire_timers, start_timers, AppState};
use agent_msg_bus::store::Store;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite;

const TOKEN: &str = "timers-test-token"; // secret-gate: allow (a test fixture)
static N: AtomicU32 = AtomicU32::new(0);

struct Rig {
    base: String,
    dir: PathBuf,
    state: AppState,
}

fn scratch() -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-timers-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::write(dir.join("tokens.json"), format!(r#"{{"machine-a":"{TOKEN}"}}"#)).unwrap();
    dir
}

/// A broker, with the timer task running unless `timers` is false.
async fn rig(timers: bool) -> Rig {
    let dir = scratch();
    let state = AppState {
        store: Arc::new(Mutex::new(Store::open(dir.join("bus.db").to_str().unwrap()).unwrap())),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(Auth::from_file(dir.join("tokens.json").to_str().unwrap()).unwrap()),
    };
    if timers {
        start_timers(state.clone());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = state.clone();
    tokio::spawn(async move {
        axum::serve(listener, app(s)).await.unwrap();
    });
    Rig { base: format!("http://127.0.0.1:{port}"), dir, state }
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn client(r: &Rig) -> Client {
    Client::new(&r.base, TOKEN)
}

async fn register(r: &Rig, addr: &'static str) {
    let c = client(r);
    blocking(move || c.register(addr, "s", "machine-a", "r", "/w", 1).unwrap()).await;
}

/// Unread mail for `addr`, waiting up to `within` for at least `want` messages.
async fn unread_within(r: &Rig, addr: &'static str, want: usize, within: Duration) -> Vec<agent_msg_bus::store::Message> {
    let c = client(r);
    blocking(move || {
        let until = std::time::Instant::now() + within;
        loop {
            let (got, _) = c.read_unread(addr, 100).unwrap();
            if got.len() >= want || std::time::Instant::now() >= until {
                return got;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })
    .await
}

fn cli(r: &Rig, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"))
        .args(["--url", &r.base, "--token", TOKEN])
        .args(args)
        .env("USERPROFILE", r.dir.join("home"))
        .env("HOME", r.dir.join("home"))
        .env_remove("AMB_URL")
        .env_remove("AMB_TOKEN")
        .env_remove("AMB_CA_FILE")
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reminder_set_with_the_cli_arrives_in_its_own_unread_mail() {
    let r = rig(true).await;
    register(&r, "machine-a/me").await;
    let rr = Rig { base: r.base.clone(), dir: r.dir.clone(), state: r.state.clone() };
    let out = blocking(move || cli(&rr, &["remind", "machine-a/me", "--in", "1s", "--subject", "check the scraper"])).await;
    assert!(out.status.success(), "{out:?}");
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(id.starts_with('t'), "prints the timer id: {id}");
    let got = unread_within(&r, "machine-a/me", 1, Duration::from_secs(20)).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].kind.as_str(), got[0].from.as_str()), ("timer", "machine-a/me"));
    assert_eq!(got[0].subject, "check the scraper");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_far_future_timer_is_not_mail_and_a_due_one_is() {
    let r = rig(false).await;
    register(&r, "machine-a/b").await;
    let c = client(&r);
    blocking(move || {
        c.set_timer("machine-a/a", "machine-a/b", "request", "tomorrow", "", "", None, Some(86_400)).unwrap();
        c.set_timer("machine-a/a", "machine-a/b", "request", "now", "", "", None, Some(0)).unwrap();
    })
    .await;
    assert_eq!(fire_timers(&r.state, time::OffsetDateTime::now_utc()), 1);
    let got = unread_within(&r, "machine-a/b", 1, Duration::from_secs(5)).await;
    assert_eq!(got.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(), ["now"]);
}

type Sock = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(sock: &mut Sock, within: Duration) -> Option<serde_json::Value> {
    let until = tokio::time::Instant::now() + within;
    loop {
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                // Status frames from the broker are not messages.
                if v.get("id").is_some() {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_subscriber_is_pushed_its_own_reminder_when_due() {
    let r = rig(false).await;
    register(&r, "machine-a/me").await;
    let host = r.base.strip_prefix("http://").unwrap().to_string();
    let url = format!("ws://{host}/sub?addr=machine-a/me&token={TOKEN}"); // secret-gate: allow (the test fixture)
    let (mut sock, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let c = client(&r);
    blocking(move || c.set_timer("machine-a/me", "machine-a/me", "timer", "ping me", "", "", None, Some(0)).unwrap()).await;
    fire_timers(&r.state, time::OffsetDateTime::now_utc());
    let frame = next_json(&mut sock, Duration::from_secs(10)).await.expect("pushed to its own live socket");
    assert_eq!(frame["subject"], "ping me");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_due_reminder_counts_as_pending_and_reaches_the_feed() {
    let r = rig(false).await;
    register(&r, "machine-a/me").await;
    let host = r.base.strip_prefix("http://").unwrap().to_string();
    let url = format!("ws://{host}/pending/sub?token={TOKEN}"); // secret-gate: allow (the test fixture)
    let (mut feed, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let c = client(&r);
    blocking(move || {
        c.set_timer("machine-a/me", "machine-a/me", "timer", "later", "", "", None, Some(3600)).unwrap();
        c.set_timer("machine-a/me", "machine-a/me", "timer", "now", "", "", None, Some(0)).unwrap();
    })
    .await;
    let c = client(&r);
    let before = blocking(move || c.pending(None, "timer").unwrap()).await;
    assert!(before["addresses"].as_array().unwrap().is_empty(), "nothing due yet: {before}");
    fire_timers(&r.state, time::OffsetDateTime::now_utc());
    let frame = next_json(&mut feed, Duration::from_secs(10)).await.expect("a feed frame for the reminder");
    assert_eq!((frame["address"].as_str(), frame["kind"].as_str()), (Some("machine-a/me"), Some("timer")));
    let c = client(&r);
    let after = blocking(move || c.pending(None, "timer").unwrap()).await;
    let addrs = after["addresses"].as_array().unwrap();
    assert_eq!(addrs.len(), 1, "{after}");
    assert_eq!(addrs[0]["pending"], 1, "the far one is still held: {after}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_to_oneself_must_be_a_reminder() {
    let r = rig(false).await;
    let c = client(&r);
    let err = blocking(move || {
        c.set_timer("machine-a/me", "machine-a/me", "request", "s", "", "", None, Some(60)).unwrap_err().to_string()
    })
    .await;
    assert!(err.contains("400") && err.contains("timer"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn times_must_name_their_zone_and_not_be_past() {
    let r = rig(false).await;
    let c = client(&r);
    let (no_zone, past, offset, whole) = blocking(move || {
        let set = |at: &str| c.set_timer("machine-a/a", "machine-a/b", "fyi", "s", "", "", Some(at), None);
        let next_year = time::OffsetDateTime::now_utc().year() + 1;
        (
            set("2026-11-16T15:00:00").unwrap_err().to_string(),
            set("2001-01-01T00:00:00Z").unwrap_err().to_string(),
            set(&format!("{next_year}-01-01T02:00:00+02:00")).unwrap(),
            set(&format!("{next_year}-01-01T00:00:00Z")).unwrap(),
        )
    })
    .await;
    assert!(no_zone.contains("zone"), "{no_zone}");
    assert!(past.contains("past"), "{past}");
    let next_year = time::OffsetDateTime::now_utc().year() + 1;
    assert_eq!(offset.1, format!("{next_year}-01-01T00:00:00.000Z"), "+02:00 is stored as UTC");
    assert_eq!(whole.1, offset.1, "no fractional seconds is fine, and stored in the ts shape");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_id_cannot_be_acked_and_cancelling_it_stops_it() {
    let r = rig(false).await;
    register(&r, "machine-a/me").await;
    let c = client(&r);
    let (id, _) = blocking(move || c.set_timer("machine-a/me", "machine-a/me", "timer", "s", "", "", None, Some(0)).unwrap()).await;
    let c = client(&r);
    let i = id.clone();
    let err = blocking(move || c.ack("machine-a/me", &i).unwrap_err().to_string()).await;
    assert!(err.contains("--cancel"), "the refusal says how to stop it: {err}");

    let rr = Rig { base: r.base.clone(), dir: r.dir.clone(), state: r.state.clone() };
    let i = id.clone();
    let listed = blocking(move || cli(&rr, &["remind", "machine-a/me", "--list"])).await;
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&id), "{listed:?}");
    let rr = Rig { base: r.base.clone(), dir: r.dir.clone(), state: r.state.clone() };
    let out = blocking(move || cli(&rr, &["remind", "machine-a/me", "--cancel", &i])).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fire_timers(&r.state, time::OffsetDateTime::now_utc()), 0, "cancelled, so nothing fires");
}

/// A broker that predates timers answers 404 on `/timer`; the client must say what that means
/// rather than look like a delivery failure.
#[tokio::test(flavor = "multi_thread")]
async fn an_older_broker_is_named_as_the_reason() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new()).await.unwrap();
    });
    let dir = scratch();
    let r = Rig {
        base: format!("http://127.0.0.1:{port}"),
        dir: dir.clone(),
        state: AppState {
            store: Arc::new(Mutex::new(Store::open(":memory:").unwrap())),
            hub: Arc::new(Hub::new()),
            auth: Arc::new(Auth::disabled()),
        },
    };
    let out = blocking(move || {
        cli(&r, &["send", "--from", "machine-a/a", "--to", "machine-a/b", "--subject", "s", "--in", "1m"])
    })
    .await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("0.4.37"), "{out:?}");
}

/// `serve` itself must run the timer task. A test that starts the task by hand proves nothing
/// about the binary that is deployed. This also covers a timer that came due while the broker was
/// down: it fires when the broker starts.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_serve_fires_timers_including_ones_due_while_it_was_down() {
    let dir = scratch();
    let db = dir.join("bus.db");
    {
        let s = Store::open(db.to_str().unwrap()).unwrap();
        s.register(&agent_msg_bus::store::Registration {
            addr: "machine-a/b".into(),
            session_id: "s".into(),
            machine: "machine-a".into(),
            repo: "r".into(),
            cwd: "/w".into(),
            pid: 1,
            version: "t".into(),
        })
        .unwrap();
        let now = time::OffsetDateTime::now_utc();
        let nm = agent_msg_bus::store::NewMessage {
            from: "machine-a/a".into(),
            to: "machine-a/b".into(),
            kind: "timer".into(),
            subject: "overdue".into(),
            body: String::new(),
            reply_to: String::new(),
        };
        s.set_timer(&nm, &agent_msg_bus::store::ts_of(now - time::Duration::hours(1)), now - time::Duration::hours(2)).unwrap();
    }
    // A token no other test's broker knows: the port is free when picked, but another test's broker
    // can take it before `serve` binds it, and that broker must then answer 401, not mail. `serve`
    // that loses the port exits, and the test tries a new one.
    const OWN: &str = "timers-serve-only-token"; // secret-gate: allow (a test fixture)
    std::fs::write(dir.join("tokens.json"), format!(r#"{{"machine-a":"{OWN}"}}"#)).unwrap();
    let tokens = dir.join("tokens.json");
    let got = blocking(move || {
        for _attempt in 0..5 {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let mut broker = std::process::Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"))
                .args(["serve", "--listen", &format!("127.0.0.1:{port}"), "--db", db.to_str().unwrap()])
                .args(["--tokens", tokens.to_str().unwrap()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let c = Client::new(&format!("http://127.0.0.1:{port}"), OWN);
            let until = std::time::Instant::now() + Duration::from_secs(30);
            let mut got = Vec::new();
            let mut lost_port = false;
            while std::time::Instant::now() < until {
                if broker.try_wait().unwrap().is_some() {
                    lost_port = true;
                    break;
                }
                if let Ok((g, _)) = c.read_unread("machine-a/b", 10) {
                    if !g.is_empty() {
                        got = g;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let still_ours = broker.try_wait().unwrap().is_none();
            let _ = broker.kill();
            let _ = broker.wait();
            if lost_port {
                continue;
            }
            assert!(still_ours, "the answer must have come from this test's own broker");
            return got;
        }
        panic!("serve lost its port five times running");
    })
    .await;
    assert_eq!(got.len(), 1, "the overdue timer fired on start: {got:?}");
    assert_eq!(got[0].subject, "overdue");
}

/// A reminder to a mistyped address fires into a mailbox nobody reads. `/send` warns about an
/// unknown recipient at once; a timer must too, when it is set, not hours later.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_for_an_unknown_address_warns_when_it_is_set() {
    let r = rig(false).await;
    let rr = Rig { base: r.base.clone(), dir: r.dir.clone(), state: r.state.clone() };
    let out = blocking(move || cli(&rr, &["remind", "machine-a/nobody-here", "--in", "1h", "--subject", "s"])).await;
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no registration answers to 'machine-a/nobody-here'"), "{err}");
}

/// A plain send of kind `timer` to oneself is delivered now, so `/send` must not say it never is.
#[tokio::test(flavor = "multi_thread")]
async fn a_reminder_sent_now_to_oneself_is_not_called_undeliverable() {
    let r = rig(false).await;
    register(&r, "machine-a/me").await;
    let rr = Rig { base: r.base.clone(), dir: r.dir.clone(), state: r.state.clone() };
    let out = blocking(move || {
        cli(&rr, &["send", "--from", "machine-a/me", "--to", "machine-a/me", "--kind", "timer", "--subject", "now"])
    })
    .await;
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("never sent its own message"), "{err}");
    assert_eq!(unread_within(&r, "machine-a/me", 1, Duration::from_secs(5)).await.len(), 1);
}
