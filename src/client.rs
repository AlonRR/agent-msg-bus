//! Client half of the single binary: what a session actually calls.
//!
//! Blocking HTTP on purpose. The client path needs no async runtime, and a session invokes these
//! one command at a time from a shell.

use serde::Deserialize;

pub struct Client {
    base: String,
    token: String,
}

#[derive(Debug)]
pub enum ClientError {
    Http(String),
    Status(u16, String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(e) => write!(f, "{e}"),
            // The status and the server's own reason are both surfaced. A rejected token must never
            // look like an idle bus - that indistinguishability is what let the old system sit
            // broken for five days.
            ClientError::Status(c, b) => write!(f, "HTTP {c}: {b}"),
        }
    }
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
        Client { base: base.trim_end_matches('/').to_string(), token: token.to_string() }
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, ClientError> {
        match ureq::post(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body)
        {
            Ok(r) => r.into_json().map_err(|e| ClientError::Http(e.to_string())),
            Err(ureq::Error::Status(code, r)) => {
                Err(ClientError::Status(code, r.into_string().unwrap_or_default()))
            }
            Err(e) => Err(ClientError::Http(e.to_string())),
        }
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        match ureq::get(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
        {
            Ok(r) => r.into_json().map_err(|e| ClientError::Http(e.to_string())),
            Err(ureq::Error::Status(code, r)) => {
                Err(ClientError::Status(code, r.into_string().unwrap_or_default()))
            }
            Err(e) => Err(ClientError::Http(e.to_string())),
        }
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
        self.post(
            "/register",
            serde_json::json!({
                "addr": addr, "session_id": session_id, "machine": machine,
                "repo": repo, "cwd": cwd, "pid": pid
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
