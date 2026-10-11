//! The broker swap script's own tests (`scripts/test-swap-broker.sh`), run by `cargo test` so they
//! are not a separate step to remember. Linux only: the script runs on the broker's host, drives its
//! service manager and talks to /health over bash's `/dev/tcp`.
#![cfg(target_os = "linux")]

#[test]
fn the_broker_swap_script_passes_its_own_tests() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/test-swap-broker.sh");
    let out = std::process::Command::new("bash")
        .arg(script)
        .output()
        .expect("bash runs");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
