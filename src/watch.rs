//! `agent-msg-bus watch <addr>` — a subscription that reconnects itself.
//!
//! **Why this exists.** Monitor's `ws:` source ENDS THE WATCH when the socket closes; it does not
//! retry. So a relay restart closes every subscription and each session is deaf from then on until a
//! human notices and re-arms. Mail is not lost — it queues — but for an idle session "queued" and
//! "lost" have the same effect, which is precisely the failure the file bus was retired for.
//!
//! Phase 0 named this as requirement 1 ("the client must re-arm on close"). The relay solved half of
//! it: it absorbs *upstream* outages so the local socket survives a broker blip. It cannot solve its
//! own restart, because that is the socket it is holding.
//!
//! Used with Monitor's `command:` form instead of `ws:`:
//!
//! ```text
//! Monitor({command: "agent-msg-bus watch machine-a/homelab.x", persistent: true, ...})
//! ```
//!
//! Monitor keeps a `command:` watch alive while the process runs, and this process never exits on
//! its own — so reconnection happens inside it and a relay restart does not end the watch. That keeps
//! push delivery (sub-second) rather than falling back to polling, which was the obvious workaround
//! and costs up to a minute of latency.
//!
//! Monitor itself can still end it. Measured 15 Sep 2026, Monitor expires a `persistent: true` watch
//! after 30 minutes and kills this process. Nothing in here can prevent that; the session has to
//! re-arm on Monitor's expiry notice, and mail sent in the gap replays on the next subscribe.
//!
//! **One message per stdout line**, because Monitor turns each line into one notification — the same
//! rule as one-message-per-frame on the wire.

use futures_util::StreamExt;
use std::time::Duration;

const MAX_BACKOFF: u64 = 30;

/// Is the relay on a different build from this watcher? Returns the sentence to say, if so.
///
/// Asked of the RELAY rather than the broker, deliberately: the relay is this machine's, so a skew
/// against it is something the person reading this transcript can act on, while a skew against the
/// broker may be another machine's business entirely. It also cannot lie by omission — a relay too
/// old to report a version predates 0.2.0, which is itself the answer.
fn version_skew(relay: &str) -> Option<String> {
    let health: serde_json::Value = ureq::get(&format!("http://{relay}/health"))
        .timeout(Duration::from_secs(2))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let theirs = health.get("version").and_then(|v| v.as_str());
    let mine = crate::VERSION;
    let theirs = match theirs {
        Some(v) if v == mine => return None,
        Some(v) => format!("the relay on this machine is {v}"),
        None => "the relay is too old to report a version, so it predates 0.2.0".to_string(),
    };
    Some(format!(
        "this watcher is {mine}; {theirs}. Messages still flow either way - the relay only proxies. \
         Run `agent-msg-bus update` to swap the binary without stopping anything, then re-arm this \
         subscription to pick the new build up."
    ))
}

/// How often `watch` asks the broker whether this subscription still exists.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(300);

/// How many consecutive not-live answers before the alarm is raised.
///
/// **A resume is briefly indistinguishable from a death, and it is not one.** While a laptop is
/// suspended the socket dies unannounced and the broker drops the address; the relay's own
/// staleness check cannot run, because the CPU is stopped. So on waking there is a real window —
/// up to the relay's 90s detection limit — where the broker says not-live and nothing is wrong that
/// is not already fixing itself.
///
/// Firing there would train people to dismiss the one alarm that matters, which is the failure this
/// whole feature exists to avoid. Requiring a second confirmation delays a genuine death by one
/// interval; measured against the five-hour outage that prompted the feature, that is nothing.
const CONFIRM_NOT_LIVE: u32 = 2;

/// What the broker thinks of a subscription, or `None` if it could not be asked.
///
/// The broker's hub is socket state rather than an inference, which made it the only indicator
/// that was TRUE when a watcher went silently deaf: the process was alive, the relay's own
/// `/health` reported the address subscribed, and `peers` alone said offline.
fn broker_thinks_live(addr: &str) -> Option<bool> {
    let cfg = crate::hook::load_config()?;
    let client = crate::client::Client::new(&cfg.url, &cfg.token);
    let out = client.peers(true).ok()?;
    Some(out.known.iter().any(|k| k.addr == addr && k.live))
}

/// What to say about a heartbeat, or `None` to say nothing.
///
/// **Silence is the healthy state.** A line every interval would be twelve notifications an hour
/// per session, and an event stream people learn to ignore fails exactly the way silence does —
/// the reason the relay's outage repeat is five minutes rather than thirty seconds. So this speaks
/// only when the broker affirmatively disagrees that the subscription exists.
///
/// A broker that cannot be reached says nothing either: the relay already announces upstream
/// outages, and two components narrating one network failure is noise, not redundancy.
fn heartbeat_alarm(
    live: Option<bool>,
    addr: &str,
    silent_for: Duration,
    strikes: u32,
) -> Option<String> {
    match live {
        Some(true) | None => None,
        Some(false) if strikes < CONFIRM_NOT_LIVE => None,
        Some(false) => Some(format!(
            "THIS SUBSCRIPTION IS DEAD. The broker has not listed {addr} as live for {strikes} \
             consecutive checks, so mail addressed to it is queueing and nothing here will \
             receive it - though this process is running and its socket looks open. Nothing has \
             arrived for {}. Re-arm the Monitor subscription to recover; queued mail replays on \
             reconnect.",
            human_gap(silent_for)
        )),
    }
}

/// What a heartbeat tick decided to say.
#[derive(Debug, PartialEq)]
enum Beat {
    /// Announce the subscription dead. Carries the text.
    Dead(String),
    /// Announce that it is live again.
    Recovered,
    /// Say nothing, which is the overwhelmingly common case.
    Quiet,
}

/// The heartbeat's state across ticks: how many consecutive not-live answers, and whether the
/// alarm has already been raised for this outage.
///
/// Extracted from the reconnect loop because it was previously inline and therefore untestable —
/// only the per-tick formatting had tests, while the accumulate/reset/latch decisions, which are
/// the part that can over- or under-suppress, had none at all.
struct HeartbeatWatch {
    strikes: u32,
    announced_dead: bool,
}

impl HeartbeatWatch {
    fn new() -> Self {
        Self { strikes: 0, announced_dead: false }
    }

    fn observe(&mut self, live: Option<bool>, addr: &str, silent_for: Duration) -> Beat {
        // An unreachable broker resets the count rather than counting against the subscription:
        // being unable to ask is not evidence that this socket is dead.
        match live {
            Some(false) => self.strikes += 1,
            _ => self.strikes = 0,
        }
        match heartbeat_alarm(live, addr, silent_for, self.strikes) {
            // Said once per outage, like the relay's: repeating every five minutes is chatter.
            Some(note) if !self.announced_dead => {
                self.announced_dead = true;
                Beat::Dead(note)
            }
            Some(_) => Beat::Quiet,
            // Recovery requires the broker to AFFIRM that the subscription is live. A `None` here
            // means the broker could not be reached at all, which is not evidence of anything —
            // and announcing "the broker lists this subscription as live again" on the strength of
            // a broker that was never reached tells a deaf session it is fine.
            None if self.announced_dead && live == Some(true) => {
                self.announced_dead = false;
                Beat::Recovered
            }
            None => Beat::Quiet,
        }
    }
}

fn human_gap(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Give the fallback name real metadata, best-effort.
///
/// `/sub` already guarantees a row and a cursor exist, so this is not what keeps the session
/// visible — it is what stops the fallback appearing on the roster as a bare name with no repo,
/// indistinguishable from a stray. Never fatal: a watcher that cannot register is still a watcher
/// that is receiving, and refusing to run would trade a cosmetic gap for a deaf session.
fn register_bound(addr: &str) {
    let Some(cfg) = crate::hook::load_config() else { return };
    let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
    let repo = addr.rsplit('/').next().unwrap_or_default().split('.').next().unwrap_or_default();
    let client = crate::client::Client::new(&cfg.url, &cfg.token);
    if let Err(e) = client.register(
        addr,
        &std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default(),
        &cfg.machine,
        repo,
        &cwd,
        std::process::id() as i64,
    ) {
        eprintln!("watch: bound {addr} but could not register it ({e})");
    }
}

/// Emit a line describing the watcher's own connectivity. These are deliberately visible: a watcher
/// that cannot reach its relay must not look like a quiet bus.
fn status(state: &str, detail: &str) {
    println!(
        "{}",
        serde_json::json!({
            "_watch": state,
            "detail": detail,
            "note": "agent-msg-bus watch reporting its own connectivity, not a message from another session.",
        })
    );
}

type Sock = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn sub_url(relay: &str, addr: &str) -> String {
    format!("ws://{}/sub?addr={}", relay.trim_end_matches('/'), crate::client::urlencode(addr))
}

/// Is this the relay saying "somebody else already holds that address"?
///
/// Matched on the typed HTTP status rather than on the text of the error, which is a rendering and
/// can change without notice.
pub fn is_conflict(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    matches!(e, tokio_tungstenite::tungstenite::Error::Http(r) if r.status().as_u16() == 409)
}

/// Claim `primary`, or `fallback` if `primary` is already held by a live socket. Returns the socket
/// and **the name actually bound**.
///
/// This is where repo-scoped addressing is actually decided. The repo name is claimed
/// optimistically, and a collision is discovered ATOMICALLY, as a 409 at claim time — never guessed
/// in advance from a session id. The relay answers that 409 out of its own local `busy` set before
/// it opens anything upstream, so two sessions on one machine are resolved machine-locally and the
/// broker needs no change at all: this ships to a client without touching the server.
///
/// `fallback` is `None` for a PINNED address, and that is deliberate — a pin is an explicit
/// declaration of identity, so a collision on one must surface as a failure rather than quietly
/// answering to some other name.
pub async fn bind(
    relay: &str,
    primary: &str,
    fallback: Option<&str>,
) -> Result<(Sock, String), tokio_tungstenite::tungstenite::Error> {
    match tokio_tungstenite::connect_async(sub_url(relay, primary)).await {
        Ok((sock, _)) => Ok((sock, primary.to_string())),
        Err(e) => match fallback {
            Some(f) if is_conflict(&e) => {
                let (sock, _) = tokio_tungstenite::connect_async(sub_url(relay, f)).await?;
                Ok((sock, f.to_string()))
            }
            _ => Err(e),
        },
    }
}

pub async fn run(relay: &str, addr: &str, fallback: Option<&str>) -> ! {
    let mut backoff: u64 = 1;
    let mut announced_down = false;
    let mut connected_once = false;
    let mut announced_version = false;
    // The bound name is STICKY. Fallback is a first-connect decision only: after that, a 409 on
    // reconnect is almost always this watcher's own socket not yet released by the relay, and
    // falling back again would change the session's identity mid-life — so every peer's `peers`
    // output, and every reply already in flight, would be addressed to a name it had stopped
    // answering to.
    let mut bound = addr.to_string();

    loop {
        // Fallback is a FIRST-CONNECT decision only. Once a name is bound, reconnects go straight
        // back to it: a 409 then is almost always this watcher's own socket not yet released.
        let attempt = if connected_once {
            tokio_tungstenite::connect_async(sub_url(relay, &bound))
                .await
                .map(|(s, _)| (s, bound.clone()))
        } else {
            bind(relay, addr, fallback).await
        };
        match attempt {
            Ok((sock, name)) => {
                if name != addr {
                    // A notification, not a log line. A session quietly answering to a different
                    // name than the one it was handed is precisely the "healthy-looking and
                    // disconnected" shape this project exists to remove — and the sender who is
                    // about to write to the other name has no way to find out.
                    status(
                        "fallback",
                        &format!(
                            "{addr} is already held by another live session on this machine, so \
                             this session bound {name} instead. Mail sent to {addr} will NOT reach \
                             it. Tell peers this address, or pin a role name for a stable one."
                        ),
                    );
                    register_bound(&name);
                }
                bound = name;
                if announced_down || !connected_once {
                    status("connected", &format!("subscribed as {bound} via {relay}"));
                }
                // THIS IS HOW A LIVE SESSION LEARNS IT IS BEHIND. A session that was offline is told
                // by the SessionStart hook; one that is already running has no such moment, and its
                // only channel to its own transcript is this stream. Said once per process, on the
                // first successful connect, so a reconnect loop cannot turn it into a drip.
                if !announced_version {
                    announced_version = true;
                    if let Some(note) = version_skew(relay) {
                        status("version_skew", &note);
                    }
                }
                announced_down = false;
                connected_once = true;
                backoff = 1;

                let (_tx, mut rx) = sock.split();
                // Checked on a timer, reported only when the answer is wrong. A watcher can sit
                // here for hours with an open socket and a subscription the broker has already
                // forgotten; nothing at this end can tell, which is why the check asks the broker.
                let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
                heartbeat.tick().await; // the first tick completes immediately
                let mut last_frame = std::time::Instant::now();
                let mut heartbeat_state = HeartbeatWatch::new();
                loop {
                    tokio::select! {
                        msg = rx.next() => match msg {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                                last_frame = std::time::Instant::now();
                                // Verbatim, one line per frame. Never merged: Monitor turns each
                                // line into one notification, so combining two would hide one.
                                println!("{}", t.to_string().replace('\n', "\\n"));
                            }
                            Some(Ok(_)) => last_frame = std::time::Instant::now(),
                            Some(Err(_)) | None => break,
                        },
                        _ = heartbeat.tick() => {
                            match heartbeat_state.observe(
                                broker_thinks_live(&bound),
                                &bound,
                                last_frame.elapsed(),
                            ) {
                                Beat::Dead(note) => status("subscription_dead", &note),
                                Beat::Recovered => status(
                                    "subscription_recovered",
                                    "the broker lists this subscription as live again",
                                ),
                                Beat::Quiet => {}
                            }
                        }
                    }
                }
                // Reaching here means the socket closed — almost always the relay restarting.
                // Reconnect rather than exiting, which is the entire point of this subcommand.
                status("reconnecting", "subscription closed (relay restart?); retrying");
            }
            Err(e) if !connected_once && is_conflict(&e) => {
                // BOTH names are held: the fallback cannot help, and this session is receiving
                // nothing. Announced IMMEDIATELY rather than after the backoff grows, and on stdout
                // rather than stderr, because Monitor turns stdout lines into notifications and
                // stderr into nothing — so the previous version of this branch left a session deaf
                // and silent at the same time, which is the fallback wearing the exact disguise
                // this project exists to strip off.
                if !announced_down {
                    status(
                        "unreachable",
                        &format!(
                            "{addr} and {} are BOTH already held by live sockets on this machine, \
                             so this session is receiving nothing. Retrying; it will connect when \
                             one of them is released. Check `agent-msg-bus peers` to see who holds \
                             them, or pin a role name to claim a mailbox of your own.",
                            fallback.unwrap_or("(no fallback offered)")
                        ),
                    );
                    announced_down = true;
                }
                eprintln!("watch: both names held ({e}); retrying in {backoff}s");
            }
            Err(e) => {
                // Announce a sustained outage once, not on every retry. A watcher that cannot reach
                // its relay is a deaf session, and that must be visible rather than quiet.
                if !announced_down && backoff >= 16 {
                    status("unreachable", &format!("cannot reach relay at {relay}: {e}"));
                    announced_down = true;
                }
                eprintln!("watch: connect failed ({e}); retrying in {backoff}s");
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}


#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    /// The failure this exists for, reported from the machine it happened to: the watch process was
    /// alive, its socket looked open, the relay's `/health` confidently listed the address as
    /// subscribed — and the broker had no socket for it at all. Every local indicator was wrong in
    /// a reassuring direction, so the alarm must come from the broker's view, not this process's.
    #[test]
    fn a_subscription_the_broker_does_not_know_about_raises_the_alarm() {
        let a = heartbeat_alarm(Some(false), "machine-b/agent-msg-bus", Duration::from_secs(4200), CONFIRM_NOT_LIVE)
            .expect("a dead subscription said nothing");
        assert!(a.contains("machine-b/agent-msg-bus"), "the alarm did not name the address: {a}");
        assert!(a.contains("1h10m"), "the alarm did not say how long it had been silent: {a}");
        assert!(a.to_lowercase().contains("re-arm"), "the alarm gave no way out: {a}");
    }

    /// Silence IS the healthy state. A line every five minutes is twelve notifications an hour per
    /// session, and a stream people learn to ignore fails the same way silence does.
    #[test]
    fn a_healthy_subscription_says_nothing_at_all() {
        assert!(heartbeat_alarm(Some(true), "machine-a/agent-msg-bus", Duration::from_secs(6 * 3600), 0)
            .is_none(),
            "a healthy subscription spoke, and on a quiet bus it would speak forever");
    }

    /// A quiet bus is not a broken one. Hours without a frame is normal — the relay forwards only
    /// real messages, not the broker's keepalive pings — so elapsed silence must never be the
    /// trigger on its own.
    #[test]
    fn long_silence_alone_is_never_the_trigger() {
        assert!(heartbeat_alarm(Some(true), "x/y", Duration::from_secs(48 * 3600), 0).is_none());
    }

    /// An unreachable broker is the relay's story to tell. Two components narrating one network
    /// failure is noise, not redundancy.
    #[test]
    fn an_unreachable_broker_is_left_to_the_relay_to_report() {
        assert!(
            heartbeat_alarm(None, "x/y", Duration::from_secs(600), 0).is_none(),
            "watch duplicated the relay's upstream_unreachable announcement"
        );
    }
    // ---- the decisions ACROSS ticks -------------------------------------------------------
    // Everything above tests one tick's formatting. These test the accumulate / reset / latch
    // logic, which lived inline in the reconnect loop and had no coverage at all — and which is
    // the half that can over-suppress a real death or invent a recovery.

    fn beat(h: &mut HeartbeatWatch, live: Option<bool>) -> Beat {
        h.observe(live, "machine-a/repo", Duration::from_secs(600))
    }

    #[test]
    fn one_not_live_answer_never_fires_because_a_resume_produces_exactly_one() {
        let mut h = HeartbeatWatch::new();
        assert_eq!(beat(&mut h, Some(false)), Beat::Quiet);
    }

    #[test]
    fn two_consecutive_not_live_answers_fire_exactly_once() {
        let mut h = HeartbeatWatch::new();
        assert_eq!(beat(&mut h, Some(false)), Beat::Quiet, "fired on the first answer");
        assert!(matches!(beat(&mut h, Some(false)), Beat::Dead(_)), "never fired on the second");
        assert_eq!(beat(&mut h, Some(false)), Beat::Quiet, "repeated itself — that is chatter");
    }

    #[test]
    fn an_intervening_live_answer_resets_the_count() {
        let mut h = HeartbeatWatch::new();
        beat(&mut h, Some(false));
        assert_eq!(beat(&mut h, Some(true)), Beat::Quiet);
        assert_eq!(beat(&mut h, Some(false)), Beat::Quiet, "the count survived a live answer");
    }

    #[test]
    fn an_unreachable_broker_resets_the_count() {
        let mut h = HeartbeatWatch::new();
        beat(&mut h, Some(false));
        assert_eq!(beat(&mut h, None), Beat::Quiet);
        assert_eq!(beat(&mut h, Some(false)), Beat::Quiet, "an unanswerable broker counted as a strike");
    }

    /// **Being unable to ask is not evidence of recovery.** The realistic sequence is a
    /// subscription dying, then the machine losing connectivity — which has been measured
    /// repeatedly on a tunnelled laptop. Announcing "the broker lists this subscription as live
    /// again" on the strength of a broker that was never reached tells a deaf session it is fine,
    /// which is the exact failure this feature exists to prevent, emitted by the feature itself.
    #[test]
    fn an_unreachable_broker_is_not_evidence_of_recovery() {
        let mut h = HeartbeatWatch::new();
        beat(&mut h, Some(false));
        assert!(matches!(beat(&mut h, Some(false)), Beat::Dead(_)), "precondition: alarm raised");
        assert_eq!(
            beat(&mut h, None),
            Beat::Quiet,
            "claimed recovery from a broker it could not reach"
        );
    }

    #[test]
    fn a_genuine_recovery_is_announced_once_and_then_is_quiet() {
        let mut h = HeartbeatWatch::new();
        beat(&mut h, Some(false));
        assert!(matches!(beat(&mut h, Some(false)), Beat::Dead(_)));
        assert_eq!(beat(&mut h, Some(true)), Beat::Recovered);
        assert_eq!(beat(&mut h, Some(true)), Beat::Quiet, "repeated the recovery");
    }

    #[test]
    fn a_second_death_after_a_recovery_fires_again() {
        let mut h = HeartbeatWatch::new();
        beat(&mut h, Some(false));
        beat(&mut h, Some(false));
        beat(&mut h, Some(true));
        beat(&mut h, Some(false));
        assert!(
            matches!(beat(&mut h, Some(false)), Beat::Dead(_)),
            "the latch stayed set and a second real death was swallowed"
        );
    }

    /// A resume is briefly indistinguishable from a death. Measured on a laptop that suspended four
    /// times in one night: the socket dies unannounced while suspended, the broker drops the
    /// address, and the relay's own staleness check cannot run because the CPU is stopped — so on
    /// waking the broker legitimately answers not-live for up to the relay's 90s detection window,
    /// while nothing is wrong that is not already repairing itself.
    ///
    /// Firing there would train people to dismiss the one alarm that matters.
    #[test]
    fn a_single_not_live_answer_is_not_enough_because_a_resume_looks_like_a_death() {
        assert!(
            heartbeat_alarm(Some(false), "machine-b/agent-msg-bus", Duration::from_secs(36000), 1)
                .is_none(),
            "alarmed on the first not-live answer, which fires on every resume"
        );
    }

    /// But a death that persists across two checks five minutes apart is real, and the delay costs
    /// nothing against the five-hour outage this feature was built for.
    #[test]
    fn a_confirmed_death_still_alarms_and_says_it_was_confirmed() {
        let a = heartbeat_alarm(Some(false), "machine-b/agent-msg-bus", Duration::from_secs(36000), 2)
            .expect("a confirmed death stayed silent");
        assert!(a.contains("2 consecutive"), "the alarm did not say it had been confirmed: {a}");
    }
}
