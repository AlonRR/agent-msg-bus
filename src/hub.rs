//! Live connection registry and token auth.
//!
//! **Liveness is socket state.** An address is live if and only if it currently holds a WebSocket
//! here. The old bus inferred liveness from PID existence, which reported dead sessions as live and
//! - on a box with ~25 stale `claude` processes - was a false positive waiting to happen. There is
//! nothing to infer here: the socket is in the map or it is not.

use crate::store::{addr_matches, Message};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Identifies one socket's occupancy of an address, so a departing connection can only ever release
/// *its own* claim.
///
/// The first version guarded release with `tx.is_closed()`, which is never true at the point a
/// connection tears down — its own receiver is still alive — so addresses were never released and a
/// session could not reconnect without `force=1`. An epoch makes the ownership question exact
/// instead of inferred.
pub type ConnId = u64;

pub struct Hub {
    conns: Mutex<HashMap<String, (ConnId, mpsc::UnboundedSender<Message>)>>,
    next_id: AtomicU64,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        Hub { conns: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1) }
    }

    /// Claim `addr` for a new socket.
    ///
    /// Returns `None` if the address is already held by a live socket and `force` is not set. Two
    /// sockets on one address would each consume part of the other's mail - the same class of bug
    /// as the old machine-wide whoami file, where the second session to start silently took over
    /// the first one's identity and read its messages.
    pub fn claim(
        &self,
        addr: &str,
        force: bool,
    ) -> Option<(ConnId, mpsc::UnboundedReceiver<Message>)> {
        let mut conns = self.conns.lock().unwrap();
        if conns.contains_key(addr) && !force {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        // A forced claim overwrites the old sender, which closes the previous socket's receiver and
        // ends that connection.
        conns.insert(addr.to_string(), (id, tx));
        Some((id, rx))
    }

    /// Release `addr`, but only if this connection is still the occupant. A socket that was
    /// force-displaced must not evict its replacement on the way out.
    pub fn release(&self, addr: &str, id: ConnId) {
        let mut conns = self.conns.lock().unwrap();
        if conns.get(addr).map(|(held, _)| *held == id).unwrap_or(false) {
            conns.remove(addr);
        }
    }

    pub fn live(&self) -> Vec<String> {
        let mut v: Vec<String> = self.conns.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_live(&self, addr: &str) -> bool {
        self.conns.lock().unwrap().contains_key(addr)
    }

    /// Push a message to every live address it is addressed to, except the sender.
    ///
    /// Best-effort by design: anything not delivered here is still in the store and replays on the
    /// recipient's next connect. Delivery is never assumed from a successful push - only from an
    /// explicit ack.
    pub fn deliver(&self, msg: &Message) -> usize {
        let conns = self.conns.lock().unwrap();
        let mut sent = 0;
        for (addr, (_, tx)) in conns.iter() {
            if addr != &msg.from && addr_matches(&msg.to, addr) && tx.send(msg.clone()).is_ok() {
                sent += 1;
            }
        }
        sent
    }
}

/// Token -> machine label.
///
/// The token travels in the query string because Monitor's `ws` schema is `{url, protocols}` with
/// no headers field - verified against a real handshake, not assumed. That is acceptable for a
/// LAN-only service and is the specific reason this must not be exposed over WAN unrevisited.
pub struct Auth {
    tokens: HashMap<String, String>,
    disabled: bool,
}

impl Auth {
    pub fn from_file(path: &str) -> std::io::Result<Auth> {
        let raw = std::fs::read_to_string(path)?;
        let map: HashMap<String, String> = serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // file is machine -> token; index by token for lookup
        let tokens = map.into_iter().map(|(machine, token)| (token, machine)).collect();
        Ok(Auth { tokens, disabled: false })
    }

    /// Only ever reached via an explicit `--insecure-no-auth` flag. There is deliberately no way to
    /// end up here by omission: a missing tokens file is a hard startup error, not a silent
    /// downgrade to open access.
    pub fn disabled() -> Auth {
        Auth { tokens: HashMap::new(), disabled: true }
    }

    pub fn check(&self, token: Option<&str>) -> bool {
        if self.disabled {
            return true;
        }
        match token {
            Some(t) => self.tokens.contains_key(t),
            None => false,
        }
    }

    pub fn machine_for(&self, token: &str) -> Option<&str> {
        self.tokens.get(token).map(|s| s.as_str())
    }
}
