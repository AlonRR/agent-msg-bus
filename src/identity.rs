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

/// What `whoami` should tell the caller to DO.
///
/// Derived from the same `RegStatus` as the warning, so the two cannot disagree. They used to be
/// computed independently: `whoami` printed a subscribe command unconditionally and then printed a
/// warning saying not to subscribe to that address. Both were individually correct and together
/// useless, because the reader was left to arbitrate — and the reason the command is printed at all
/// is so that they do not have to.
#[derive(Debug)]
pub enum Recommend {
    /// Registered: subscribing is safe, say so in one line.
    Subscribe,
    /// A legitimate address that simply has no registry row yet. Registering is the fix.
    ///
    /// This case became the normal one when addresses became repo-scoped. Before that, an
    /// unregistered derived address nearly always meant a phantom minted from the wrong directory;
    /// now it usually means "nobody has claimed this repo's mailbox yet", which is not a hazard —
    /// it is a missing step, and the difference matters because the old advice was "avoid it".
    RegisterThenSubscribe,
    /// The same session already has a mailbox under another name. Use that one.
    ///
    /// Registering the derived name here would mint a SECOND mailbox for one session and split its
    /// mail across two addresses, which is worse than the confusion it would resolve.
    UseInstead(String),
    /// The broker could not be reached. Recommend nothing confidently in either direction.
    Unknown,
}

pub fn recommend(status: &RegStatus) -> Recommend {
    match status {
        RegStatus::Registered => Recommend::Subscribe,
        RegStatus::Unverified(_) => Recommend::Unknown,
        RegStatus::Unregistered { session_peer: Some(p) } => Recommend::UseInstead(p.clone()),
        RegStatus::Unregistered { session_peer: None } => Recommend::RegisterThenSubscribe,
    }
}

/// Old session-suffixed names for *this* address that nothing has aliased to it — i.e. addresses
/// that still exist and will silently strand anything sent to them. Worst first.
///
/// **The gap the repo-scoped rollout opened.** When a session moves from `machine-a/x.<session>` to
/// `machine-a/x`, its old registration does not go anywhere. Anyone still holding the old name sends
/// there, the mail queues where nobody is listening, and neither end sees an error — the sender is
/// told "queued for a known address", which is true and useless. Migrating fixes it in one command;
/// the difficulty was never the fix, it was that nothing told anyone the trap existed.
///
/// Measured on the live bus the day after the rollout: **12 such addresses holding 23 unread
/// messages**, and the only aliased one was the address whose session had done it by hand.
///
/// Matched on an exact `<me>.<suffix>` prefix, so `machine-a/tools-extra.abc` is never claimed as a
/// sibling of `machine-a/tools`, and neither is another machine's copy of the same repo name.
pub fn stranding_siblings(me: &str, peers: &PeersOut) -> Vec<(String, usize)> {
    let mine: Option<&KnownPeer> = peers.known.iter().find(|k| k.addr == me);
    let aliased: &[String] = mine.map(|k| k.aliases.as_slice()).unwrap_or(&[]);
    let prefix = format!("{me}.");
    let mut out: Vec<(String, usize)> = peers
        .known
        .iter()
        .filter(|k| k.addr.starts_with(&prefix))
        .filter(|k| !aliased.iter().any(|a| a == &k.addr))
        .map(|k| (k.addr.clone(), k.pending))
        .collect();
    // Worst first: the one holding unread mail is the one worth acting on today.
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
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
                // No sibling: this is almost certainly the repo's own address, simply unclaimed.
                // Since addresses became repo-scoped that is a MISSING STEP, not a hazard, and the
                // blanket "do not subscribe to it" this branch used to print was advice from the
                // session-derived era — where an unregistered address really did mean a phantom.
                // Saying it here sent a session away from its own correct mailbox.
                None => {
                    out.push(
                        "          Nothing has claimed it yet, and no other address shares this session id."
                            .to_string(),
                    );
                    out.push(
                        "          Register it before relying on it — until something does, senders".to_string(),
                    );
                    out.push(
                        "          are told it does not exist and their mail is orphaned:".to_string(),
                    );
                    out.push(format!("            agent-msg-bus register {addr}"));
                    return out;
                }
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
            version: String::new(),
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
        assert!(text.contains("NOT REGISTERED"), "{text}");
        // Must NOT blame the working directory here. An address typed by hand was not derived from
        // anything, and asserting a false cause is the defect this check exists to catch.
        assert!(
            !text.contains("CURRENT DIRECTORY"),
            "claimed a cwd derivation that did not happen: {text}"
        );
        // And it must name the fix. Since addresses became repo-scoped, an unclaimed address is
        // usually a missing step rather than a phantom, so the advice that belongs here is how to
        // claim it — not the blanket "do not subscribe to it" this branch used to print, which sent
        // a session away from its own correct mailbox.
        assert!(text.contains("register machine-a/homelab.3da118c4"), "no fix offered: {text}");
        assert!(
            !text.contains("do not subscribe to it"),
            "still telling a session to avoid its own repo address: {text}"
        );
    }
}

#[cfg(test)]
mod recommend_tests {
    use super::*;

    /// THE INVARIANT THIS FILE EXISTS TO KEEP, and the one it broke: whatever `whoami` tells a
    /// caller to run must not be the thing it warns them against three lines later.
    ///
    /// Reported from a live session on 5 Sep 2026, against 0.4.0: `whoami` printed
    /// `subscribe: Monitor({command: "... watch machine-a/inventory ..."})` and then
    /// `WARNING: machine-a/inventory is NOT REGISTERED ... do not subscribe to it`. The recommendation
    /// and the diagnosis were computed independently and simply disagreed, leaving the reader to
    /// arbitrate — which is exactly the job printing the line was supposed to remove.
    #[test]
    fn no_status_both_warns_and_recommends_a_bare_subscribe() {
        let cases = [
            RegStatus::Registered,
            RegStatus::Unregistered { session_peer: None },
            RegStatus::Unregistered { session_peer: Some("machine-a/elsewhere".into()) },
            RegStatus::Unverified("broker down".into()),
        ];
        for status in cases {
            let warns = !advisory("machine-a/thing", &status).is_empty();
            let bare = matches!(recommend(&status), Recommend::Subscribe);
            assert!(
                !(warns && bare),
                "{status:?} warns AND recommends a plain subscribe — the two disagree"
            );
        }
    }

    /// A registered address is the common case and must stay a single clean line.
    #[test]
    fn a_registered_address_is_simply_told_to_subscribe() {
        assert!(matches!(recommend(&RegStatus::Registered), Recommend::Subscribe));
    }

    /// Repo-scoped addressing changed what an unregistered address MEANS. It used to imply a
    /// phantom derived from the wrong directory; now it is usually a perfectly good repo address
    /// that simply has no row yet, and the fix is to register it — not to avoid it.
    #[test]
    fn an_unregistered_address_with_no_sibling_is_told_to_register_first() {
        assert!(matches!(
            recommend(&RegStatus::Unregistered { session_peer: None }),
            Recommend::RegisterThenSubscribe
        ));
    }

    /// The original danger is still real and must keep its original answer: same session id, a
    /// different directory, and a mailbox that already exists somewhere else. Registering the
    /// derived name here would create a SECOND mailbox and split the session's mail in two.
    #[test]
    fn a_sibling_registration_means_use_that_one_not_this() {
        let r = recommend(&RegStatus::Unregistered {
            session_peer: Some("machine-a/elsewhere".into()),
        });
        match r {
            Recommend::UseInstead(a) => assert_eq!(a, "machine-a/elsewhere"),
            other => panic!("expected UseInstead, got {other:?}"),
        }
    }

    /// An unreachable broker must never produce a confident instruction in either direction.
    #[test]
    fn an_unverified_status_recommends_nothing_confidently() {
        assert!(matches!(
            recommend(&RegStatus::Unverified("x".into())),
            Recommend::Unknown
        ));
    }
}

#[cfg(test)]
mod sibling_tests {
    use super::*;

    fn p(addr: &str, pending: usize, aliases: &[&str]) -> KnownPeer {
        KnownPeer {
            addr: addr.into(),
            machine: "machine-a".into(),
            repo: "r".into(),
            cwd: "c".into(),
            live: false,
            pending,
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            version: String::new(),
        }
    }
    fn out(v: Vec<KnownPeer>) -> PeersOut {
        PeersOut { live: vec![], known: v }
    }

    /// The gap the repo-scoped rollout opened: a session moves from `machine-a/x.<session>` to
    /// `machine-a/x`, and its old name keeps existing as a separate registration. Anyone still holding
    /// it sends there and the mail queues where nobody is listening, with no error at either end.
    /// Measured on the live bus the day after the rollout: 12 such addresses holding 23 unread.
    #[test]
    fn an_unaliased_session_suffixed_sibling_is_reported_as_a_trap() {
        let peers = out(vec![p("machine-a/tools", 0, &[]), p("machine-a/tools.7b7dddac", 9, &[])]);
        let found = stranding_siblings("machine-a/tools", &peers);
        assert_eq!(found.len(), 1, "the old name was not reported: {found:?}");
        assert_eq!(found[0].0, "machine-a/tools.7b7dddac");
        assert_eq!(found[0].1, 9, "unread count not carried through");
    }

    /// Once migrated it is not a trap — mail to it resolves. Reporting it anyway would train people
    /// to ignore the warning, which is how a real one gets missed.
    #[test]
    fn an_aliased_sibling_is_not_reported() {
        let peers = out(vec![
            p("machine-a/tools", 0, &["machine-a/tools.7b7dddac"]),
            p("machine-a/tools.7b7dddac", 9, &[]),
        ]);
        assert!(stranding_siblings("machine-a/tools", &peers).is_empty());
    }

    /// Another repo's addresses are none of my business, and a prefix match would claim them:
    /// `machine-a/tools-extra.abc` must not look like a sibling of `machine-a/tools`.
    #[test]
    fn a_different_repo_is_never_claimed_as_a_sibling() {
        let peers = out(vec![
            p("machine-a/tools", 0, &[]),
            p("machine-a/tools-extra.7b7dddac", 4, &[]),
            p("machine-a/toolsmith", 3, &[]),
            p("machine-b/tools.7b7dddac", 5, &[]),
        ]);
        assert!(
            stranding_siblings("machine-a/tools", &peers).is_empty(),
            "claimed an address belonging to another repo or machine"
        );
    }

    /// A session on a session-suffixed address of its own has no siblings to worry about — the
    /// check is for a repo-scoped address looking back at what it replaced.
    #[test]
    fn several_siblings_are_all_reported_worst_first() {
        let peers = out(vec![
            p("machine-a/homelab", 0, &[]),
            p("machine-a/homelab.3da118c4", 0, &[]),
            p("machine-a/homelab.814c3d22", 7, &[]),
            p("machine-a/homelab.8e13fdc7", 2, &[]),
        ]);
        let found = stranding_siblings("machine-a/homelab", &peers);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].1, 7, "not ordered by unread count, so the worst is not first");
        assert_eq!(found[2].1, 0);
    }
}
