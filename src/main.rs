//! One binary, both halves: `serve` runs the broker, everything else is the client a session calls.
//!
//! Single binary because it deploys to a CT and to three client machines, and two artefacts is two
//! things to keep in step. The old bus drifted precisely that way — two hand-synced copies of the
//! same scripts, kept aligned only by whoever remembered to copy one to the other.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use clap::{Parser, Subcommand};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "agent-msg-bus", about = "Push-delivery message bus for Claude Code sessions")]
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
        #[arg(long, default_value = "")]
        reply_to: String,
    },
    /// Confirm messages up to and including this id have been handled.
    Ack { addr: String, up_to_id: String },
    /// Who is on the bus.
    Peers,
    /// Retire an address (drops its registration and cursor, not its message history).
    Forget { addr: String },
    /// Read stored messages for an address. Does NOT consume them or move the cursor.
    ///
    /// Use this to recover a message that arrived truncated in a notification — delivery is
    /// push-only, so without this the full text of a long message was unrecoverable once acked.
    Read {
        addr: String,
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
        Cmd::Serve { listen, db, tokens, insecure_no_auth } => {
            serve(&listen, &db, tokens.as_deref(), insecure_no_auth)
        }
        Cmd::Relay { ref listen } => {
            let listen = listen.clone();
            let (url, token) = resolve_conn(&cli);
            relay(&listen, &url, &token)
        }
        Cmd::SessionStart => agent_msg_bus::hook::run(),
        Cmd::Whoami => whoami(),
        _ => run_client(&cli),
    }
}

fn whoami() {
    let Some(cfg) = agent_msg_bus::hook::load_config() else {
        eprintln!(
            "agent-msg-bus: no config at {}",
            agent_msg_bus::hook::config_path().display()
        );
        std::process::exit(1);
    };
    let session = std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
    let addr = agent_msg_bus::hook::derive_address(&cfg.machine, &cwd, &session);
    println!("address : {addr}");
    println!("broker  : {}", cfg.url);
    println!("relay   : {}", cfg.relay);
    println!(
        "subscribe: ws://{}/sub?addr={}",
        cfg.relay,
        agent_msg_bus::client::urlencode(&addr)
    );
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
async fn serve(listen: &str, db: &str, tokens: Option<&str>, insecure: bool) {
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

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-msg-bus: cannot bind {listen}: {e}");
            std::process::exit(1);
        }
    };
    println!("agent-msg-bus listening on {listen}, db {db}");

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
        Cmd::Send { from, to, kind, subject, body, reply_to } => {
            c.send(from, to, kind, subject, body, reply_to).map(|id| println!("{id}")).map_err(Into::into)
        }
        Cmd::Ack { addr, up_to_id } => {
            c.ack(addr, up_to_id).map(|_| println!("acked {addr} up to {up_to_id}")).map_err(Into::into)
        }
        Cmd::Read { addr, since, limit } => c
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
        Cmd::Prune { days, yes } => c
            .prune(*days, !*yes)
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
        Cmd::Peers => c
            .peers()
            .map(|p| {
                for k in &p.known {
                    println!(
                        "{:<32} {:<8} {:>3} pending  {}",
                        k.addr,
                        if k.live { "live" } else { "offline" },
                        k.pending,
                        k.repo
                    );
                }
                if p.known.is_empty() {
                    println!("no addresses registered");
                }
            })
            .map_err(Into::into),
        Cmd::SubUrl { addr } => {
            println!("{}", c.sub_url(addr));
            Ok(())
        }
        Cmd::Serve { .. } | Cmd::Relay { .. } | Cmd::SessionStart | Cmd::Whoami => {
            unreachable!("handled in main")
        }
    };
    if let Err(e) = result {
        eprintln!("agent-msg-bus: {e}");
        std::process::exit(1);
    }
}
