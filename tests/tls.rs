//! A broker behind a TLS front, the way it is reached from outside the network it lives on.
//!
//! The broker never speaks TLS itself. A reverse proxy terminates it and forwards to the broker's
//! plain port, and that proxy usually writes every request URL to an access log. So these tests
//! put a real TLS front, with a certificate from a throwaway private root, in front of a real
//! broker, and check three things: that both client legs (the CLI's HTTP and the relay's
//! WebSocket) get through it once that root is named in `ca_file`; that neither leg gets through
//! without it, and says why; and that the token never appears in a request line the front sees.

use agent_msg_bus::client::Client;
use agent_msg_bus::hub::{Auth, Hub};
use agent_msg_bus::relay::RelayState;
use agent_msg_bus::server::{app, AppState};
use agent_msg_bus::store::Store;
use futures_util::StreamExt;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite;

const TOKEN: &str = "tls-test-token-7f3a";
static N: AtomicU32 = AtomicU32::new(0);

struct Front {
    /// `https://127.0.0.1:<port>`: the TLS front, which forwards to the broker.
    base: String,
    /// The private root's PEM, as an operator would save it.
    ca_file: std::path::PathBuf,
    /// Every byte clients sent through the front, after TLS was removed: what an access log sees.
    seen: Arc<Mutex<Vec<u8>>>,
}

fn private_root_and_leaf() -> (String, rustls::ServerConfig) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "test private root");
    let ca_cert = ca.self_signed(&ca_key).unwrap();

    let key = rcgen::KeyPair::generate().unwrap();
    // By IP as well as by name: connecting to 127.0.0.1 avoids `localhost` resolving to ::1 first.
    let leaf = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
        .unwrap()
        .signed_by(&key, &ca_cert, &ca_key)
        .unwrap();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    (ca_cert.pem(), server)
}

async fn start() -> Front {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("amb-tls-it-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    let tokens = dir.join("tokens.json");
    std::fs::write(&tokens, format!(r#"{{"test":"{TOKEN}"}}"#)).unwrap();

    let state = AppState {
        store: Arc::new(Mutex::new(
            Store::open(dir.join("bus.db").to_str().unwrap()).unwrap(),
        )),
        hub: Arc::new(Hub::new()),
        auth: Arc::new(Auth::from_file(tokens.to_str().unwrap()).unwrap()),
    };
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broker = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, app(state)).await.unwrap() });

    let (ca_pem, server) = private_root_and_leaf();
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, ca_pem).unwrap();

    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let fl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = fl.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = fl.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let log = log.clone();
            tokio::spawn(async move {
                // A client that rejects the certificate ends the handshake here. That is a test
                // outcome, not a front failure.
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let upstream = tokio::net::TcpStream::connect(broker).await.unwrap();
                let (mut cr, mut cw) = tokio::io::split(tls);
                let (mut ur, mut uw) = upstream.into_split();
                let to_broker = async move {
                    let mut buf = [0u8; 8192];
                    loop {
                        match cr.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                log.lock().unwrap().extend_from_slice(&buf[..n]);
                                if uw.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = uw.shutdown().await;
                };
                let to_client = async move {
                    let _ = tokio::io::copy(&mut ur, &mut cw).await;
                    let _ = cw.shutdown().await;
                };
                tokio::join!(to_broker, to_client);
            });
        }
    });

    Front {
        base: format!("https://127.0.0.1:{port}"),
        ca_file,
        seen,
    }
}

/// A relay pointed at the front, trusting `ca_file` if given. Returns its loopback port.
async fn relay_to(front: &Front, ca_file: Option<&std::path::Path>) -> u16 {
    let st = RelayState {
        broker: Arc::new(front.base.clone()),
        token: Arc::new(TOKEN.to_string()),
        busy: Arc::new(Mutex::new(HashSet::new())),
        tls: agent_msg_bus::tls::client_config(ca_file).unwrap(),
    };
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = rl.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(rl, agent_msg_bus::relay::app(st))
            .await
            .unwrap()
    });
    port
}

fn client(front: &Front, ca_file: Option<&std::path::Path>) -> Client {
    Client::with_tls(
        &front.base,
        TOKEN,
        Duration::from_secs(10),
        agent_msg_bus::tls::client_config(ca_file).unwrap(),
    )
}

async fn blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap()
}

type Sock =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The next frame that is a message, skipping the relay's own `_relay` status frames.
async fn next_message(sock: &mut Sock, within: Duration) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v.get("_relay").is_none() {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// The request lines and headers the front saw, one string per request.
fn request_heads(seen: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(seen);
    text.split("\r\n\r\n")
        .filter_map(|chunk| {
            let start = ["GET ", "POST "]
                .iter()
                .filter_map(|m| chunk.find(m))
                .min()?;
            Some(chunk[start..].to_string())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_legs_reach_a_broker_behind_a_tls_front_once_its_root_is_named() {
    let front = start().await;
    let ca = front.ca_file.clone();

    // The CLI leg: register over HTTPS.
    let c = client(&front, Some(&ca));
    blocking(move || c.register("m/a", "s1", "m", "repo", "/x", 1).unwrap()).await;

    // The relay leg: a session subscribes on loopback, and the relay holds the upstream over wss.
    let rport = relay_to(&front, Some(&ca)).await;
    let (mut sock, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{rport}/sub?addr=m%2Fa"))
            .await
            .unwrap();

    let c = client(&front, Some(&ca));
    blocking(move || {
        c.send("m/b", "m/a", "fyi", "through the front", "b", "")
            .unwrap()
    })
    .await;
    let got = next_message(&mut sock, Duration::from_secs(10)).await;
    assert_eq!(
        got.as_ref().map(|v| v["subject"].clone()),
        Some(serde_json::json!("through the front")),
        "the message did not arrive through relay and front: {got:?}"
    );

    // What an access log on the front would hold.
    let heads = request_heads(&front.seen.lock().unwrap());
    assert!(
        heads.iter().any(|h| h.starts_with("GET /sub?addr=m%2Fa")),
        "the relay's subscribe never went through the front: {heads:?}"
    );
    for h in &heads {
        let line = h.lines().next().unwrap_or("");
        assert!(
            !line.contains("token="),
            "a request line carries the token: {line}"
        );
        assert!(
            !line.contains(TOKEN),
            "a request line carries the token: {line}"
        );
        assert!(
            h.to_ascii_lowercase().contains(&format!(
                "authorization: bearer {}",
                TOKEN.to_ascii_lowercase()
            )),
            "a request reached the broker without the token header: {line}"
        );
    }
}

/// Without the private root, both legs must fail, and say what to set. A bare `UnknownIssuer`
/// names neither the file nor the setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_root_both_legs_fail_and_name_the_setting_that_fixes_it() {
    let front = start().await;

    let c = client(&front, None);
    let err = blocking(move || c.peers(false).map(|_| ()))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ca_file"),
        "the CLI's TLS failure does not name ca_file: {err}"
    );

    let req = agent_msg_bus::relay::upstream_request(&front.base, "m/a", TOKEN).unwrap();
    let tls = agent_msg_bus::tls::client_config(None).unwrap();
    let err = match agent_msg_bus::relay::connect_upstream(req, tls).await {
        Ok(_) => panic!("the relay connected through a front whose root it does not trust"),
        Err(e) => e,
    };
    assert!(
        err.contains("ca_file"),
        "the relay's TLS failure does not name ca_file: {err}"
    );
}
