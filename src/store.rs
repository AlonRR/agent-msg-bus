//! Durable message store.
//!
//! Delivery is **cursor-based**: a message is stored once with its `to` pattern, and each address
//! keeps a cursor. Pending = messages matching the pattern with `id > cursor`. Ack advances the
//! cursor. This is the shape the old file bus used, moved server-side where it is transactional and
//! testable instead of being spread across PowerShell, bash and a sync engine.
//!
//! The broker is the **only writer**. That alone removes the failure that cost the old bus real
//! messages: two processes appending to one file both seeking to the same end-of-file offset, each
//! reporting success, one silently overwriting the other.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// Monotonic tiebreaker so two messages minted in the same millisecond still sort deterministically.
static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub ts: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub subject: String,
    pub body: String,
    pub reply_to: String,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub subject: String,
    pub body: String,
    pub reply_to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Registration {
    pub addr: String,
    pub session_id: String,
    pub machine: String,
    pub repo: String,
    pub cwd: String,
    pub pid: i64,
}

/// Does `addr` receive a message addressed to `pattern`?
///
/// `*` matches any run of characters, so `machine-a/*` reaches every address on machine-a and `*/*.photos`
/// reaches the photos session wherever it runs.
pub fn addr_matches(pattern: &str, addr: &str) -> bool {
    fn walk(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => (0..=s.len()).any(|i| walk(&p[1..], &s[i..])),
            Some(c) => !s.is_empty() && *c == s[0] && walk(&p[1..], &s[1..]),
        }
    }
    walk(pattern.as_bytes(), addr.as_bytes())
}

/// `<compact utc>-<counter>`, so ids sort lexicographically **and** chronologically, and cursors can
/// be plain string comparisons. The counter breaks ties inside one millisecond; it is process-wide,
/// which is sound because the broker is the only writer. The PRIMARY KEY is the backstop: a
/// collision would fail the insert loudly rather than overwrite a message silently.
fn mint_id() -> (String, String) {
    let now = time::OffsetDateTime::now_utc();
    let id = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}{:03}-{:09}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let ts = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    );
    (id, ts)
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        let conn = Connection::open(path)?;
        // WAL so readers never block the writer. busy_timeout turns lock contention into a short
        // wait instead of an error - the old bus needed a hand-rolled retry loop for this, and the
        // retry was written as `catch [IO.IOException]`, which PowerShell never matched, so it
        // never fired.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS messages(
               id TEXT PRIMARY KEY,
               ts TEXT NOT NULL,
               sender TEXT NOT NULL,
               recipient TEXT NOT NULL,
               kind TEXT NOT NULL,
               subject TEXT NOT NULL,
               body TEXT NOT NULL,
               reply_to TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS cursors(
               addr TEXT PRIMARY KEY,
               up_to TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS registry(
               addr TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               machine TEXT NOT NULL,
               repo TEXT NOT NULL,
               cwd TEXT NOT NULL,
               pid INTEGER NOT NULL,
               registered_at TEXT NOT NULL
             );",
        )?;
        Ok(Store { conn })
    }

    /// Claim an address. A **new** address starts its cursor at the current head, so it does not
    /// receive history; a **returning** address keeps its existing cursor, so a session that
    /// crashed still gets the mail it never read.
    pub fn register(&self, reg: &Registration) -> Result<()> {
        let (_, ts) = mint_id();
        self.conn.execute(
            "INSERT OR REPLACE INTO registry
               (addr, session_id, machine, repo, cwd, pid, registered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![reg.addr, reg.session_id, reg.machine, reg.repo, reg.cwd, reg.pid, ts],
        )?;
        let head: String = self
            .conn
            .query_row("SELECT COALESCE(MAX(id), '') FROM messages", [], |r| r.get(0))?;
        // OR IGNORE is what distinguishes "new" from "merely disconnected": a returning address
        // keeps whatever cursor it had, so its unread mail survives.
        self.conn.execute(
            "INSERT OR IGNORE INTO cursors(addr, up_to) VALUES (?1, ?2)",
            params![reg.addr, head],
        )?;
        Ok(())
    }

    pub fn send(&self, m: &NewMessage) -> Result<String> {
        let (id, ts) = mint_id();
        self.conn.execute(
            "INSERT INTO messages(id, ts, sender, recipient, kind, subject, body, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, ts, m.from, m.to, m.kind, m.subject, m.body, m.reply_to],
        )?;
        Ok(id)
    }

    /// Undelivered messages for `addr`, oldest first. Idempotent until acked.
    pub fn pending_for(&self, addr: &str) -> Result<Vec<Message>> {
        let cursor: String = self
            .conn
            .query_row("SELECT up_to FROM cursors WHERE addr = ?1", params![addr], |r| r.get(0))
            .unwrap_or_default();

        let mut stmt = self.conn.prepare(
            "SELECT id, ts, sender, recipient, kind, subject, body, reply_to
               FROM messages
              WHERE id > ?1 AND sender <> ?2
              ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![cursor, addr], |r| {
            Ok(Message {
                id: r.get(0)?,
                ts: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
                kind: r.get(4)?,
                subject: r.get(5)?,
                body: r.get(6)?,
                reply_to: r.get(7)?,
            })
        })?;

        // Pattern matching stays in Rust rather than SQL GLOB: one implementation, directly unit
        // tested, with no dependency on SQLite's glob dialect quirks.
        let mut out = Vec::new();
        for row in rows {
            let m = row?;
            if addr_matches(&m.to, addr) {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Read a single stored message back. Used by `/send` so the pushed frame is the row that was
    /// actually persisted, rather than a reconstruction of it — if the two ever disagreed, the
    /// recipient would see something no replay could reproduce.
    pub fn by_id(&self, id: &str) -> Result<Option<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, sender, recipient, kind, subject, body, reply_to
               FROM messages WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |r| {
            Ok(Message {
                id: r.get(0)?,
                ts: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
                kind: r.get(4)?,
                subject: r.get(5)?,
                body: r.get(6)?,
                reply_to: r.get(7)?,
            })
        })?;
        rows.next().transpose()
    }

    /// Advance the cursor. Never moves backwards.
    pub fn ack(&self, addr: &str, up_to: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cursors(addr, up_to) VALUES (?1, ?2)
             ON CONFLICT(addr) DO UPDATE SET up_to = ?2 WHERE ?2 > cursors.up_to",
            params![addr, up_to],
        )?;
        Ok(())
    }

    /// Stored history for an address, independent of the cursor.
    ///
    /// Delivery is push-only, and a notification can be truncated by whatever renders it. Without
    /// this there was no way to recover the rest of a long message: the only copy a session could
    /// reach was the undelivered one, so replaying it required *not* having acked, and after an ack
    /// it was unrecoverable. Long messages were effectively lossy. This reads the stored rows, so
    /// acked or not makes no difference.
    ///
    /// Unlike `pending_for`, this deliberately ignores the cursor and does **not** advance it.
    pub fn history(&self, addr: &str, since: Option<&str>, limit: usize) -> Result<Vec<Message>> {
        let since = since.unwrap_or("");
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, sender, recipient, kind, subject, body, reply_to
               FROM messages
              WHERE id > ?1
              ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(Message {
                id: r.get(0)?,
                ts: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
                kind: r.get(4)?,
                subject: r.get(5)?,
                body: r.get(6)?,
                reply_to: r.get(7)?,
            })
        })?;
        // Sent-or-received: a session recovering a conversation wants both halves, not just inbound.
        let mut out = Vec::new();
        for row in rows {
            let m = row?;
            if addr_matches(&m.to, addr) || m.from == addr {
                out.push(m);
            }
        }
        if out.len() > limit {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }

    /// Registrations with no live socket that have not re-registered since `cutoff_ts`.
    ///
    /// The registry grows once per session-directory combination and nothing ever removes an entry,
    /// so it accrues dead addresses. That matters for two reasons beyond tidiness: a wildcard send
    /// fans out to every dead address (each accumulating pending that nothing will ever ack), and a
    /// dead address that is later reclaimed keeps its old cursor and gets flooded with backlog —
    /// the exact behaviour the "a new address gets no history" rule exists to prevent.
    ///
    /// Liveness is not knowable here; the caller filters on the hub.
    pub fn stale_registrations(&self, cutoff_ts: &str) -> Result<Vec<Registration>> {
        let mut stmt = self.conn.prepare(
            "SELECT addr, session_id, machine, repo, cwd, pid
               FROM registry WHERE registered_at < ?1 ORDER BY addr",
        )?;
        let rows = stmt.query_map(params![cutoff_ts], |r| {
            Ok(Registration {
                addr: r.get(0)?,
                session_id: r.get(1)?,
                machine: r.get(2)?,
                repo: r.get(3)?,
                cwd: r.get(4)?,
                pid: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Retire an address: drop its registration and its cursor.
    ///
    /// Messages already sent to it stay in `messages` — this removes the *identity*, not history.
    /// Needed because a bad registration would otherwise sit in `peers` forever, and a stale
    /// address that looks live is the kind of thing that gets trusted later.
    pub fn forget(&self, addr: &str) -> Result<bool> {
        let n = self.conn.execute("DELETE FROM registry WHERE addr = ?1", params![addr])?;
        self.conn.execute("DELETE FROM cursors WHERE addr = ?1", params![addr])?;
        Ok(n > 0)
    }

    pub fn peers(&self) -> Result<Vec<Registration>> {
        let mut stmt = self
            .conn
            .prepare("SELECT addr, session_id, machine, repo, cwd, pid FROM registry ORDER BY addr")?;
        let rows = stmt.query_map([], |r| {
            Ok(Registration {
                addr: r.get(0)?,
                session_id: r.get(1)?,
                machine: r.get(2)?,
                repo: r.get(3)?,
                cwd: r.get(4)?,
                pid: r.get(5)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(":memory:").unwrap()
    }

    fn reg(addr: &str) -> Registration {
        Registration {
            addr: addr.into(),
            session_id: format!("sess-{addr}"),
            machine: "machine-a".into(),
            repo: "homelab".into(),
            cwd: "C:/x".into(),
            pid: 1,
        }
    }

    fn msg(from: &str, to: &str, subject: &str) -> NewMessage {
        NewMessage {
            from: from.into(),
            to: to.into(),
            kind: "fyi".into(),
            subject: subject.into(),
            body: "b".into(),
            reply_to: String::new(),
        }
    }

    // ---- addressing -------------------------------------------------------

    #[test]
    fn exact_address_matches() {
        assert!(addr_matches("machine-a/homelab", "machine-a/homelab"));
        assert!(!addr_matches("machine-a/homelab", "machine-a/machine-a"));
    }

    #[test]
    fn wildcards_match_across_device_and_role() {
        assert!(addr_matches("machine-a/*", "machine-a/homelab.loop"));
        assert!(addr_matches("*/*.photos", "machine-b/notes.photos"));
        assert!(addr_matches("machine-a/machine-a.*", "machine-a/machine-a.fixes"));
        assert!(!addr_matches("machine-a/*", "machine-b/notes"));
    }

    // ---- delivery ---------------------------------------------------------

    #[test]
    fn delivers_a_message_to_its_recipient() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/b", "hello")).unwrap();

        let pending = s.pending_for("machine-a/b").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].subject, "hello");
    }

    #[test]
    fn sender_never_receives_its_own_broadcast() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/*", "broadcast")).unwrap();

        assert_eq!(s.pending_for("machine-a/a").unwrap().len(), 0, "sender got its own broadcast");
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 1);
    }

    #[test]
    fn ack_advances_the_cursor() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        let id = s.send(&msg("machine-a/a", "machine-a/b", "one")).unwrap();
        s.ack("machine-a/b", &id).unwrap();
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 0);
    }

    #[test]
    fn ack_never_moves_the_cursor_backwards() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        let first = s.send(&msg("machine-a/a", "machine-a/b", "one")).unwrap();
        let second = s.send(&msg("machine-a/a", "machine-a/b", "two")).unwrap();

        s.ack("machine-a/b", &second).unwrap();
        s.ack("machine-a/b", &first).unwrap(); // late/duplicate ack must not resurrect `two`
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 0);
    }

    /// At-least-once, not at-most-once. The old bus consumed on read, so a crash between reading
    /// and displaying destroyed the message - that is how a reply from machine-a/homelab was lost.
    #[test]
    fn unacked_messages_replay() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/b", "one")).unwrap();

        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 1);
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 1, "reading consumed the message");
    }

    // ---- history and offline ----------------------------------------------

    /// The old bus flooded a watcher with the entire backlog on first attach. A never-before-seen
    /// address starts at the head instead.
    #[test]
    fn a_brand_new_address_does_not_receive_history() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/*", "before anyone joined")).unwrap();

        s.register(&reg("machine-a/latecomer")).unwrap();
        assert_eq!(s.pending_for("machine-a/latecomer").unwrap().len(), 0);
    }

    /// The counterpart: a known address that was merely disconnected keeps its cursor, so mail sent
    /// while it was offline is waiting when it returns. This is what makes machine-b workable on a
    /// LAN-only bus.
    #[test]
    fn a_returning_address_still_gets_mail_sent_while_it_was_away() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-b/notes")).unwrap();

        s.send(&msg("machine-a/a", "machine-b/notes", "while you were out")).unwrap();
        s.register(&reg("machine-b/notes")).unwrap(); // reconnect: re-register, same address

        let pending = s.pending_for("machine-b/notes").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].subject, "while you were out");
    }

    // ---- robustness --------------------------------------------------------

    /// The invariant that caught the worst bug in the old bus: 40 concurrent writers all exited 0,
    /// all printed "sent", and produced 35 lines. A single-writer smoke test passes happily on
    /// broken code, so this test exists specifically to not be that.
    #[test]
    fn concurrent_sends_lose_nothing() {
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("amb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("concurrent.db");
        let _ = std::fs::remove_file(&path);
        let path = path.to_string_lossy().to_string();

        {
            let s = Store::open(&path).unwrap();
            s.register(&reg("machine-a/a")).unwrap();
            s.register(&reg("machine-a/b")).unwrap();
        }

        const WRITERS: usize = 100;
        let path = Arc::new(path);
        let mut handles = Vec::new();
        for i in 0..WRITERS {
            let p = Arc::clone(&path);
            handles.push(std::thread::spawn(move || {
                let s = Store::open(&p).unwrap();
                s.send(&msg("machine-a/a", "machine-a/b", &format!("m{i}"))).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let s = Store::open(&path).unwrap();
        let pending = s.pending_for("machine-a/b").unwrap();
        assert_eq!(pending.len(), WRITERS, "messages were lost under concurrency");

        let mut ids: Vec<_> = pending.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), WRITERS, "duplicate ids minted under concurrency");
    }

    #[test]
    fn pending_is_ordered_oldest_first() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        for i in 0..10 {
            s.send(&msg("machine-a/a", "machine-a/b", &format!("m{i}"))).unwrap();
        }
        let pending = s.pending_for("machine-a/b").unwrap();
        let subjects: Vec<_> = pending.iter().map(|m| m.subject.as_str()).collect();
        assert_eq!(subjects[0], "m0");
        assert_eq!(subjects[9], "m9");
    }

    /// The gap this closes: delivery is push-only, so a notification truncated by whatever renders
    /// it left no way to recover the rest. The only reachable copy was the *undelivered* one, so
    /// replay required not having acked — and after an ack the text was gone for good.
    #[test]
    fn history_is_readable_after_acking_and_does_not_move_the_cursor() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        let id = s.send(&msg("machine-a/a", "machine-a/b", "a long message")).unwrap();

        s.ack("machine-a/b", &id).unwrap();
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 0, "precondition: acked");

        let h = s.history("machine-a/b", None, 20).unwrap();
        assert_eq!(h.len(), 1, "acked message was unrecoverable");
        assert_eq!(h[0].subject, "a long message");
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 0, "history advanced the cursor");
    }

    #[test]
    fn history_includes_messages_the_address_sent() {
        // Recovering a conversation means both halves, not just inbound.
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/b", "outbound")).unwrap();
        let h = s.history("machine-a/a", None, 20).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].subject, "outbound");
    }

    #[test]
    fn stale_registrations_respects_the_cutoff() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        // Everything registered now is newer than a past cutoff, so nothing is stale...
        assert!(s.stale_registrations("2000-01-01T00:00:00.000Z").unwrap().is_empty());
        // ...and everything is older than a future one.
        let all = s.stale_registrations("2999-01-01T00:00:00.000Z").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].addr, "machine-a/a");
    }

    #[test]
    fn forget_removes_the_address_but_not_the_messages() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/b", "keep me")).unwrap();

        assert!(s.forget("machine-a/b").unwrap());
        assert!(!s.forget("machine-a/b").unwrap(), "second forget should report nothing removed");
        assert_eq!(s.peers().unwrap().len(), 1);
        // History survives: forget retires an identity, not the record of what was said.
        assert_eq!(s.history("machine-a/b", None, 20).unwrap().len(), 1);
    }

    #[test]
    fn peers_lists_registered_addresses() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-b/notes")).unwrap();
        let peers = s.peers().unwrap();
        assert_eq!(peers.len(), 2);
    }
}
