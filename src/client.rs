//! Client half of the single binary: what a session actually calls.
//!
//! Blocking HTTP on purpose. The client path needs no async runtime, and a session invokes these
//! one command at a time from a shell.

use serde::Deserialize;
use std::time::Duration;

/// The deadline on every call unless a caller asks for a shorter one. Generous, because a CLI
/// command reading a long history is allowed to take a while; what it must never do is hang.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
    timeout: Duration,
}

#[derive(Debug)]
pub enum ClientError {
    Http(String),
    Status(u16, String),
    /// No answer within the deadline. Kept apart from `Http` because a timed-out request may still
    /// have taken effect at the broker, which a refusal or a dead connection has not.
    Timeout(Duration),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(e) => write!(f, "{e}"),
            // The status and the server's own reason are both surfaced. A rejected token must never
            // look like an idle bus - that indistinguishability is what let the old system sit
            // broken for five days.
            ClientError::Status(c, b) => write!(f, "HTTP {c}: {b}"),
            ClientError::Timeout(t) => write!(f, "no answer within {} ms (timed out)", t.as_millis()),
        }
    }
}

/// Was this a deadline expiring, rather than a refusal or a broken connection? ureq reports a
/// timeout as an I/O error somewhere in the source chain, so walk it.
fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(io.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

impl std::error::Error for ClientError {}

#[derive(Debug, Deserialize)]
pub struct PeersOut {
    pub live: Vec<String>,
    pub known: Vec<KnownPeer>,
}

#[derive(Debug, Deserialize)]
pub struct KnownPeer {
    pub addr: String,
    #[serde(default)]
    pub version: String,
    pub machine: String,
    pub repo: String,
    pub cwd: String,
    pub live: bool,
    pub pending: usize,
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Client {
        Client::with_timeout(base, token, DEFAULT_TIMEOUT)
    }

    /// A client whose every call gives up after `timeout`: connecting, sending and reading the
    /// reply together. The SessionStart hook uses a short one, because Claude Code kills the hook
    /// outright at its own limit and the session then starts with no banner at all.
    pub fn with_timeout(base: &str, token: &str, timeout: Duration) -> Client {
        Client {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
            timeout,
        }
    }

    /// A client that trusts what `tls` trusts. See `crate::tls` and `Client::configured`.
    pub fn with_tls(
        base: &str,
        token: &str,
        timeout: Duration,
        tls: std::sync::Arc<rustls::ClientConfig>,
    ) -> Client {
        Client {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent: ureq::AgentBuilder::new().timeout(timeout).tls_config(tls).build(),
            timeout,
        }
    }

    /// The client every command uses: `ca_file`, when set, is trusted alongside the public roots.
    /// An unusable `ca_file` is an error here, before any request, naming the file.
    pub fn configured(
        base: &str,
        token: &str,
        timeout: Duration,
        ca_file: &str,
    ) -> Result<Client, String> {
        let ca = (!ca_file.is_empty()).then(|| std::path::Path::new(ca_file));
        Ok(Client::with_tls(base, token, timeout, crate::tls::client_config(ca)?))
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, ClientError> {
        let r = self
            .agent
            .post(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body);
        self.finish(r)
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let r = self
            .agent
            .get(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call();
        self.finish(r)
    }

    /// One place that turns ureq's outcome into ours, so a timeout is recognised on every route,
    /// including one that expires while the body is still being read.
    fn finish(
        &self,
        r: Result<ureq::Response, ureq::Error>,
    ) -> Result<serde_json::Value, ClientError> {
        match r {
            Ok(r) => r.into_json().map_err(|e| {
                if is_timeout(&e) {
                    ClientError::Timeout(self.timeout)
                } else {
                    ClientError::Http(e.to_string())
                }
            }),
            Err(ureq::Error::Status(code, r)) => {
                Err(ClientError::Status(code, r.into_string().unwrap_or_default()))
            }
            Err(ureq::Error::Transport(t)) => {
                if is_timeout(&t) {
                    Err(ClientError::Timeout(self.timeout))
                } else {
                    Err(ClientError::Http(crate::tls::with_hint(t.to_string())))
                }
            }
        }
    }

    /// The broker's build, or `None` if it cannot be reached or is too old to report one.
    ///
    /// `None` is a real answer and not an error: every broker built before versions were on the
    /// wire returns a `/health` without the field, and that is exactly the case a version check
    /// most wants to name.
    pub fn broker_version(&self) -> Option<String> {
        self.broker_health()?.get("version")?.as_str().map(|s| s.to_string())
    }

    /// The broker's raw `/health`, or `None` if it could not be reached.
    ///
    /// Kept separate from `broker_version` because the two `None`s mean very different things and
    /// collapsing them loses the more interesting one: a broker that ANSWERS but reports no version
    /// is a pre-0.2.0 broker, which is a fact worth telling someone, while a broker that cannot be
    /// reached is not evidence about its build at all.
    pub fn broker_health(&self) -> Option<serde_json::Value> {
        self.get("/health").ok()
    }

    pub fn register(
        &self,
        addr: &str,
        session_id: &str,
        machine: &str,
        repo: &str,
        cwd: &str,
        pid: i64,
    ) -> Result<(), ClientError> {
        // `version` is attached HERE rather than taken as a parameter, deliberately. It is the build
        // of the binary actually making the request, so no caller can pass a stale or wrong one, and
        // there is no code path that forgets to send it.
        self.post(
            "/register",
            serde_json::json!({
                "addr": addr, "session_id": session_id, "machine": machine,
                "repo": repo, "cwd": cwd, "pid": pid, "version": crate::VERSION
            }),
        )?;
        Ok(())
    }

    pub fn send(
        &self,
        from: &str,
        to: &str,
        kind: &str,
        subject: &str,
        body: &str,
        reply_to: &str,
    ) -> Result<String, ClientError> {
        let v = self.post(
            "/send",
            serde_json::json!({
                "from": from, "to": to, "kind": kind,
                "subject": subject, "body": body, "reply_to": reply_to
            }),
        )?;
        // Surface the broker's explanation when nothing was pushed live, so `pushed_to: 0` stops
        // being mistaken for a delivery failure.
        if let Some(note) = v.get("note").and_then(|x| x.as_str()) {
            eprintln!("agent-msg-bus: {note}");
        }
        Ok(v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string())
    }

    pub fn ack(&self, addr: &str, up_to_id: &str) -> Result<(), ClientError> {
        self.post("/ack", serde_json::json!({"addr": addr, "up_to_id": up_to_id}))?;
        Ok(())
    }

    /// Stored history for an address. Does not consume or advance the cursor.
    pub fn read(
        &self,
        addr: &str,
        since: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::store::Message>, ClientError> {
        let mut path = format!("/messages?addr={}&limit={}", urlencode(addr), limit);
        if let Some(s) = since {
            path.push_str(&format!("&since={}", urlencode(s)));
        }
        let v = self.get(&path)?;
        serde_json::from_value(v.get("messages").cloned().unwrap_or_default())
            .map_err(|e| ClientError::Http(e.to_string()))
    }

    /// What the cursor says is pending, oldest first, and how many are pending in total.
    ///
    /// Refuses a reply that does not confirm the flag was honoured: a broker older than 0.4.23
    /// ignores `unread=1` and returns the full log, which is exactly what this exists to avoid.
    pub fn read_unread(
        &self,
        addr: &str,
        limit: usize,
    ) -> Result<(Vec<crate::store::Message>, usize), ClientError> {
        let v = self.get(&format!("/messages?addr={}&limit={}&unread=1", urlencode(addr), limit))?;
        unread_reply(&v).map_err(ClientError::Http)
    }

    pub fn prune(
        &self,
        days: i64,
        provisional_hours: i64,
        dry_run: bool,
    ) -> Result<Vec<String>, ClientError> {
        let v = self.post(
            "/prune",
            serde_json::json!({
                "older_than_days": days,
                "provisional_hours": provisional_hours,
                "dry_run": dry_run
            }),
        )?;
        let key = if dry_run { "would_forget" } else { "forgot" };
        Ok(v.get(key)
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default())
    }

    /// Make `to` answer to `from` as well, and inherit its reading position.
    pub fn migrate(&self, from: &str, to: &str) -> Result<(usize, String), ClientError> {
        let v = self.post("/migrate", serde_json::json!({"from": from, "to": to}))?;
        Ok((
            v.get("pending_now").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
            v.get("adopted_cursor").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        ))
    }

    /// Mail addressed to something no registration answers to.
    pub fn orphans(&self, limit: usize) -> Result<Vec<crate::store::Message>, ClientError> {
        let v = self.get(&format!("/orphans?limit={limit}"))?;
        serde_json::from_value(v.get("orphans").cloned().unwrap_or_default())
            .map_err(|e| ClientError::Http(e.to_string()))
    }

    pub fn delete_orphan(&self, id: &str) -> Result<bool, ClientError> {
        let v = self.post("/orphans/delete", serde_json::json!({"id": id}))?;
        Ok(v.get("deleted").and_then(|x| x.as_bool()).unwrap_or(false))
    }

    pub fn forget(&self, addr: &str) -> Result<bool, ClientError> {
        let v = self.post("/forget", serde_json::json!({"addr": addr}))?;
        // Retiring an address strands anything still queued for it. Say so at the moment it happens.
        if let Some(note) = v.get("note").and_then(|x| x.as_str()) {
            eprintln!("agent-msg-bus: {note}");
        }
        Ok(v.get("forgotten").and_then(|x| x.as_bool()).unwrap_or(false))
    }

    pub fn peers(&self, all: bool) -> Result<PeersOut, ClientError> {
        let v = self.get(if all { "/peers?all=1" } else { "/peers" })?;
        serde_json::from_value(v).map_err(|e| ClientError::Http(e.to_string()))
    }

    /// The URL a session hands to `Monitor({ws: …})`.
    ///
    /// `ws://`, not `wss://`: this is a LAN-only service in v1, and whether Monitor's WS client
    /// accepts a certificate from the homelab's internal CA is **untested**. Do not switch this to
    /// `wss://` on the assumption that it works.
    pub fn sub_url(&self, addr: &str) -> String {
        let host = self.base.strip_prefix("http://").unwrap_or(&self.base);
        let host = host.strip_prefix("https://").unwrap_or(host);
        format!("ws://{}/sub?addr={}&token={}", host, urlencode(addr), urlencode(&self.token))
    }
}

/// Accept a `/messages?unread=1` reply only if the broker says it honoured the flag.
fn unread_reply(v: &serde_json::Value) -> Result<(Vec<crate::store::Message>, usize), String> {
    if v.get("unread").and_then(|x| x.as_bool()) != Some(true) {
        return Err("this broker does not serve --unread (it needs 0.4.23 or later) and returned \
                    the full log instead, which would be printed as unread. `peers` shows the \
                    unread count; `read <addr> --since <the id you last acked>` lists what came \
                    after it."
            .to_string());
    }
    let msgs = serde_json::from_value(v.get("messages").cloned().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    Ok((msgs, total))
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod unread_reply_tests {
    use super::unread_reply;

    /// A broker older than 0.4.23 does not know `unread=1` and answers with the address's whole
    /// log. Printing that as "unread" would be the exact confusion `--unread` exists to remove,
    /// so the client refuses unless the broker says it honoured the flag.
    #[test]
    fn a_reply_that_did_not_honour_unread_is_refused_not_printed() {
        let old = serde_json::json!({"messages": []});
        let err = unread_reply(&old).unwrap_err();
        assert!(err.contains("--unread"), "does not name the flag: {err}");
        assert!(err.contains("full log"), "does not say what the broker returned instead: {err}");
    }

    #[test]
    fn an_honoured_reply_yields_the_messages_and_the_total() {
        let new = serde_json::json!({"messages": [], "unread": true, "total": 7});
        let (msgs, total) = unread_reply(&new).unwrap();
        assert!(msgs.is_empty());
        assert_eq!(total, 7);
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::{Client, ClientError};
    use std::time::{Duration, Instant};

    /// Reported 7 Oct 2026: the SessionStart hook's broker calls had no deadline, so a broker that
    /// accepted the connection and then stalled held the hook until Claude Code killed it at 15 s,
    /// and the session started with no banner at all. A stall has to come back as a timeout, fast,
    /// and be distinguishable from a refusal - a timed-out register may still have landed.
    #[test]
    fn a_broker_that_accepts_and_never_answers_is_a_timeout_not_a_hang() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept and hold every connection, never writing a byte.
            let mut held = Vec::new();
            for s in listener.incoming() {
                held.push(s);
            }
        });
        let c = Client::with_timeout(&format!("http://{addr}"), "", Duration::from_millis(300));
        let t0 = Instant::now();
        let err = c.register("machine-a/x", "", "machine-a", "", "", 0).unwrap_err();
        assert!(t0.elapsed() < Duration::from_secs(3), "the stall was not cut short: {:?}", t0.elapsed());
        assert!(matches!(err, ClientError::Timeout(_)), "a stall was not reported as a timeout: {err}");
    }
}
