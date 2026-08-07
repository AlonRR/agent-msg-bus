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
             -- An address that also receives mail sent to `alias`. This is how a mailbox migrates
             -- without rewriting history: the stored `to` field is never touched, so what was
             -- actually sent stays true, and delivery resolves the alias at read time.
             CREATE TABLE IF NOT EXISTS aliases(
               alias TEXT PRIMARY KEY,
               target TEXT NOT NULL,
               created_at TEXT NOT NULL
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

        // `provisional` separates "this address exists and can receive" from "this address has ever
        // joined the bus". Conflating those is what made every alternative uncomfortable: registering
        // eagerly fills `peers` with sessions that never join, but registering lazily breaks
        // send-before-subscribe, where mail sent to a live session that has not yet armed its
        // subscription must still queue. A provisional row does both - it is a valid send target from
        // the moment a session starts, and it stays out of `peers` until the address actually
        // subscribes.
        //
        // Added by ALTER rather than in the CREATE so existing databases migrate in place. Existing
        // rows default to provisional and self-correct: anything that subscribes is promoted at once,
        // and anything that never does was never real.
        let has_col: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('registry') WHERE name = 'provisional'")?
            .exists([])?;
        if !has_col {
            conn.execute_batch(
                "ALTER TABLE registry ADD COLUMN provisional INTEGER NOT NULL DEFAULT 1;",
            )?;
        }
        Ok(Store { conn })
    }

    /// Mark an address as having genuinely joined the bus. Called when a subscription is accepted.
    pub fn promote(&self, addr: &str) -> Result<()> {
        self.conn
            .execute("UPDATE registry SET provisional = 0 WHERE addr = ?1", params![addr])?;
        Ok(())
    }

    /// Claim an address. A **new** address starts its cursor at the current head, so it does not
    /// receive history; a **returning** address keeps its existing cursor, so a session that
    /// crashed still gets the mail it never read.
    pub fn register(&self, reg: &Registration) -> Result<()> {
        let (_, ts) = mint_id();
        // Never demote: an address that has already joined stays joined even if a later session
        // re-registers it. Otherwise a routine re-register would hide a live address from `peers`.
        self.conn.execute(
            "INSERT INTO registry
               (addr, session_id, machine, repo, cwd, pid, registered_at, provisional)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
             ON CONFLICT(addr) DO UPDATE SET
               session_id = ?2, machine = ?3, repo = ?4, cwd = ?5, pid = ?6, registered_at = ?7",
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

    /// Every name `addr` answers to: itself, plus any alias pointing at it.
    pub fn names_for(&self, addr: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT alias FROM aliases WHERE target = ?1")?;
        let rows = stmt.query_map(params![addr], |r| r.get::<_, String>(0))?;
        let mut names = vec![addr.to_string()];
        for r in rows {
            names.push(r?);
        }
        Ok(names)
    }

    /// Migrate a mailbox: `to` starts answering to `from` as well, and inherits its reading
    /// position so `from`'s undelivered mail is actually visible.
    ///
    /// Both halves are required. The alias alone is not enough: `to` was registered later, so its
    /// cursor sits at the head and it would see none of `from`'s backlog — the same
    /// "a new address gets no history" rule that normally protects against flooding would silently
    /// defeat the migration. Taking the *older* cursor is what makes the mail appear, and taking
    /// the older one rather than `from`'s outright means migrating twice cannot move a cursor
    /// forwards and skip mail.
    ///
    /// The stored `to` field on existing messages is never rewritten: what was actually sent stays
    /// true, and the alias is resolved at read time instead.
    pub fn migrate(&self, from: &str, to: &str) -> Result<(usize, String)> {
        let (_, now) = mint_id();
        self.conn.execute(
            "INSERT OR REPLACE INTO aliases(alias, target, created_at) VALUES (?1, ?2, ?3)",
            params![from, to, now],
        )?;
        let from_cursor: String = self
            .conn
            .query_row("SELECT up_to FROM cursors WHERE addr = ?1", params![from], |r| r.get(0))
            .unwrap_or_default();
        let to_cursor: String = self
            .conn
            .query_row("SELECT up_to FROM cursors WHERE addr = ?1", params![to], |r| r.get(0))
            .unwrap_or_default();
        let adopted = if from_cursor < to_cursor { from_cursor } else { to_cursor };
        self.conn.execute(
            "INSERT INTO cursors(addr, up_to) VALUES (?1, ?2)
             ON CONFLICT(addr) DO UPDATE SET up_to = ?2",
            params![to, adopted],
        )?;
        let now_pending = self.pending_for(to)?.len();
        Ok((now_pending, adopted))
    }

    pub fn aliases_of(&self, target: &str) -> Result<Vec<String>> {
        let mut stmt =
            self.conn.prepare("SELECT alias FROM aliases WHERE target = ?1 ORDER BY alias")?;
        let rows = stmt.query_map(params![target], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Undelivered messages for `addr`, oldest first. Idempotent until acked.
    pub fn pending_for(&self, addr: &str) -> Result<Vec<Message>> {
        let names = self.names_for(addr)?;
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
        //
        // Matched against every name this address answers to, so a migrated mailbox receives mail
        // that was addressed to its predecessor. The sender is still excluded by its own address.
        let mut out = Vec::new();
        for row in rows {
            let m = row?;
            if names.iter().any(|n| addr_matches(&m.to, n)) {
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
        // Aliases count too, so a migrated mailbox can read its predecessor's history.
        let names = self.names_for(addr)?;
        let mut out = Vec::new();
        for row in rows {
            let m = row?;
            if names.iter().any(|n| addr_matches(&m.to, n) || &m.from == n) {
                out.push(m);
            }
        }
        if out.len() > limit {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }

    /// Addresses that have never joined the bus, older than `cutoff_ts`.
    ///
    /// Expired far more aggressively than joined addresses (hours, not days): a session that started
    /// and never subscribed genuinely is dead, and there is nothing to lose by forgetting it.
    pub fn stale_provisional(&self, cutoff_ts: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT addr FROM registry
              WHERE provisional = 1 AND registered_at < ?1 ORDER BY addr",
        )?;
        let rows = stmt.query_map(params![cutoff_ts], |r| r.get::<_, String>(0))?;
        let addrs: Vec<String> = rows.collect::<Result<Vec<_>>>()?;
        self.without_trafficked(addrs)
    }

    /// Drop any address that took part, or that still holds unread mail. THE single place
    /// bulk-retirement candidates are filtered.
    ///
    /// It exists as its own function because the guard has now been added to one query and missed on
    /// another twice: first the stranding check went into the `forget` handler and the sweeper
    /// bypassed it, then this guard went into `stale_provisional` and `stale_registrations` - the
    /// `prune --days` path - bypassed it. Both callers now go through here, so a third candidate
    /// query cannot silently be the unguarded one.
    ///
    /// Two predicates rather than one, because the first version of this collapsed them and was
    /// wrong in both directions at once: it counted a broadcast as participation (freezing a whole
    /// machine's registry forever) while relying on that same over-broad match to prevent stranding.
    /// Narrowing it alone would have traded a leak for lost mail.
    ///
    /// `forget` deliberately does NOT use this: retiring a trafficked address by explicit human
    /// action is legitimate, which is why that path reports what it strands instead of refusing.
    fn without_trafficked(&self, addrs: Vec<String>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for addr in addrs {
            if self.has_traffic(&addr)? || self.has_pending(&addr)? {
                continue;
            }
            out.push(addr);
        }
        Ok(out)
    }

    /// Has this address ever sent or been sent anything?
    ///
    /// Traffic is proof of participation, and it outranks the `provisional` flag. The flag means
    /// "has never subscribed *since promotion existed*" — for any address registered before that
    /// change it means "no record either way", and those two are not the same thing. Nothing
    /// distinguished them, so the sweeper deleted the second kind: `machine-b/agent-msg-bus.1956ec12`
    /// had subscribed, sent, and received, but subscribed *before* promotion shipped, so its flag
    /// still read provisional and it was swept along with genuine churn.
    ///
    /// "Has never joined" and "has traffic" must not be able to be true at once. When they are, the
    /// traffic is the stronger signal.
    pub fn has_traffic(&self, addr: &str) -> Result<bool> {
        let sent: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE sender = ?1",
            params![addr],
            |r| r.get(0),
        )?;
        if sent > 0 {
            return Ok(true);
        }
        // Deliberately an equality test, NOT `addr_matches`. A wildcard recipient is how a
        // broadcast reaches this address, so pattern-matching is correct for DELIVERY - but
        // retention asks whether THIS address took part, and "someone addressed the whole
        // machine once" is not evidence that it did. Matching patterns here meant a single
        // historical `machine-b/*` froze every machine-b address, past and future, permanently
        // unsweepable - the machine's registry could then only ever grow. Undelivered
        // broadcast mail is protected by `has_pending` instead, which is the narrower thing
        // that was actually worth protecting.
        let addressed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient = ?1",
            params![addr],
            |r| r.get(0),
        )?;
        Ok(addressed > 0)
    }

    /// Does this address still hold mail nobody has read?
    ///
    /// Separate from `has_traffic` because the two answer different questions, and a broadcast is
    /// where they come apart: receiving one is not participation, but sweeping an address that has
    /// not read one yet still destroys a message. `retire` only *reports* what it strands, so the
    /// prevention has to live in the candidate query.
    fn has_pending(&self, addr: &str) -> Result<bool> {
        Ok(!self.pending_for(addr)?.is_empty())
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
        let all: Vec<Registration> = rows.collect::<Result<Vec<_>>>()?;
        // Same guard as the provisional path. `prune --days N` used to bypass it entirely, so a
        // trafficked address could be retired in bulk and its mail stranded.
        let keep = self.without_trafficked(all.iter().map(|r| r.addr.clone()).collect())?;
        Ok(all.into_iter().filter(|r| keep.contains(&r.addr)).collect())
    }

    /// Does any registered address answer to this `to` pattern?
    ///
    /// The difference between "queued for a session that has not subscribed yet" and "queued for an
    /// address that does not exist" is the difference between patience and a typo, and the sender
    /// cannot tell them apart from the outside.
    pub fn recipient_is_known(&self, to: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare("SELECT addr FROM registry")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            let addr = r?;
            if addr_matches(to, &addr) {
                return Ok(true);
            }
            // An alias is a real name too: mail to a migrated-from address is not orphaned.
            if !self.names_for(&addr)?.iter().all(|n| !addr_matches(to, n)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Messages addressed to something no registration answers to.
    ///
    /// This is storage with no owner: it is not a stale registry entry, so nothing enumerates it and
    /// neither `forget` nor `prune` can reach it. Without a way to list it, a mistyped recipient is
    /// accepted, stored forever, and invisible — the sender having been told it would be delivered.
    pub fn orphaned_messages(&self, limit: usize) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, sender, recipient, kind, subject, body, reply_to
               FROM messages ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
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
        let mut out = Vec::new();
        for row in rows {
            let m = row?;
            if out.len() >= limit {
                break;
            }
            if !self.recipient_is_known(&m.to)? {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Delete a stored message outright. The only way to clear orphaned mail, since there is no
    /// registration for `forget` to remove.
    pub fn delete_message(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Retire an address: drop its registration and its cursor.
    ///
    /// Messages already sent to it stay in `messages` — this removes the *identity*, not history.
    /// Needed because a bad registration would otherwise sit in `peers` forever, and a stale
    /// address that looks live is the kind of thing that gets trusted later.
    pub fn forget(&self, addr: &str) -> Result<bool> {
        Ok(self.retire(addr)?.0)
    }

    /// Retire an address, reporting how many undelivered messages it strands.
    ///
    /// Returns `(existed, stranded)`. The stranding count is computed HERE, before the deletion, so
    /// every caller receives it. It used to be computed in the `forget` HTTP handler instead — and
    /// the sweeper, a second caller added later, therefore stranded mail silently on a 30-minute
    /// timer. Fixing a caller rather than the invariant is what allowed that.
    pub fn retire(&self, addr: &str) -> Result<(bool, usize)> {
        let stranded = self.pending_for(addr).map(|v| v.len()).unwrap_or(0);
        let n = self.conn.execute("DELETE FROM registry WHERE addr = ?1", params![addr])?;
        self.conn.execute("DELETE FROM cursors WHERE addr = ?1", params![addr])?;
        Ok((n > 0, stranded))
    }

    /// Addresses on the bus. By default only those that have actually joined — `peers` answers
    /// "who is here", and an address that has never subscribed is not.
    pub fn peers(&self, include_provisional: bool) -> Result<Vec<Registration>> {
        let sql = if include_provisional {
            "SELECT addr, session_id, machine, repo, cwd, pid FROM registry ORDER BY addr"
        } else {
            "SELECT addr, session_id, machine, repo, cwd, pid FROM registry
              WHERE provisional = 0 ORDER BY addr"
        };
        let mut stmt = self.conn.prepare(sql)?;
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
        assert_eq!(s.peers(true).unwrap().len(), 1);
        // History survives: forget retires an identity, not the record of what was said.
        assert_eq!(s.history("machine-a/b", None, 20).unwrap().len(), 1);
    }

    // ---- mailbox migration -------------------------------------------------

    /// The case this exists for: a session dies, its successor gets a different derived address,
    /// and mail already queued for the dead one must not be stranded.
    #[test]
    fn migrate_delivers_the_old_addresss_backlog_to_the_new_one() {
        let s = store();
        s.register(&reg("machine-a/sender")).unwrap();
        s.register(&reg("machine-a/homelab.old")).unwrap();
        s.send(&msg("machine-a/sender", "machine-a/homelab.old", "queued for the dead session")).unwrap();

        // The successor registers later, so its cursor starts at the head and it sees nothing.
        s.register(&reg("machine-a/homelab.new")).unwrap();
        assert_eq!(s.pending_for("machine-a/homelab.new").unwrap().len(), 0, "precondition");

        let (pending, _) = s.migrate("machine-a/homelab.old", "machine-a/homelab.new").unwrap();
        assert_eq!(pending, 1);
        let got = s.pending_for("machine-a/homelab.new").unwrap();
        assert_eq!(got.len(), 1, "backlog did not follow the migration");
        assert_eq!(got[0].subject, "queued for the dead session");
    }

    /// The alias alone is not enough, and this is the trap: without adopting the older cursor the
    /// successor is protected from the very backlog it is trying to inherit.
    #[test]
    fn migrate_adopts_the_older_cursor_not_the_newer() {
        let s = store();
        s.register(&reg("machine-a/sender")).unwrap();
        s.register(&reg("machine-a/a")).unwrap();
        s.send(&msg("machine-a/sender", "machine-a/a", "old mail")).unwrap();
        s.register(&reg("machine-a/b")).unwrap(); // cursor at head, ahead of a's

        let (_, adopted) = s.migrate("machine-a/a", "machine-a/b").unwrap();
        let a_cursor = s.pending_for("machine-a/a").unwrap();
        assert_eq!(a_cursor.len(), 1, "sanity: a still has its own backlog");
        assert!(adopted.is_empty() || adopted < "20260805".to_string(), "adopted cursor: {adopted}");
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 1);
    }

    /// Migrating twice must not skip mail by dragging the cursor forwards.
    #[test]
    fn migrating_twice_never_moves_the_cursor_forwards() {
        let s = store();
        s.register(&reg("machine-a/sender")).unwrap();
        s.register(&reg("machine-a/a")).unwrap();
        s.send(&msg("machine-a/sender", "machine-a/a", "one")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();

        s.migrate("machine-a/a", "machine-a/b").unwrap();
        let first = s.pending_for("machine-a/b").unwrap().len();
        s.migrate("machine-a/a", "machine-a/b").unwrap();
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), first, "second migrate lost mail");
    }

    #[test]
    fn a_migrated_mailbox_receives_mail_still_addressed_to_the_old_name() {
        // Senders that have not learned the new address must keep working.
        let s = store();
        s.register(&reg("machine-a/sender")).unwrap();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        s.migrate("machine-a/a", "machine-a/b").unwrap();

        s.send(&msg("machine-a/sender", "machine-a/a", "sent to the old name")).unwrap();
        let got = s.pending_for("machine-a/b").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "sent to the old name");
        assert_eq!(got[0].to, "machine-a/a", "stored `to` was rewritten; history must stay true");
    }

    #[test]
    fn migration_does_not_make_a_sender_receive_its_own_message() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/b")).unwrap();
        s.migrate("machine-a/a", "machine-a/b").unwrap();
        // b now answers to a. A broadcast from b must still not come back to b.
        s.send(&msg("machine-a/b", "machine-a/*", "broadcast")).unwrap();
        assert_eq!(s.pending_for("machine-a/b").unwrap().len(), 0);
    }

    // ---- provisional registration ------------------------------------------

    /// The whole point: a session that registers but never subscribes stays out of `peers`, while
    /// remaining a valid send target so send-before-subscribe still queues.
    #[test]
    fn a_registered_but_never_subscribed_address_is_hidden_from_peers_yet_still_receives() {
        let s = store();
        s.register(&reg("machine-a/sender")).unwrap();
        s.register(&reg("machine-a/never-joined")).unwrap();

        assert!(
            !s.peers(false).unwrap().iter().any(|p| p.addr == "machine-a/never-joined"),
            "a provisional address appeared in peers"
        );
        assert!(
            s.peers(true).unwrap().iter().any(|p| p.addr == "machine-a/never-joined"),
            "--all did not reveal it"
        );

        // Still addressable: this is the send-before-subscribe guarantee.
        s.send(&msg("machine-a/sender", "machine-a/never-joined", "queued before subscribing")).unwrap();
        assert_eq!(s.pending_for("machine-a/never-joined").unwrap().len(), 1);
    }

    #[test]
    fn subscribing_promotes_an_address_into_peers() {
        let s = store();
        s.register(&reg("machine-a/joins")).unwrap();
        assert!(!s.peers(false).unwrap().iter().any(|p| p.addr == "machine-a/joins"));
        s.promote("machine-a/joins").unwrap();
        assert!(s.peers(false).unwrap().iter().any(|p| p.addr == "machine-a/joins"));
    }

    /// A routine re-register must not hide an address that has already joined.
    #[test]
    fn re_registering_never_demotes_a_joined_address() {
        let s = store();
        s.register(&reg("machine-a/joins")).unwrap();
        s.promote("machine-a/joins").unwrap();
        s.register(&reg("machine-a/joins")).unwrap(); // e.g. the session restarted
        assert!(
            s.peers(false).unwrap().iter().any(|p| p.addr == "machine-a/joins"),
            "re-registering demoted a joined address"
        );
    }

    #[test]
    fn only_provisional_addresses_expire_on_the_short_clock() {
        let s = store();
        s.register(&reg("machine-a/joined")).unwrap();
        s.promote("machine-a/joined").unwrap();
        s.register(&reg("machine-a/never")).unwrap();

        let stale = s.stale_provisional("2999-01-01T00:00:00.000Z").unwrap();
        assert!(stale.contains(&"machine-a/never".to_string()));
        assert!(!stale.contains(&"machine-a/joined".to_string()), "a joined address was short-expired");
    }

    // ---- orphaned mail -----------------------------------------------------

    /// Mail to an address nothing answers to is stored, undeliverable, and — before this — invisible
    /// to every listing, unreachable by `forget` (no registration to remove) and by `prune` (same).
    /// Found by machine-b while inventing a guaranteed-offline target for a test.
    #[test]
    fn mail_to_a_nonexistent_address_is_findable_and_clearable() {
        let s = store();
        s.register(&reg("machine-a/real")).unwrap();
        let good = s.send(&msg("machine-a/real", "machine-a/real2", "to a typo")).unwrap();
        s.register(&reg("machine-a/real2")).unwrap();
        let orphan = s.send(&msg("machine-a/real", "machine-a/does-not-exist", "orphaned")).unwrap();

        assert!(s.recipient_is_known("machine-a/real2").unwrap());
        assert!(!s.recipient_is_known("machine-a/does-not-exist").unwrap());

        let found = s.orphaned_messages(50).unwrap();
        assert_eq!(found.len(), 1, "expected exactly the orphan, got {found:?}");
        assert_eq!(found[0].id, orphan);
        assert!(found.iter().all(|m| m.id != good), "a deliverable message was called orphaned");

        assert!(s.delete_message(&orphan).unwrap());
        assert!(s.orphaned_messages(50).unwrap().is_empty());
    }

    /// A wildcard recipient is not orphaned just because it is a pattern.
    #[test]
    fn a_wildcard_recipient_counts_as_known_when_something_matches_it() {
        let s = store();
        s.register(&reg("machine-a/alpha")).unwrap();
        assert!(s.recipient_is_known("machine-a/*").unwrap());
        assert!(!s.recipient_is_known("machine-b/*").unwrap());
    }

    /// A migrated-from address still has an owner, so mail to it is not orphaned.
    #[test]
    fn an_alias_target_keeps_the_old_name_from_looking_orphaned() {
        let s = store();
        s.register(&reg("machine-a/new")).unwrap();
        s.migrate("machine-a/gone", "machine-a/new").unwrap();
        assert!(s.recipient_is_known("machine-a/gone").unwrap(), "an aliased name looked orphaned");
    }

    // ---- the sweeper must never take a participant --------------------------

    /// The real incident: `machine-b/agent-msg-bus.1956ec12` had subscribed, sent and received — but it
    /// subscribed *before* promotion shipped, so its stored flag still read provisional and the
    /// sweeper deleted it, stranding the offline-queue proof. `provisional` meant "never joined" for
    /// new rows and "no record either way" for old ones, and nothing told those apart.
    #[test]
    fn an_address_with_traffic_is_never_swept_however_old_or_provisional() {
        let s = store();
        s.register(&reg("machine-a/participant")).unwrap();
        s.register(&reg("machine-a/genuine-churn")).unwrap();
        // Traffic, but never promoted — exactly the pre-migration shape.
        s.send(&msg("machine-a/participant", "machine-a/somewhere", "I did things")).unwrap();

        let stale = s.stale_provisional("2999-01-01T00:00:00.000Z").unwrap();
        assert!(
            !stale.contains(&"machine-a/participant".to_string()),
            "an address with traffic was offered up for sweeping: {stale:?}"
        );
        assert!(
            stale.contains(&"machine-a/genuine-churn".to_string()),
            "genuine churn was not swept: {stale:?}"
        );
    }

    /// The `prune --days N` path bypassed the guard entirely - it had been added to
    /// `stale_provisional` only. Found by dry-running prune against the LIVE broker, which listed a
    /// trafficked address as sweepable. Both candidate queries now share one filter, so this asserts
    /// the other one.
    #[test]
    fn a_broadcast_alone_does_not_protect_an_address_from_sweeping() {
        // A wildcard recipient means "everyone on that machine", so `addr_matches` is right for
        // DELIVERY - but treating it as participation makes one historical `machine-b/*` broadcast
        // protect every address that machine will ever have, forever. Retention asks a different
        // question than delivery: did THIS address take part?
        let s = store();
        s.register(&reg("machine-a/only-broadcast")).unwrap();
        let id = s.send(&msg("machine-a/sender", "machine-a/*", "broadcast")).unwrap();
        s.ack("machine-a/only-broadcast", &id).unwrap();

        let stale = s.stale_provisional("2999-01-01T00:00:00.000Z").unwrap();
        assert!(
            stale.contains(&"machine-a/only-broadcast".to_string()),
            "a broadcast it had already read froze it permanently: {stale:?}"
        );
    }

    #[test]
    fn pending_mail_protects_an_address_even_when_it_arrived_by_broadcast() {
        // The other half. The sweeper only REPORTS stranding, so whatever prevents it has to be
        // in the candidate query. Narrowing participation without adding this would trade an
        // over-broad guard for stranded mail.
        let s = store();
        s.register(&reg("machine-a/unread-broadcast")).unwrap();
        s.send(&msg("machine-a/sender", "machine-a/*", "broadcast")).unwrap();

        let stale = s.stale_provisional("2999-01-01T00:00:00.000Z").unwrap();
        assert!(
            !stale.contains(&"machine-a/unread-broadcast".to_string()),
            "offered up an address holding undelivered mail: {stale:?}"
        );
    }

    #[test]
    fn the_age_based_prune_path_also_refuses_an_address_with_traffic() {
        let s = store();
        s.register(&reg("machine-a/participant")).unwrap();
        s.register(&reg("machine-a/genuine-churn")).unwrap();
        s.send(&msg("machine-a/other", "machine-a/participant", "traffic")).unwrap();

        let stale = s.stale_registrations("2999-01-01T00:00:00.000Z").unwrap();
        let addrs: Vec<String> = stale.into_iter().map(|r| r.addr).collect();
        assert!(
            !addrs.contains(&"machine-a/participant".to_string()),
            "the age path offered up a trafficked address: {addrs:?}"
        );
        assert!(
            addrs.contains(&"machine-a/genuine-churn".to_string()),
            "the age path stopped returning genuine churn: {addrs:?}"
        );
    }

    #[test]
    fn a_broadcast_is_delivery_not_participation_but_still_protects_unread_mail() {
        // This test used to assert the opposite - that a wildcard recipient counts as traffic -
        // written while fixing `machine-b/agent-msg-bus.1956ec12`. Checking the broker afterwards
        // showed that address had 1 sent and 3 EXACTLY-addressed messages, so the wildcard clause
        // was never what saved it: the generalisation went past the evidence, and the cost was
        // that one `machine-b/*` broadcast made every machine-b address unsweepable forever.
        //
        // What the incident actually needed is below - and the protection an unread broadcast
        // deserves now comes from pending mail, which is the narrower true reason.
        let s = store();
        s.register(&reg("machine-a/sender")).unwrap();
        s.register(&reg("machine-a/quiet-receiver")).unwrap();
        s.send(&msg("machine-a/sender", "machine-a/*", "broadcast")).unwrap();

        assert!(
            !s.has_traffic("machine-a/quiet-receiver").unwrap(),
            "a broadcast was counted as this address participating"
        );
        assert!(
            !s.stale_provisional("2999-01-01T00:00:00.000Z")
                .unwrap()
                .contains(&"machine-a/quiet-receiver".to_string()),
            "offered up an address holding an unread broadcast"
        );

        // Exact addressing IS participation, which is what 1956ec12 had.
        s.send(&msg("machine-a/sender", "machine-a/quiet-receiver", "direct")).unwrap();
        assert!(s.has_traffic("machine-a/quiet-receiver").unwrap());
    }

    /// Retirement must report what it strands, at the store level, so no caller can be the quiet
    /// one. The previous guard lived in the `forget` HTTP handler; the sweeper bypassed it.
    #[test]
    fn retire_reports_stranded_mail_to_every_caller() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-a/doomed")).unwrap();
        s.send(&msg("machine-a/a", "machine-a/doomed", "you will never read this")).unwrap();

        let (existed, stranded) = s.retire("machine-a/doomed").unwrap();
        assert!(existed);
        assert_eq!(stranded, 1, "retire did not report the mail it stranded");
    }

    #[test]
    fn peers_lists_registered_addresses() {
        let s = store();
        s.register(&reg("machine-a/a")).unwrap();
        s.register(&reg("machine-b/notes")).unwrap();
        let peers = s.peers(true).unwrap();
        assert_eq!(peers.len(), 2);
    }
}
