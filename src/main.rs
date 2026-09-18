//! One binary, both halves: `serve` runs the broker, everything else is the client a session calls.
//!
//! Single binary because it deploys to a CT and to three client machines, and two artefacts is two
//! things to keep in step. The old bus drifted precisely that way — two hand-synced copies of the
//! same scripts, kept aligned only by whoever remembered to copy one to the other.

use agent_msg_bus::client::Client;
use agent_msg_bus::identity::Recommend;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use clap::{Parser, Subcommand};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
// `version` takes the crate version from Cargo.toml, so `--version` and the release tag cannot
// drift apart without the tag being wrong. Several machines run their own copy of this binary and
// they are updated at different times; without this, "is the fix actually deployed over there?"
// could only be answered by hashing files.
#[command(
    name = "agent-msg-bus",
    version,
    about = "Push-delivery message bus for Claude Code sessions"
)]
struct Cli {
    /// Broker base URL for client commands. Env: AMB_URL
    #[arg(long, global = true, env = "AMB_URL", default_value = "http://127.0.0.1:9450")]
    url: String,

    /// Token for client commands. Env: AMB_TOKEN
    #[arg(long, global = true, env = "AMB_TOKEN", default_value = "")]
    token: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the broker.
    Serve {
        #[arg(long, default_value = "127.0.0.1:9450")]
        listen: String,
        #[arg(long, default_value = "agent-msg-bus.db")]
        db: String,
        /// JSON file of {"machine": "token"}.
        #[arg(long)]
        tokens: Option<String>,
        /// Run with no authentication. Explicit only — a missing tokens file is a hard error, never
        /// a silent downgrade to open access.
        #[arg(long)]
        insecure_no_auth: bool,
        /// How often to sweep aged-out provisional registrations. 0 disables the sweeper.
        #[arg(long, default_value_t = 30)]
        sweep_minutes: u64,
        /// Provisional registrations older than this, with no live socket, are forgotten.
        #[arg(long, default_value_t = 6)]
        provisional_hours: i64,
    },
    /// Claim an address.
    Register {
        addr: String,
        #[arg(long, default_value = "")]
        session_id: String,
        #[arg(long, default_value = "")]
        machine: String,
        #[arg(long, default_value = "")]
        repo: String,
        #[arg(long, default_value = "")]
        cwd: String,
        #[arg(long, default_value_t = 0)]
        pid: i64,
    },
    /// Send a message to another session.
    Send {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "fyi")]
        kind: String,
        #[arg(long)]
        subject: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Read the body from a file, or from stdin with `-`.
        ///
        /// Prefer this for anything non-trivial. A body passed as a shell argument is interpolated
        /// by that shell first: backticks become command substitution in bash, and `$` expands in
        /// both bash and double-quoted PowerShell. The result is a body with silent holes in it —
        /// `send` still exits 0 and returns an id, so the sender sees success while the recipient
        /// reads prose that is fluent and wrong. Message bodies *about* shell commands are exactly
        /// the ones most likely to contain both characters.
        #[arg(long)]
        body_file: Option<String>,
        #[arg(long, default_value = "")]
        reply_to: String,
    },
    /// Confirm messages up to and including this id have been handled.
    Ack { addr: String, up_to_id: String },
    /// Who is on the bus (addresses that have actually joined).
    Peers {
        /// Include provisional addresses: sessions that registered but never subscribed.
        #[arg(long)]
        all: bool,
    },
    /// Migrate a mailbox: <to> starts answering to <from> and inherits its unread mail.
    ///
    /// Use when a session ended and its successor has a different derived address, so mail queued
    /// for the dead one would otherwise be stranded. Existing messages are not rewritten — the
    /// alias is resolved at delivery time, so history stays true.
    Migrate { from: String, to: String },
    /// Pin this session's address so it stops being derived from the session id.
    ///
    /// Two problems, one fix: a derived address changes every restart, so a mailbox cannot carry
    /// over, and every session-directory pair leaves a permanent registry entry.
    Pin { addr: String },
    /// Drop this session's pin and go back to the derived address.
    Unpin,
    /// Retire an address (drops its registration and cursor, not its message history).
    Forget { addr: String },
    /// List mail addressed to something no registration answers to.
    ///
    /// Storage with no owner: no registration means nothing enumerates it and neither `forget` nor
    /// `prune` can reach it. Usually a mistyped recipient. `--delete <id>` clears one.
    Orphans {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        delete: Option<String>,
    },
    /// Read stored messages for an address. Does NOT consume them or move the cursor.
    ///
    /// Use this to recover a message that arrived truncated in a notification — delivery is
    /// push-only, so without this the full text of a long message was unrecoverable once acked.
    Read {
        addr: String,
        /// Only messages after this message id — YYYYMMDDThhmmssmmm-nnnnnnnnn. NOT a timestamp.
        ///
        /// Ids are compared as text, so a timestamp does not narrow the read: it matches every
        /// stored row and returns the whole history looking like a filtered one. Anything that is
        /// not an id is refused rather than accepted.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Forget registrations with no live socket that have not been seen for a while.
    ///
    /// Dry run unless --yes. The registry gains an entry per session-directory and never loses one,
    /// so dead addresses accumulate — and a wildcard send fans out to every one of them.
    Prune {
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// Provisional addresses (registered, never subscribed) expire on this much shorter clock.
        #[arg(long, default_value_t = 6)]
        provisional_hours: i64,
        #[arg(long)]
        yes: bool,
    },
    /// Print the ws:// URL to hand to Monitor.
    SubUrl { addr: String },
    /// Hold the broker connection on this machine and re-serve it on loopback.
    ///
    /// Monitor refuses to open a WebSocket to a private IP, so it cannot reach the broker directly;
    /// it can reach 127.0.0.1. One of these per MACHINE, multiplexing every session on it — run it
    /// as a service, not per session.
    Relay {
        #[arg(long, default_value = "127.0.0.1:9451")]
        listen: String,
    },
    /// Claude Code SessionStart hook. Reads the hook payload on stdin, emits hook JSON on stdout.
    SessionStart,
    /// Print this session's derived address and its Monitor subscribe URL.
    Whoami,
    /// Replace the installed binary with a newer one, WITHOUT stopping anything.
    ///
    /// A running executable cannot be overwritten but can be renamed, so the installed binary is
    /// moved aside (keeping its version in the name) and the new one copied into place. Processes
    /// already running keep executing the old file, undisturbed and still on the old build; only new
    /// invocations pick up the new one.
    ///
    /// Nothing is killed and nothing is restarted — which is what makes this safe on a machine whose
    /// relay supervisor is broken, where killing the relay would leave every session on it deaf with
    /// no way back. The price is that long-lived processes stay on the old build until something
    /// restarts them, and this command reports exactly which ones rather than deciding for you.
    Update {
        /// New binary to install. Defaults to this repo's `target/release` build.
        #[arg(long)]
        from: Option<String>,
        /// Installed binary to replace. Defaults to this platform's install location.
        #[arg(long)]
        to: Option<String>,
        /// Say what would change, then stop.
        #[arg(long)]
        dry_run: bool,
        /// Install even when the source is OLDER than what is already installed.
        ///
        /// Needed for a deliberate rollback. Without it an older source is refused, because the
        /// default source is this repo's last `target/release` build and that can be months behind
        /// the installed binary — a downgrade nobody asked for, machine-wide and silent.
        #[arg(long)]
        force: bool,
    },
    /// Install a newer build if one is available, and do nothing otherwise. Never fails.
    ///
    /// Built for a machine's startup script to call, unattended, once per boot: it decides for
    /// itself whether there is anything to do and **always exits 0**, because a starter that cannot
    /// bring the fleet up because an update check failed is worse than a machine on yesterday's
    /// build. Nothing is killed or restarted, exactly as `update`.
    ///
    /// The source is the first of: `--from`, `AMB_UPDATE_SOURCE`, `update_source` in the config
    /// file, then this repo's `target/release` build if there is one. A machine with none of those
    /// never self-updates, which is the safe default for one nobody has told where builds come from.
    SelfUpdate {
        /// Binary to install from, overriding every other source.
        #[arg(long)]
        from: Option<String>,
        /// Installed binary to replace. Defaults to this platform's install location.
        #[arg(long)]
        to: Option<String>,
        /// Say what would happen, change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Subscribe and print each message as a line, reconnecting forever.
    ///
    /// Use this with Monitor's `command:` form instead of `ws:`. Monitor's ws source ENDS the watch
    /// when the socket closes and does not retry, so a relay restart leaves the session deaf until a
    /// human re-arms it. This reconnects internally, so a relay restart does not end the watch.
    /// Monitor itself may still expire it; the session must re-arm when Monitor says so.
    Watch {
        addr: String,
        #[arg(long)]
        relay: Option<String>,
        /// Name to bind instead if ADDR is already held by another live session on this machine.
        ///
        /// This is what makes a repo-scoped address safe to claim optimistically: the collision is
        /// discovered as a 409 at claim time rather than guessed in advance, so the common case
        /// (one session per repo) gets the stable name and the rare case still gets a mailbox.
        /// Omit it for a PINNED address — a pin is an explicit identity, and a collision on one
        /// should fail loudly rather than quietly answer to a different name.
        #[arg(long)]
        fallback: Option<String>,
    },
}

/// Resolve broker URL and token: explicit flag/env first, then `~/.agent-msg-bus/config.json`.
///
/// The config fallback is what lets the relay run as a Scheduled Task or systemd unit without a
/// token on its command line or in a unit file - the two places a secret is most likely to be read
/// by something that should not have it.
fn resolve_conn(cli: &Cli) -> (String, String) {
    let default_url = "http://127.0.0.1:9450";
    let cfg = agent_msg_bus::hook::load_config();
    let url = if cli.url != default_url {
        cli.url.clone()
    } else {
        cfg.as_ref().map(|c| c.url.clone()).unwrap_or_else(|| cli.url.clone())
    };
    let token = if !cli.token.is_empty() {
        cli.token.clone()
    } else {
        cfg.as_ref().map(|c| c.token.clone()).unwrap_or_default()
    };
    (url, token)
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { listen, db, tokens, insecure_no_auth, sweep_minutes, provisional_hours } => {
            serve(&listen, &db, tokens.as_deref(), insecure_no_auth, sweep_minutes, provisional_hours)
        }
        Cmd::Relay { ref listen } => {
            let listen = listen.clone();
            let (url, token) = resolve_conn(&cli);
            relay(&listen, &url, &token)
        }
        Cmd::SessionStart => agent_msg_bus::hook::run(),
        Cmd::Watch { ref addr, ref relay, ref fallback } => {
            let relay = relay.clone().or_else(|| {
                agent_msg_bus::hook::load_config().map(|c| c.relay)
            }).unwrap_or_else(|| "127.0.0.1:9451".to_string());
            watch(&relay, addr, fallback.clone())
        }
        Cmd::Whoami => whoami(&cli),
        Cmd::Update { ref from, ref to, dry_run, force } => {
            update(from.as_deref(), to.as_deref(), dry_run, force)
        }
        Cmd::SelfUpdate { ref from, ref to, dry_run } => {
            self_update(from.as_deref(), to.as_deref(), dry_run)
        }
        _ => run_client(&cli),
    }
}

#[tokio::main]
async fn watch(relay: &str, addr: &str, fallback: Option<String>) -> ! {
    agent_msg_bus::watch::run(relay, addr, fallback.as_deref()).await
}

/// Swap the installed binary. Reports what stays on the old build; never restarts anything.
fn update(from: Option<&str>, to: Option<&str>, dry_run: bool, force: bool) {
    use agent_msg_bus::update as up;
    let from = from.map(std::path::PathBuf::from).unwrap_or_else(up::default_source_path);
    let to = to.map(std::path::PathBuf::from).unwrap_or_else(up::default_install_path);

    let plan = match up::plan(&from, &to) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent-msg-bus: {e}");
            std::process::exit(1);
        }
    };

    println!("from : {}  ({})", plan.from.display(), plan.from_version);
    println!(
        "to   : {}  ({})",
        plan.to.display(),
        plan.to_version.clone().unwrap_or_else(|| "not installed, or too old to report".into())
    );

    if plan.already_current() {
        println!("\nalready on {}; nothing to do.", plan.from_version);
        return;
    }
    // Before --dry-run, deliberately: a dry run of a refused update should report the refusal, not
    // describe a swap that would not happen.
    if let Some(why) = plan.downgrade_refusal() {
        if !force {
            eprintln!("\nagent-msg-bus: {why}");
            std::process::exit(1);
        }
        println!(
            "\n--force: installing {} over the newer {} deliberately.",
            plan.from_version,
            plan.to_version.clone().unwrap_or_default()
        );
    }
    if dry_run {
        println!("\n--dry-run: would move the installed binary to {}", plan.backup.display());
        return;
    }

    if let Err(e) = up::apply(&plan) {
        eprintln!("agent-msg-bus: {e}");
        std::process::exit(1);
    }
    println!("\ninstalled {} at {}", plan.from_version, plan.to.display());
    println!("previous build kept at {}", plan.backup.display());

    // The whole point of the rename-swap is that these are STILL RUNNING. Saying so is not a
    // warning about a failure; it is the completion report. An update that silently left a relay on
    // the old build would be the same class of quiet half-done state this project keeps fixing.
    let holders = up::processes_still_on_the_old_build();
    println!();
    if holders.is_empty() {
        println!("Nothing is running the old binary. The next invocation of anything uses the new one.");
    } else {
        println!("STILL RUNNING THE OLD BUILD — nothing was killed, by design:");
        for h in &holders {
            println!("  {h}");
        }
        println!();
        println!("Each keeps the build it started with until it is restarted:");
        println!("  - a session picks the new build up at its next start, when the SessionStart");
        println!("    hook runs the installed binary (a `watch`, if one was deliberately started,");
        println!("    keeps the old build until that watch is restarted);");
        println!("  - the relay updates only when the relay is restarted, which is a decision about");
        println!("    whether this machine can get its relay back — check that its supervisor can");
        println!("    actually relaunch it BEFORE stopping it.");
    }
}

/// `self-update` — install a newer build if there is one, and never be the reason a boot fails.
///
/// The starter script calls this once per boot and passes no arguments, which is the whole point:
/// every decision lives here, so the starter is one fixed line that never needs revisiting. It
/// **always exits 0**, including when the swap itself fails — a machine left on yesterday's build is
/// a much smaller problem than a fleet that did not start because an update check went wrong. A
/// failure is still said out loud, on stderr, rather than swallowed.
fn self_update(from: Option<&str>, to: Option<&str>, dry_run: bool) {
    use agent_msg_bus::update as up;

    let to = to.map(std::path::PathBuf::from).unwrap_or_else(up::default_install_path);
    let cfg_source = agent_msg_bus::hook::load_config().map(|c| c.update_source).unwrap_or_default();
    let env_source = std::env::var("AMB_UPDATE_SOURCE").unwrap_or_default();

    let Some(from) = up::resolve_source(from, Some(env_source.as_str()), Some(cfg_source.as_str()))
    else {
        println!(
            "self-update: no source configured on this machine; nothing to do. Set `update_source` \
             in the config file (or AMB_UPDATE_SOURCE) to the binary this machine should install \
             from. There is deliberately no implicit source — see `update` for the by-hand path."
        );
        return;
    };
    if !from.exists() {
        println!("self-update: source {} does not exist; nothing to do.", from.display());
        return;
    }

    let source_version = up::version_of(&from);
    let installed = if to.exists() { up::version_of(&to) } else { None };

    match up::decide(source_version.as_deref(), installed.as_deref()) {
        up::SelfUpdate::NoSource => {
            println!("self-update: {} could not report a version; nothing to do.", from.display())
        }
        up::SelfUpdate::AlreadyCurrent(v) => println!("self-update: already on {v}; nothing to do."),
        up::SelfUpdate::SourceIsOlder { source, installed } => println!(
            "self-update: the source is {source} and {installed} is installed — keeping the newer \
             one. Nothing to do."
        ),
        up::SelfUpdate::CannotCompare { source, installed } => println!(
            "self-update: cannot tell whether {source} is newer than the installed {installed}, so \
             nothing was changed."
        ),
        up::SelfUpdate::Install { version } => {
            if dry_run {
                println!(
                    "self-update: --dry-run: would install {version} from {} over {}",
                    from.display(),
                    to.display()
                );
                return;
            }
            match up::plan(&from, &to).and_then(|p| up::apply(&p).map(|()| p)) {
                Ok(p) => {
                    println!("self-update: installed {version} at {}", p.to.display());
                    println!("self-update: previous build kept at {}", p.backup.display());
                    let holders = up::processes_still_on_the_old_build();
                    if !holders.is_empty() {
                        println!(
                            "self-update: {} process(es) still run the old build — nothing was \
                             killed, by design; each picks the new one up when it next starts.",
                            holders.len()
                        );
                    }
                }
                // Loud, and still exit 0: the caller is a boot task that has a fleet to start.
                Err(e) => eprintln!("self-update: FAILED to install {version}: {e} — continuing."),
            }
        }
    }
}

/// The line a sender reads back to check they addressed the message they think they did.
///
/// Exists because `docs/usage.md` promised it and the code did not provide it: senders were told the
/// output "names the recipient — the only signal that would catch a wrong address", while `send`
/// printed the id and nothing else. The misaddressed send it was meant to catch has now happened
/// three times in a week, and it survives review every time because the body and subject are freshly
/// written and correct — only the header is stale.
///
/// Direction is explicit rather than positional: naming both addresses without saying which is which
/// would still let a glance land on the wrong one.
fn send_confirmation(id: &str, from: &str, to: &str) -> String {
    // ONE line, not two. `2>&1 | tail -1` is a common idiom in exactly the scripted sends most at
    // risk, and it splits a two-line confirmation — leaving whichever half it lands on, possibly the
    // one without the recipient. A self-contained line survives truncation to a single line.
    format!("sent {id}  {from}  ->  {to}")
}

fn whoami(cli: &Cli) {
    let Some(cfg) = agent_msg_bus::hook::load_config() else {
        eprintln!(
            "agent-msg-bus: no config at {}",
            agent_msg_bus::hook::config_path().display()
        );
        std::process::exit(1);
    };
    let session = std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
    let addr = agent_msg_bus::hook::derive_address(&cfg.machine, &cwd);
    let pinned = agent_msg_bus::hook::pinned_address().is_some();
    let fallback = if pinned || session.is_empty() {
        None
    } else {
        Some(agent_msg_bus::hook::fallback_address(&cfg.machine, &cwd, &session))
    };

    // Which name is ACTUALLY bound, asked of the relay rather than re-derived. Deriving it twice
    // only ever reproduces the same guess: if this session fell back to the disambiguated name
    // because the repo address was taken, a derivation cannot know that and `whoami` would confidently
    // report an address nothing is listening on. The relay knows because it holds the socket.
    let bound: Option<String> = ureq::get(&format!("http://{}/health", cfg.relay))
        .timeout(std::time::Duration::from_secs(2))
        .call()
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|v| {
            let held: Vec<String> = v
                .get("subscribed")?
                .as_array()?
                .iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect();
            held.iter()
                .find(|h| *h == &addr)
                .or_else(|| held.iter().find(|h| Some(h.as_str()) == fallback.as_deref()))
                .cloned()
        });

    // Ask the broker whether this derived address is real BEFORE handing out a subscribe URL for it.
    // An unchecked URL is how a session gets a silently dead inbox: the relay accepts the socket,
    // `peers` reports it live, and nothing is ever sent there because nobody knows the address.
    // Use the RESOLVED connection, not cfg directly - otherwise --url is silently ignored here
    // while working everywhere else, which is its own small version of this bug.
    let (url, token) = resolve_conn(cli);
    let peers = agent_msg_bus::client::Client::new(&url, &token)
        .peers(true)
        .map_err(|e| e.to_string());
    let status = agent_msg_bus::identity::classify(&addr, &peers);

    println!("address : {addr}{}", if pinned { "   (pinned)" } else { "   (from the repo)" });
    if let Some(f) = &fallback {
        println!("fallback: {f}   (used only if another live session already holds the address)");
    }
    match &bound {
        Some(b) if b == &addr => println!("bound   : {b}   (subscribed on this machine's relay)"),
        Some(b) => println!("bound   : {b}   ← FELL BACK; the address above is held by another session"),
        None => println!("bound   : (nothing on this machine's relay is subscribed as either name)"),
    }
    println!("broker  : {url}");
    println!("relay   : {}", cfg.relay);
    let watch_cmd = match &fallback {
        Some(f) => format!("agent-msg-bus watch {addr} --fallback {f}"),
        None => format!("agent-msg-bus watch {addr}"),
    };
    // The recommendation is derived from the SAME status as the warning below, so the two cannot
    // contradict each other. They used to be independent: this line was printed unconditionally and
    // a warning three lines later said not to subscribe to the address it named. Reported from a
    // live session, which correctly refused to follow it.
    match agent_msg_bus::identity::recommend(&status) {
        Recommend::Subscribe => {
            println!("subscribe: Monitor({{command: \"{watch_cmd}\", persistent: true}})");
        }
        Recommend::RegisterThenSubscribe => {
            println!("subscribe: NOT YET — this address has no registration, so nothing knows to");
            println!("           send to it. Claim it first, then subscribe:");
            println!("             agent-msg-bus register {addr}");
            println!("             Monitor({{command: \"{watch_cmd}\", persistent: true}})");
        }
        Recommend::UseInstead(peer) => {
            println!("subscribe: NOT to the address above — this session already has a mailbox:");
            println!("             {peer}");
            println!("           Subscribing to the derived name would give you a second, empty one");
            println!("           and split this session's mail across two addresses.");
        }
        Recommend::Unknown => {
            println!("subscribe: unverified — the broker could not be reached, so whether this");
            println!("           address is registered is unknown. The line below is what you would");
            println!("           run if it is; check with `peers` once the broker is back.");
            println!("             Monitor({{command: \"{watch_cmd}\", persistent: true}})");
        }
    }
    for line in agent_msg_bus::identity::advisory(&addr, &status) {
        eprintln!("{line}");
    }

    // Old names that still exist and are not aliased to this one. Nothing else on the bus reports
    // this: the sender is told "queued for a known address", which is true, and the mail sits where
    // nobody listens. Only worth printing when there is something to say.
    if let Ok(p) = &peers {
        let siblings = agent_msg_bus::identity::stranding_siblings(&addr, p);
        if !siblings.is_empty() {
            let unread: usize = siblings.iter().map(|(_, n)| n).sum();
            eprintln!();
            eprintln!("WARNING : {} older name(s) for this address still exist and are NOT aliased", siblings.len());
            eprintln!("          to it. Anything sent there queues where nothing is listening, and the");
            eprintln!("          sender is told it was accepted.");
            for (a, n) in &siblings {
                eprintln!("            {a:<34} {n} unread");
            }
            if unread > 0 {
                eprintln!("          {unread} message(s) are sitting on them right now.");
            }
            eprintln!("          Point them at this address — one command each, mail included:");
            for (a, _) in &siblings {
                eprintln!("            agent-msg-bus migrate {a} {addr}");
            }
        }
    }
}

#[tokio::main]
async fn relay(listen: &str, broker: &str, token: &str) {
    if token.is_empty() {
        eprintln!("agent-msg-bus: no token. Pass --token or set AMB_TOKEN.");
        std::process::exit(1);
    }
    let state = agent_msg_bus::relay::RelayState {
        broker: Arc::new(broker.to_string()),
        token: Arc::new(token.to_string()),
        busy: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
    };

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        // Exit 0, not 1: another relay already serves this port, so there is nothing to do and
        // nothing wrong. This makes a supervisor that re-launches on a timer a safe no-op instead of
        // a restart loop - which is what lets the Scheduled Task carry a repetition trigger.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            println!("agent-msg-bus: {listen} is already served by another relay; nothing to do");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("agent-msg-bus: cannot bind {listen}: {e}");
            std::process::exit(1);
        }
    };
    // The token is deliberately absent from this line: it would otherwise be the one place the value
    // gets printed, and the URL below is what a session copies into a Monitor call. The token stays
    // on the relay's upstream leg and never reaches the session.
    println!("relay listening on {listen} -> broker {broker}");
    println!("subscribe with: ws://{listen}/sub?addr=<your-address>");
    if let Err(e) = axum::serve(listener, agent_msg_bus::relay::app(state)).await {
        eprintln!("agent-msg-bus: relay error: {e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn serve(
    listen: &str,
    db: &str,
    tokens: Option<&str>,
    insecure: bool,
    sweep_minutes: u64,
    provisional_hours: i64,
) {
    let auth = match (tokens, insecure) {
        (Some(path), _) => match Auth::from_file(path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("agent-msg-bus: cannot read tokens file {path}: {e}");
                std::process::exit(1);
            }
        },
        (None, true) => {
            eprintln!("agent-msg-bus: WARNING - running with --insecure-no-auth. Anything that can");
            eprintln!("  reach this port can post as any address and read any mailbox.");
            Auth::disabled()
        }
        (None, false) => {
            eprintln!("agent-msg-bus: refusing to start without --tokens.");
            eprintln!("  Pass --tokens <file> (JSON: {{\"machine\": \"token\"}}),");
            eprintln!("  or --insecure-no-auth if you really mean to run it open.");
            std::process::exit(1);
        }
    };

    let store = match Store::open(db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agent-msg-bus: cannot open database {db}: {e}");
            std::process::exit(1);
        }
    };

    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(auth),
    };

    // The sweeper is what makes `provisional_hours` a behaviour rather than an intention. Without
    // it the value was only reachable through /prune, which nobody runs.
    if sweep_minutes > 0 {
        agent_msg_bus::server::spawn_sweeper(state.clone(), sweep_minutes, provisional_hours);
    }

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-msg-bus: cannot bind {listen}: {e}");
            std::process::exit(1);
        }
    };
    println!("agent-msg-bus listening on {listen}, db {db}");
    if sweep_minutes > 0 {
        println!("sweeping provisional registrations older than {provisional_hours}h every {sweep_minutes}m");
    } else {
        println!("provisional sweeper DISABLED - entries will accumulate until `prune` is run by hand");
    }

    // Graceful shutdown so connected sockets get a proper 1001 close rather than the bare TCP drop
    // that the Phase 0 probe produced (which clients see as 1006 and cannot distinguish from a
    // network fault).
    let served = axum::serve(listener, app(state)).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        println!("agent-msg-bus: shutting down");
    });
    if let Err(e) = served.await {
        eprintln!("agent-msg-bus: server error: {e}");
        std::process::exit(1);
    }
}

fn run_client(cli: &Cli) {
    let (url, token) = resolve_conn(cli);
    let c = Client::new(&url, &token);
    let result: Result<(), Box<dyn std::error::Error>> = match &cli.cmd {
        Cmd::Register { addr, session_id, machine, repo, cwd, pid } => c
            .register(addr, session_id, machine, repo, cwd, *pid)
            .map(|_| println!("registered {addr}"))
            .map_err(Into::into),
        Cmd::Send { from, to, kind, subject, body, body_file, reply_to } => {
            let resolved = match body_file.as_deref() {
                Some("-") => {
                    let mut s = String::new();
                    match std::io::Read::read_to_string(&mut std::io::stdin(), &mut s) {
                        Ok(_) => s,
                        Err(e) => {
                            eprintln!("agent-msg-bus: cannot read body from stdin: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                Some(p) => match std::fs::read_to_string(p) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("agent-msg-bus: cannot read body file {p}: {e}");
                        std::process::exit(1);
                    }
                },
                None => body.clone(),
            };
            // Warn, never refuse: an unknown `--to` already only warns, three sessions are live on
            // this bus, and a send that starts rejecting mid-flight is a worse failure than the one
            // being fixed. The cost of a bad `--from` is not this message - it is every reply to it.
            let peers = c.peers(true).map_err(|e| e.to_string());
            for line in agent_msg_bus::identity::advisory(
                from,
                &agent_msg_bus::identity::classify(from, &peers),
            ) {
                eprintln!("{line}");
            }
            c.send(from, to, kind, subject, &resolved, reply_to)
                .map(|id| {
                    // The id alone goes to stdout, so `ID=$(... send ...)` keeps working.
                    println!("{id}");
                    // The confirmation goes to STDERR, which is what makes it a real safeguard
                    // rather than a documented one. docs/usage.md already told senders the output
                    // "names the recipient — the only signal that would catch a wrong address";
                    // it did not, because only the id was ever printed. Putting it on stderr means
                    // the overwhelmingly common idiom for capturing the id — redirecting stdout —
                    // cannot hide it, which is precisely when a stale `--to` used to slip through.
                    eprintln!("{}", send_confirmation(&id, from, to));
                })
                .map_err(Into::into)
        }
        Cmd::Ack { addr, up_to_id } => {
            c.ack(addr, up_to_id).map(|_| println!("acked {addr} up to {up_to_id}")).map_err(Into::into)
        }
        // Refused before the request goes out, so the caller is corrected rather than handed the
        // whole history in the shape of a filtered read.
        Cmd::Read { addr, since, limit } => {
            match since.as_deref().and_then(agent_msg_bus::store::since_refusal) {
                Some(why) => Err(why.into()),
                None => c
                    .read(addr, since.as_deref(), *limit)
                    .map(|msgs| {
                        if msgs.is_empty() {
                            println!("no stored messages for {addr}");
                        }
                        for m in msgs {
                            println!("{}", "=".repeat(76));
                            println!("id      : {}", m.id);
                            println!("from    : {}  ->  {}   [{}]", m.from, m.to, m.kind);
                            println!("ts      : {}", m.ts);
                            println!("subject : {}", m.subject);
                            println!("{}", "-".repeat(76));
                            println!("{}", m.body);
                        }
                    })
                    .map_err(Into::into),
            }
        }
        Cmd::Prune { days, provisional_hours, yes } => c
            .prune(*days, *provisional_hours, !*yes)
            .map(|addrs| {
                if addrs.is_empty() {
                    println!("nothing to prune (offline and unseen for more than {days} days)");
                } else if *yes {
                    for a in &addrs {
                        println!("forgot {a}");
                    }
                } else {
                    println!("would forget {} address(es) — re-run with --yes:", addrs.len());
                    for a in &addrs {
                        println!("  {a}");
                    }
                }
            })
            .map_err(Into::into),
        Cmd::Migrate { from, to } => c
            .migrate(from, to)
            .map(|(pending, cursor)| {
                println!("{to} now also answers to {from}");
                println!("  adopted cursor : {}", if cursor.is_empty() { "(beginning)" } else { &cursor });
                println!("  pending now    : {pending}");
                if cursor.is_empty() {
                    println!("  note: this mailbox starts from the beginning, so mail already read");
                    println!("        under another name will be delivered again. A replayed frame");
                    println!("        carries \"replay\": true. Acking under either name now advances");
                    println!("        the same cursor.");
                }
                if pending == 0 {
                    println!("  note: nothing was waiting for {from} — the migration is still in");
                    println!("        effect for anything sent to that name from now on.");
                }
                println!("{to} is now a registered address. Mail for it queues in the broker; read");
                println!("it with `read {to}`. Nothing needs arming.");
            })
            .map_err(Into::into),
        Cmd::Pin { addr } => match agent_msg_bus::hook::set_pin(addr) {
            Ok(p) => {
                println!("pinned this session to {addr}");
                println!("  {}", p.display());
                println!("Re-run the SessionStart hook or `whoami` to see it take effect.");
                println!("Mail for the new address queues in the broker; nothing needs arming.");
                Ok(())
            }
            Err(e) => Err(e.into()),
        },
        Cmd::Unpin => match agent_msg_bus::hook::clear_pin() {
            Ok(true) => {
                println!("pin cleared; back to the derived address");
                Ok(())
            }
            Ok(false) => {
                println!("no pin was set");
                Ok(())
            }
            Err(e) => Err(e.into()),
        },
        Cmd::Orphans { limit, delete } => match delete {
            Some(id) => c
                .delete_orphan(id)
                .map(|d| println!("{}", if d { format!("deleted {id}") } else { "nothing deleted".into() }))
                .map_err(Into::into),
            None => c
                .orphans(*limit)
                .map(|ms| {
                    if ms.is_empty() {
                        println!("no orphaned mail");
                        return;
                    }
                    println!("{} orphaned message(s) - addressed to something that does not exist:", ms.len());
                    for m in ms {
                        println!("  {}  {} -> {}", m.id, m.from, m.to);
                        println!("      {}", m.subject);
                    }
                    println!("clear one with: agent-msg-bus orphans --delete <id>");
                })
                .map_err(Into::into),
        },
        Cmd::Forget { addr } => c
            .forget(addr)
            .map(|existed| {
                if existed {
                    println!("forgot {addr}")
                } else {
                    println!("{addr} was not registered")
                }
            })
            .map_err(Into::into),
        Cmd::Peers { all } => c
            .peers(*all)
            .map(|p| {
                for k in &p.known {
                    let alias = if k.aliases.is_empty() {
                        String::new()
                    } else {
                        format!("  (also answers to {})", k.aliases.join(", "))
                    };
                    // A blank version is an address that registered before versions were on the
                    // wire. Rendered as `?` rather than left empty, so the column reads as
                    // "unknown" rather than as a formatting glitch.
                    let ver = if k.version.is_empty() { "?" } else { k.version.as_str() };
                    println!(
                        "{:<32} {:<8} {:>3} pending  {:<8} {}{}",
                        k.addr,
                        if k.live { "live" } else { "offline" },
                        k.pending,
                        ver,
                        k.repo,
                        alias
                    );
                }
                if p.known.is_empty() {
                    println!("no addresses have joined the bus yet");
                    println!("(sessions that registered but never subscribed are hidden; --all shows them)");
                } else {
                    // The version column only means something next to the build it is compared with.
                    println!();
                    println!("this client: {}", agent_msg_bus::VERSION);
                    if let Some(v) = c.broker_version() {
                        println!("broker     : {v}");
                    }
                    let behind: Vec<&str> = p
                        .known
                        .iter()
                        .filter(|k| !k.version.is_empty() && k.version != agent_msg_bus::VERSION)
                        .map(|k| k.addr.as_str())
                        .collect();
                    if !behind.is_empty() {
                        println!("on a different build: {}", behind.join(", "));
                    }
                }
            })
            .map_err(Into::into),
        Cmd::SubUrl { addr } => {
            println!("{}", c.sub_url(addr));
            Ok(())
        }
        Cmd::Serve { .. } | Cmd::Relay { .. } | Cmd::SessionStart | Cmd::Whoami
        | Cmd::Watch { .. } | Cmd::Update { .. } | Cmd::SelfUpdate { .. } => {
            unreachable!("handled in main")
        }
    };
    if let Err(e) = result {
        eprintln!("agent-msg-bus: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{send_confirmation, Cli};
    use clap::CommandFactory;

    /// docs/usage.md tells senders that the send's output "names the recipient - the only signal
    /// that would catch a wrong address". It did not: `send` printed the message id and nothing
    /// else. A safeguard that is documented and absent is worse than one that is merely absent,
    /// because people stop looking for the thing it was supposed to catch — and the misaddressed
    /// send it was meant to catch has now happened three times in a week, twice between the same
    /// two sessions, each time surviving review because everything except the header was correct.
    #[test]
    fn the_send_confirmation_names_the_recipient() {
        let c = send_confirmation("20260906T1-1", "machine-a/a", "machine-a/b");
        assert!(c.contains("machine-a/b"), "confirmation does not name the recipient: {c}");
    }

    /// The sender too, because the other half of this failure is a stale `--from`: four messages
    /// once went out with an unroutable return address and every reply to them bounced.
    #[test]
    fn the_send_confirmation_names_the_sender_and_the_id() {
        let c = send_confirmation("20260906T1-1", "machine-a/a", "machine-a/b");
        assert!(c.contains("machine-a/a"), "no sender: {c}");
        assert!(c.contains("20260906T1-1"), "no id: {c}");
    }

    /// Direction must be unambiguous. "machine-a/a machine-a/b" would name both and still let a reader who
    /// glances at it mistake which is which, which is the exact mistake being guarded against.
    #[test]
    fn the_send_confirmation_shows_the_direction() {
        let c = send_confirmation("id", "SENDER", "RECIPIENT");
        let arrow = c.find("->").expect("no direction marker");
        assert!(
            c.find("SENDER").unwrap() < arrow && c.find("RECIPIENT").unwrap() > arrow,
            "sender and recipient are not on the expected sides of the arrow: {c}"
        );
    }

    /// A binary that cannot say which build it is turns every "is the fix actually deployed over
    /// there?" question into a file-hash comparison. Several machines run their own copy of this
    /// one and they are updated at different times, so the version has to be askable at the command
    /// line rather than inferred from a timestamp.
    ///
    /// Asserting it equals `CARGO_PKG_VERSION` is what keeps `--version`, `Cargo.toml` and the
    /// release tag from drifting: bump one and this test is the thing that notices.
    #[test]
    fn the_binary_reports_its_own_version() {
        assert_eq!(
            Cli::command().get_version().map(|s| s.to_string()).as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "`--version` is not wired to the crate version"
        );
    }

    /// clap only builds the command lazily, so a malformed argument definition is a runtime panic
    /// in whatever subcommand happens to be invoked first - on a user's machine, not here.
    #[test]
    fn the_argument_definitions_are_internally_consistent() {
        Cli::command().debug_assert();
    }

    fn arg_help(sub: &str, arg: &str) -> String {
        let cmd = Cli::command();
        let sub = cmd
            .get_subcommands()
            .find(|s| s.get_name() == sub)
            .unwrap_or_else(|| panic!("no `{sub}` subcommand"));
        let a = sub
            .get_arguments()
            .find(|a| a.get_id() == arg)
            .unwrap_or_else(|| panic!("`{sub}` has no --{arg}"));
        a.get_help().map(|h| h.to_string()).unwrap_or_default()
    }

    /// `--since` took a message id and said so nowhere, so a timestamp looked like a reasonable
    /// guess - and a timestamp is accepted by text comparison against every row. The refusal is the
    /// real fix; the help text is what stops the wrong guess being made in the first place.
    #[test]
    fn the_read_since_argument_says_it_takes_a_message_id() {
        let help = arg_help("read", "since");
        assert!(help.contains("message id"), "--since does not say what it takes: {help:?}");
        assert!(help.contains("NOT a timestamp"), "--since does not rule out a timestamp: {help:?}");
    }

    /// A rollback is a real operation, so the downgrade guard must have a documented way through.
    #[test]
    fn update_has_a_force_flag_for_a_deliberate_downgrade() {
        let help = arg_help("update", "force");
        assert!(help.to_lowercase().contains("older"), "--force does not say what it permits: {help:?}");
    }
}
