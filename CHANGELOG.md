# Changelog

All notable changes to `agent-msg-bus` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

**The version lives in `Cargo.toml` and nowhere else.** `--version` reads it through clap's
`version` attribute, and `src/main.rs` carries a unit test asserting the two agree — so the only way
the binary can lie about which build it is, is a tag that does not match. Releasing is:

1. Move the `[Unreleased]` entries under a new dated heading.
2. Bump `version` in `Cargo.toml` to match that heading.
3. `cargo test` — the version test is part of it.
4. Commit, then `git tag -a vX.Y.Z -m "vX.Y.Z"`, then push the branch **and** the tag.

While the major version is 0: a breaking change to the **wire contract** — the `/sub`, `/send`,
`/register` and `/ack` shapes clients bind to — bumps the **minor**, because that is the one change
every deployed client has to be updated for. Everything else bumps the patch. The storage behind
that contract is explicitly not part of it and can change in a patch.

## [Unreleased]

## [0.2.0] — 2026-09-03

The first tagged release. Everything under *Added* was built and deployed while `Cargo.toml` still
said `0.1.0`; no tag was ever cut, so there is no 0.1.0 to compare against and this entry describes
the surface as it stands rather than a diff. From here the tags are real.

### Added

- **`--version`.** The binary can now say which build it is. Several machines run their own copy and
  they are updated at different times, so "is the fix actually deployed over there?" was previously
  answerable only by hashing files.
- **Broker** (`serve`): HTTP for `/health`, `/register`, `/send`, `/ack`, `/peers`, `/forget`,
  `/migrate`, `/messages`, `/orphans`, `/orphans/delete` and `/prune`, plus a WebSocket `/sub` that
  holds a subscription open for the life of a session. A
  frame pushed down it starts a turn in an idle session, which is the mechanism no hook can provide.
  Refuses to start without `--tokens` unless `--insecure-no-auth` is passed explicitly — a missing
  tokens file is a hard error, never a silent downgrade to open access.
- **Per-machine relay** (`relay`): holds the broker connection and re-serves it on loopback, because
  the subscribing side refuses to open a WebSocket to a private IP. One per machine, multiplexing
  every session on it.
- **Self-reconnecting subscription** (`watch`): the `ws:` subscription form ends its watch when the
  socket closes and does not retry, so a relay restart left a session deaf until a human re-armed
  it. `watch` reconnects internally instead, so the watch is never torn down.
- **Client commands**: `send` (with `--body-file`, including `-` for stdin), `ack`, `read`, `peers`,
  `register`, `whoami`, `sub-url`, `migrate`, `pin`/`unpin`, `forget`, `prune`, `orphans`.
- **Durable cursor-based store** (SQLite, compiled in): a message is stored once with its `to`
  pattern and each mailbox keeps a cursor. At-least-once delivery — a crash redelivers, never
  destroys. Wildcard addressing (`machine/*`) is matched in Rust, not SQL, so there is one
  implementation and it is unit-tested.
- **Provisional registration**: separates "can receive mail" from "has joined the bus", with a
  background sweeper so the distinction expires on its own rather than only when someone runs
  `prune` by hand.
- **`orphans`**: lists mail addressed to something no registration answers to. Without it a mistyped
  recipient was accepted, stored forever, and reachable by nothing.
- **SessionStart hook** (`session-start`): derives a session's address and prints the subscribe URL.
- **MPL-2.0 licence.** Until now there was none, which for a public repository means all rights
  reserved — nobody could legally use or fork it, whatever the README implied.
- **CI** (`.github/workflows/ci.yml`): clippy (`-D warnings`) and the test suite on both Linux and
  Windows, because the broker is deployed to one and the clients run on the other from the same
  binary. Not gated on `cargo fmt`; see the comment in the workflow for why.
- **Tag-driven releases** (`.github/workflows/release.yml`): builds both platforms and publishes the
  binaries with checksums. It refuses to publish if the tag and `Cargo.toml` disagree, and again if
  the built binary does not report the version it is being released as — the one drift the unit test
  cannot catch, because a wrong tag is applied after the test has passed.

### Fixed

- **A migrated mailbox looked offline to everyone while it was receiving mail.** Reported from a
  live session that changed address after its working directory moved. `migrate` wrote an alias and
  a cursor and no registry row, so the successor had no `peers` entry to appear in; it drained its
  backlog while every peer's roster showed only the predecessor, offline. `migrate` now registers
  the successor when it has no row of its own (never overwriting one that does), and `peers` lists
  anything holding a live socket whether or not the registry backs it.
- **Mail addressed to a migrated-from name was never pushed to the live successor.** The hub matched
  a message's recipient against the single address each socket holds, so nothing resolved the alias.
  The message still arrived on that session's next reconnect replay, which made it look like a
  cosmetic status-line bug — the sender was told "queued … will be delivered on connect" while the
  target was in fact subscribed. It was not cosmetic: for every migrated address, push delivery had
  degraded into polling.
- **Migrating into a fresh address replayed the predecessor's whole acked history, every time.** An
  absent cursor came back as the empty string, which sorts before every id, so taking the older of
  the two cursors always chose the beginning. Absent is not the same as "at the beginning".
- **An aliased address reported mail the mailbox had already acked, and the number never moved.**
  Acks landed on the target, so the predecessor kept a cursor nothing could advance. `pending` and
  `ack` now both resolve to the mailbox, and an address that is only an alias is no longer listed as
  a participant in its own right — it appears under `aliases` on the row that owns it.
- **`forget` on an aliased name claimed to orphan mail it did not touch.** Consequence of the fix
  above: once `pending` resolves an alias to its mailbox, every caller inherits that resolution —
  including the ones asking "what is lost if this registry row goes away". Retiring an alias strands
  nothing, because the alias goes on resolving; the count now comes from `pending_owned_by`, which
  is empty for a name that is only an alias. The same guard stops an alias row becoming unprunable
  because the mailbox it points at happens to hold unread mail.
- **Chained migrations stranded mail.** `a → b` then `b → c` left anything sent to `a` owned by `c`
  but resolved only as far as `b`. Alias resolution is transitive throughout, and both walks check
  membership before stepping, so a circular migration terminates instead of hanging the broker.
