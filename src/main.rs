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
    /// Print the ws:// URL to hand to Monitor.
    SubUrl { addr: String },
    /// Hold the broker connection on this machine and re-serve it on loopback.
    ///
    /// Monitor refuses to open a WebSocket to a private IP, so it cannot reach the broker directly;
    /// it can reach 127.0.0.1. Run one of these per session address.
    Relay {
        addr: String,
        #[arg(long, default_value = "127.0.0.1:9451")]
        listen: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { listen, db, tokens, insecure_no_auth } => {
            serve(&listen, &db, tokens.as_deref(), insecure_no_auth)
        }
        Cmd::Relay { ref addr, ref listen } => {
            let (addr, listen) = (addr.clone(), listen.clone());
            relay(&listen, &cli.url, &addr, &cli.token)
        }
        _ => run_client(&cli),
    }
}

#[tokio::main]
async fn relay(listen: &str, broker: &str, addr: &str, token: &str) {
    if token.is_empty() {
        eprintln!("agent-msg-bus: no token. Pass --token or set AMB_TOKEN.");
        std::process::exit(1);
    }
    let upstream = agent_msg_bus::relay::upstream_url(broker, addr, token);
    let state = agent_msg_bus::relay::RelayState {
        upstream_url: Arc::new(upstream),
        addr: Arc::new(addr.to_string()),
        busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-msg-bus: cannot bind {listen}: {e}");
            std::process::exit(1);
        }
    };
    // The token is deliberately absent from this line: it would otherwise be the one place the
    // value gets printed, and this is what a session copies into a Monitor call.
    println!("relay for {addr} listening on {listen}");
    println!("subscribe with: ws://{listen}/sub");
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
    let c = Client::new(&cli.url, &cli.token);
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
        Cmd::Serve { .. } | Cmd::Relay { .. } => unreachable!("handled in main"),
    };
    if let Err(e) = result {
        eprintln!("agent-msg-bus: {e}");
        std::process::exit(1);
    }
}
