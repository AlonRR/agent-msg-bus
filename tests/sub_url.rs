//! `sub-url` against an https broker.
//!
//! An https broker takes the token only in an `Authorization` header, and the TLS front in front
//! of it writes every request URL to its access log. `sub-url` used to print a plain `ws://` URL
//! with the token in its query whatever the scheme: one that could not connect to a TLS port, and
//! that would have put the token in that log had anyone used it. Found by a peer on 0.4.26.

use std::process::{Command, Output};

const TOKEN: &str = "sub-url-test-token-41c9";

/// Runs `sub-url` with no config file in reach, so only the flags given here count.
fn sub_url(broker: &str) -> Output {
    let home = std::env::temp_dir().join(format!("amb-sub-url-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_agent-msg-bus"))
        .args(["--url", broker, "--token", TOKEN, "sub-url", "machine-a/thing"])
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .env_remove("AMB_URL")
        .env_remove("AMB_TOKEN")
        .env_remove("AMB_CA_FILE")
        .output()
        .expect("the binary runs")
}

#[test]
fn sub_url_refuses_an_https_broker_rather_than_print_the_token_in_a_url() {
    let out = sub_url("https://broker.example:9450");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "gave a URL for an https broker: {stdout}");
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN), "printed the token: {stdout}{stderr}");
    assert!(stderr.contains("watch"), "does not say what to use instead: {stderr}");
}

#[test]
fn sub_url_for_a_plain_http_broker_is_unchanged() {
    let out = sub_url("http://192.0.2.10:9450");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        stdout.trim(),
        format!("ws://192.0.2.10:9450/sub?addr=machine-a%2Fthing&token={TOKEN}") // secret-gate: allow
    );
}
