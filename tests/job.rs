//! `job run`: a command whose end, and whose matching lines, reach a session as bus mail
//! (Phase 11 piece D).
//!
//! The session that starts a long job should hear when it ends, and when it prints something the
//! session asked about, without a Monitor re-armed every 30 minutes. These tests run a real child
//! process against a real broker and read what the recipient's mailbox holds afterwards.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "job-test-token"; // secret-gate: allow (a test fixture)
static N: AtomicU32 = AtomicU32::new(0);

struct Env {
    dir: PathBuf,
    home: PathBuf,
}

/// A scratch folder with a config naming the machine `machine-a`, as an installed client has.
fn env() -> Env {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-job-{}-{}", std::process::id(), n));
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".agent-msg-bus")).unwrap();
    std::fs::write(
        home.join(".agent-msg-bus").join("config.json"),
        r#"{"url":"http://127.0.0.1:1","machine":"machine-a","token":""}"#,
    )
    .unwrap();
    std::fs::write(dir.join("tokens.json"), format!(r#"{{"machine-a":"{TOKEN}"}}"#)).unwrap();
    Env { dir, home }
}

async fn serve_on(e: &Env, listener: tokio::net::TcpListener) {
    let state = AppState {
        store: Arc::new(Mutex::new(Store::open(e.dir.join("bus.db").to_str().unwrap()).unwrap())),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(Auth::from_file(e.dir.join("tokens.json").to_str().unwrap()).unwrap()),
    };
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
}

async fn broker(e: &Env) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    serve_on(e, listener).await;
    format!("http://127.0.0.1:{port}")
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// A shell line, run the way each platform runs one.
fn shell(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), script.into()]
    } else {
        vec!["sh".into(), "-c".into(), script.into()]
    }
}

/// Prints `one`, `two` and `three`, then exits with 3.
fn three_lines_then_3() -> Vec<String> {
    if cfg!(windows) {
        shell("echo one& echo two& echo three& exit /b 3")
    } else {
        shell("echo one; echo two; echo three; exit 3")
    }
}

/// Runs `job run` with `args` before the `--` and `cmd` after it, and returns its output.
fn job_run(e: &Env, base: &str, args: &[&str], cmd: &[String]) -> std::process::Output {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"));
    c.args(["--url", base, "--token", TOKEN, "job", "run"])
        .args(args)
        .arg("--")
        .args(cmd)
        .env("USERPROFILE", &e.home)
        .env("HOME", &e.home)
        .env_remove("AMB_URL")
        .env_remove("AMB_TOKEN")
        .env_remove("AMB_CA_FILE");
    c.output().unwrap()
}

fn mail(base: &str, addr: &str) -> Vec<agent_msg_bus::store::Message> {
    Client::new(base, TOKEN).read(addr, None, 100).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_end_reaches_the_parent_with_exit_code_run_time_and_last_lines() {
    let e = env();
    let base = broker(&e).await;
    let b = base.clone();
    let out = blocking(move || {
        let e = Env { dir: e.dir, home: e.home };
        job_run(&e, &b, &["--to", "machine-a/parent", "--name", "scrape"], &three_lines_then_3())
    })
    .await;
    assert_eq!(out.status.code(), Some(3), "job run exits with the child's code: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("two"), "the child's output still reaches job run's own stdout: {stdout}");

    let b = base.clone();
    let got = blocking(move || mail(&b, "machine-a/parent")).await;
    assert_eq!(got.len(), 1, "one message, at the end: {got:?}");
    let m = &got[0];
    assert_eq!(m.kind, "job");
    assert_eq!(m.from, "machine-a/job", "sent from the machine's job address, never as the parent");
    assert!(m.subject.contains("scrape") && m.subject.contains("exited 3"), "subject: {}", m.subject);
    for line in ["one", "two", "three"] {
        assert!(m.body.contains(line), "body has the last lines: {}", m.body);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recipient_sees_it_unread_so_the_mod_wakes_for_it() {
    // The broker never delivers a sender its own mail. A default sender equal to the recipient
    // would make the one message that matters vanish with an id and no error.
    let e = env();
    let base = broker(&e).await;
    let b = base.clone();
    blocking(move || {
        Client::new(&b, TOKEN).register("machine-a/parent", "s", "machine-a", "parent", "/w", 1).unwrap()
    })
    .await;
    let b = base.clone();
    let out = blocking(move || {
        let e = Env { dir: e.dir, home: e.home };
        job_run(&e, &b, &["--to", "machine-a/parent"], &three_lines_then_3())
    })
    .await;
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let b = base.clone();
    let (unread, total) =
        blocking(move || Client::new(&b, TOKEN).read_unread("machine-a/parent", 100).unwrap()).await;
    assert_eq!((unread.len(), total), (1, 1), "unread for the parent: {unread:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recipient_equal_to_the_sender_is_refused_before_the_command_runs() {
    let e = env();
    let base = broker(&e).await;
    let marker = e.dir.join("ran.txt");
    let script = if cfg!(windows) {
        format!("echo x> \"{}\"", marker.display())
    } else {
        format!("echo x > '{}'", marker.display())
    };
    let b = base.clone();
    let out = blocking(move || {
        let e = Env { dir: e.dir, home: e.home };
        job_run(&e, &b, &["--to", "machine-a/job"], &shell(&script))
    })
    .await;
    assert_eq!(out.status.code(), Some(2), "refused as a usage error: {out:?}");
    assert!(!marker.exists(), "the command must not run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("own mail"), "says why: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_match_goes_at_once_and_the_rest_ride_with_the_end() {
    let e = env();
    let base = broker(&e).await;
    let script = if cfg!(windows) {
        "echo hit 1& echo miss& echo hit 2& echo hit 3& exit /b 0"
    } else {
        "echo hit 1; echo miss; echo hit 2; echo hit 3; exit 0"
    };
    let b = base.clone();
    let out = blocking(move || {
        let e = Env { dir: e.dir, home: e.home };
        job_run(&e, &b, &["--to", "machine-a/parent", "--match", "^hit", "--every", "1h"], &shell(script))
    })
    .await;
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let b = base.clone();
    let got = blocking(move || mail(&b, "machine-a/parent")).await;
    assert_eq!(got.len(), 2, "one match message, then the end: {got:?}");
    assert!(got[0].subject.contains("matched"), "first: {}", got[0].subject);
    assert!(got[0].body.contains("hit 1"), "{}", got[0].body);
    assert!(got[1].subject.contains("exited 0"), "second: {}", got[1].subject);
    assert!(
        got[1].body.contains("hit 2") && got[1].body.contains("hit 3"),
        "matches still held when the job ended go with the end message: {}",
        got[1].body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn match_without_every_is_refused() {
    let e = env();
    let base = broker(&e).await;
    let b = base.clone();
    let out = blocking(move || {
        let e = Env { dir: e.dir, home: e.home };
        job_run(&e, &b, &["--to", "machine-a/parent", "--match", "x"], &three_lines_then_3())
    })
    .await;
    assert_eq!(out.status.code(), Some(2), "clap refuses it: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--every"), "names the missing flag: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_cannot_start_is_reported_to_the_parent() {
    let e = env();
    let base = broker(&e).await;
    let b = base.clone();
    let out = blocking(move || {
        let e = Env { dir: e.dir, home: e.home };
        job_run(&e, &b, &["--to", "machine-a/parent"], &["no-such-command-amb-job-test".to_string()])
    })
    .await;
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    let b = base.clone();
    let got = blocking(move || mail(&b, "machine-a/parent")).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].subject.contains("did not start"), "{}", got[0].subject);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_end_message_waits_out_a_broker_restart() {
    // A broker swap no longer waits for anyone's yes, so a job that runs for days can end while the
    // broker is down. The end message is the one that matters, and it must arrive anyway.
    //
    // The port stays held the whole time: a port released for the "down" phase can be taken by
    // another test's broker running in parallel, and then the first send succeeds and the test
    // proves nothing (seen 16 Nov 2026). While "down", every connection is dropped unanswered.
    let e = env();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    held.set_nonblocking(true).unwrap();
    // "Down" lasts until a send has been refused, not for a fixed time: under a loaded full run,
    // starting the job took longer than a fixed 3 s and its first send found the broker up.
    let down = std::thread::spawn(move || {
        let give_up = std::time::Instant::now() + Duration::from_secs(60);
        let mut refused = 0;
        while refused == 0 && std::time::Instant::now() < give_up {
            if let Ok((sock, _)) = held.accept() {
                drop(sock);
                refused += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(refused, 1, "job run never tried to send");
        held
    });

    let b = base.clone();
    let home = e.home.clone();
    let dir = e.dir.clone();
    let run = tokio::task::spawn_blocking(move || {
        let e = Env { dir, home };
        job_run(&e, &b, &["--to", "machine-a/parent"], &three_lines_then_3())
    });
    let held = blocking(move || down.join().unwrap()).await;
    serve_on(&e, tokio::net::TcpListener::from_std(held).unwrap()).await;

    let out = tokio::time::timeout(Duration::from_secs(60), run).await.expect("job run gives up or sends").unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("trying again"), "the first send must have failed, or this proves nothing: {err}");
    let b = base.clone();
    let got = blocking(move || mail(&b, "machine-a/parent")).await;
    assert_eq!(got.len(), 1, "sent once the broker was back: {got:?}");
}
