//! Is the address this session *derives* the one the broker actually knows?
//!
//! `hook::derive_address` builds an address from machine + **current working directory** + session
//! id. The registration is written once, at session start, from the cwd the session started in. Move
//! to another directory and the derivation silently changes while the registration does not, so one
//! session has as many identities as it has visited directories and only one of them exists.
//!
//! That is not a misuse to stumble into - it is the default for anyone whose work spans repos. It
//! produced two distinct failures on this bus in one week:
//!
//! - **Outbound**: four messages sent with `--from machine-a/agent-msg-bus.3da118c4`, an address that was
//!   never registered. Every reply to them bounced, and the peer who replied had done nothing wrong -
//!   they took the address from the `from:` header, and the header was fabricated.
//! - **Inbound, and worse**: `whoami` prints a *subscribe URL*. Point a watcher at a phantom address
//!   and the relay accepts the subscription, `peers` shows it live, and nothing ever arrives -
//!   because nobody is sending there. Silently, permanently dead, with every indicator reading
//!   healthy. That is the old file bus's exact signature, reachable in one command from the wrong
//!   folder.
//!
//! The check lives here, at the point the wrong value is **minted**, rather than only in `send`.
//! One read-only place fixes the return address and the subscribe URL together.

use crate::client::{KnownPeer, PeersOut};

#[derive(Debug, PartialEq, Eq)]
pub enum RegStatus {
    /// The broker knows this exact address.
    Registered,
    /// The broker does not know it. `session_peer` is the address the SAME session id is registered
    /// under, when there is one - which is the thing the caller actually wanted.
    Unregistered { session_peer: Option<String> },
    /// We could not find out. Deliberately NOT folded into `Unregistered`.
    ///
    /// An unreachable broker returns no peers, and treating that as "not registered" is precisely
    /// the defect this module exists to prevent, one level up: reporting absence as a fact when the
    /// instrument could not see. machine-b's A/B watcher failed this way and was about to announce
    /// `GUARD FAILED: both swept` at the moment the data supported no conclusion at all.
    Unverified(String),
}

/// Session id is the part after the final `.` - `machine-a/homelab.3da118c4` -> `3da118c4`.
fn session_of(addr: &str) -> Option<&str> {
    addr.rsplit_once('.').map(|(_, s)| s).filter(|s| !s.is_empty())
}

pub fn classify(addr: &str, peers: &Result<PeersOut, String>) -> RegStatus {
    let peers = match peers {
        Ok(p) => p,
        Err(e) => return RegStatus::Unverified(e.clone()),
    };
    if peers.known.iter().any(|k: &KnownPeer| k.addr == addr) {
        return RegStatus::Registered;
    }
    // Same session, different directory: name it, because it is what the caller meant.
    let session_peer = session_of(addr).and_then(|sess| {
        peers
            .known
            .iter()
            .find(|k| k.addr != addr && session_of(&k.addr) == Some(sess))
            .map(|k| k.addr.clone())
    });
    RegStatus::Unregistered { session_peer }
}

/// The lines `whoami` prints under the address. Returned rather than printed so it can be tested.
pub fn advisory(addr: &str, status: &RegStatus) -> Vec<String> {
    match status {
        RegStatus::Registered => vec![],
        RegStatus::Unverified(why) => vec![
            format!("WARNING : could not reach the broker to check this address ({why})."),
            "          It may or may not be registered - this is not a claim that it is not."
                .to_string(),
        ],
        RegStatus::Unregistered { session_peer } => {
            let mut out = vec![format!("WARNING : {addr} is NOT REGISTERED with the broker.")];
            match session_peer {
                // Only here is the cwd explanation actually TRUE: same session id, different repo
                // segment, so the address really was re-derived from a directory. Saying it in the
                // other case would assert a cause that may be false - an explicitly typed address
                // was not derived from anything - and a warning that is right about the fact and
                // wrong about the reason is the same defect this whole check exists to catch.
                Some(p) => {
                    out.push(format!("          This session is registered as {p}."));
                    out.push(
                        "          The address above was re-derived from the CURRENT DIRECTORY,"
                            .to_string(),
                    );
                    out.push(
                        "          which differs from the one this session started in.".to_string(),
                    );
                }
                None => out.push(
                    "          No registration exists for it, and no other address shares this session id."
                        .to_string(),
                ),
            }
            out.push(
                "          Do not send --from it (replies bounce) and do not subscribe to it:"
                    .to_string(),
            );
            out.push(
                "          the relay accepts the socket and peers reports it live, but nothing is"
                    .to_string(),
            );
            out.push("          ever sent there, so the inbox is silently dead.".to_string());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(addr: &str) -> KnownPeer {
        KnownPeer {
            addr: addr.into(),
            machine: "machine-a".into(),
            repo: "r".into(),
            cwd: "c".into(),
            live: false,
            pending: 0,
            aliases: vec![],
        }
    }
    fn peers(addrs: &[&str]) -> Result<PeersOut, String> {
        Ok(PeersOut { live: vec![], known: addrs.iter().map(|a| peer(a)).collect() })
    }

    #[test]
    fn a_registered_address_is_reported_clean_and_says_nothing() {
        let p = peers(&["machine-a/homelab.3da118c4"]);
        let s = classify("machine-a/homelab.3da118c4", &p);
        assert_eq!(s, RegStatus::Registered);
        assert!(advisory("machine-a/homelab.3da118c4", &s).is_empty(), "clean case must be silent");
    }

    #[test]
    fn the_cwd_derived_phantom_names_the_address_the_session_is_actually_registered_as() {
        // The real incident: whoami run inside Tools/agent-msg-bus while the session had started in
        // Tools/homelab. Same session id, different repo segment.
        let p = peers(&["machine-a/homelab.3da118c4", "machine-b/agent-msg-bus.e36dd72d"]);
        let s = classify("machine-a/agent-msg-bus.3da118c4", &p);
        assert_eq!(
            s,
            RegStatus::Unregistered { session_peer: Some("machine-a/homelab.3da118c4".into()) }
        );
        let text = advisory("machine-a/agent-msg-bus.3da118c4", &s).join("\n");
        assert!(text.contains("NOT REGISTERED"));
        assert!(text.contains("machine-a/homelab.3da118c4"), "must name the real address: {text}");
        assert!(text.contains("silently dead"), "must warn about the subscribe URL: {text}");
    }

    #[test]
    fn an_unreachable_broker_is_never_reported_as_unregistered() {
        // The whole point. Absence of evidence from a broker we could not reach is not evidence of
        // absence - reporting it as such is the failure this check exists to prevent.
        let s = classify("machine-a/homelab.3da118c4", &Err("connection refused".into()));
        assert!(matches!(s, RegStatus::Unverified(_)), "an unreachable broker became a verdict");
        let text = advisory("machine-a/homelab.3da118c4", &s).join("\n");
        assert!(text.contains("could not reach"));
        assert!(
            !text.contains("NOT REGISTERED"),
            "claimed unregistered when it simply could not see: {text}"
        );
    }

    #[test]
    fn no_registration_for_the_session_at_all_is_distinguished_from_wrong_directory() {
        let p = peers(&["machine-a/homelab.deadbeef"]);
        let s = classify("machine-a/homelab.3da118c4", &p);
        assert_eq!(s, RegStatus::Unregistered { session_peer: None });
        let text = advisory("machine-a/homelab.3da118c4", &s).join("\n");
        assert!(text.contains("No registration exists for it"), "{text}");
        // Must NOT blame the working directory here. An address typed by hand was not derived from
        // anything, and asserting a false cause is the defect this check exists to catch.
        assert!(
            !text.contains("CURRENT DIRECTORY"),
            "claimed a cwd derivation that did not happen: {text}"
        );
    }
}
