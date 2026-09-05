# agent-msg-bus — working notes for a session in this repo

## Your bus address

It should be **`<machine>/agent-msg-bus`** — no session-id suffix. Check with `agent-msg-bus
whoami`, which also prints the name actually **bound** on the relay.

**From v0.3.0 this is automatic**: the address is derived from the repo, and if another live session
in this repo already holds it, `watch --fallback` binds `<machine>/agent-msg-bus.<8 hex>` instead and
says so on its first line. Nothing to do.

**Below v0.3.0 — including any broker or client not yet reinstalled — the hook hands you the
suffixed name and you must fix it by hand.** `agent-msg-bus --version` says which build you have; if
it answers *"unexpected argument"*, it predates all of this. The suffix is not cosmetic: it gives
every new session a fresh, empty mailbox, so mail queued for the repo while no session was running
is never delivered — it sits under a dead name nothing drains. That cost 27 messages across 14 dead
addresses before this was fixed.

**Order matters, and getting it wrong is the difference between a clean handover and a mess.**
`register` FIRST, then `migrate`:

```bash
agent-msg-bus register <machine>/agent-msg-bus --machine <machine> --repo agent-msg-bus \
                       --session-id "$CLAUDE_CODE_SESSION_ID" --cwd "$(pwd)"
agent-msg-bus pin <machine>/agent-msg-bus
agent-msg-bus migrate <machine>/agent-msg-bus.<your suffix> <machine>/agent-msg-bus
```

Then re-arm `Monitor` against the new address, and stop the old watch first — `migrate` returns
**409** while the predecessor still holds a live socket, which is correct and means "disconnect it
first", not "something is broken".

Why register before migrate, on any broker older than v0.2.0:

- `migrate` does not create a registry row, so without it you are live and receiving while `peers`
  shows only the OLD address, offline. Every peer deciding whether you are reachable concludes no.
- It gives the new address a cursor. Without one, cursor adoption takes `min(from, to)` where a
  missing cursor reads as the empty string — the oldest possible value — so **the predecessor's
  entire acked history replays**. With it, adoption picks your real position and nothing replays.

Both are fixed from v0.2.0 on, so on a current broker `migrate` alone is enough. `agent-msg-bus
--version` says which build you have; if it answers *"unexpected argument"*, it predates the fix.

> This is a workaround, not the design. The address format is `<machine>/<repo>.<role>` and the
> session id is squatting the **role** slot. See *"Identity should be repo-scoped"* in
> [docs/plan.md](docs/plan.md) for the proper fix.

## Updating this machine's binary

`agent-msg-bus update` — swaps the installed binary by renaming the old one aside, so **nothing is
killed and nothing is restarted**. Do NOT use `scripts/install-windows.ps1` to update a running
machine: it kills every `agent-msg-bus.exe`, which ends every session's inbox and stops the relay,
and if the relay's supervisor cannot relaunch it the machine stays deaf.

After `update`, already-running processes still hold the old build. A session picks the new one up by
re-arming its `Monitor` subscription; the relay only on restart, which is a decision about whether
this machine can get its relay back.

## Before changing code

- `cargo test` is the baseline — run it unchanged first, and compare against the count it prints
  rather than a number written down here, which goes stale within a release or two. Four groups:
  store/hook/update unit tests, CLI argument tests, integration tests that drive a real broker over
  a real socket, and relay tests that cover the fallback path through a real 409.
- **Behaviour change → write the failing test first.** Every fix in this repo's history has one, and
  the commit messages say what the symptom was. Keep that.
- `cargo clippy --all-targets -- -D warnings` must stay clean; CI gates on it.
- `cargo fmt` is **not** gated and the tree is not rustfmt-clean. Do not reformat as a side effect.

## Documentation rules this repo actually enforces

- **`docs/usage.md` and `docs/operations.md` name no deployment** — no addresses, hostnames or
  container IDs. They describe the tool, not one installation. Deployment specifics belong in the
  lab's own notes.
- **Label a claim measured, relayed, or inferred** — in commit messages, in docs, and in bus
  messages. The receiver cannot tell them apart and will act on a guess as if you had checked it.
- Version and release rules are at the top of [CHANGELOG.md](CHANGELOG.md). The version lives in
  `Cargo.toml` and nowhere else.
