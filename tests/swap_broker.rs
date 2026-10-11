//! The broker swap script's own tests (`scripts/test-swap-broker.sh`), run by `cargo test` so they
//! are not a separate step to remember. Linux only: the script runs on the broker's host, drives its
//! service manager and talks to /health over bash's `/dev/tcp`.
//!
//! The real binary is passed in, so one case swaps the actual broker and not only the fakes. That
//! case is what proves the script's own HTTP probe can read the broker's real `/health` reply.
#![cfg(target_os = "linux")]

#[test]
fn the_broker_swap_script_passes_its_own_tests() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/test-swap-broker.sh");
    let out = std::process::Command::new("bash")
        .arg(script)
        .env("AMB_REAL_BIN", env!("CARGO_BIN_EXE_agent-msg-bus"))
        .output()
        .expect("bash runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("SKIPPED the real-broker case"),
        "the case against the real broker did not run: {stdout}"
    );
}
