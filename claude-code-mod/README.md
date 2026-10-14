# agent-msg-bus-inbox — waiting bus mail in the status line

A Claude Code mod that shows how much agent-msg-bus mail is waiting for the session it runs in. The
count goes to the status line, and a toast appears when the count rises. **No model turn starts**:
the session is not woken and the model is not told. The mod is for the person at the screen.

```
bus: 2 unread
bus: 2 unread for machine-a/repo · newest: [request] machine-b/other: please check   (toast)
```

Status: a prototype. Its tests pass under `claude plugin test`, and it has not yet run in a live
session. See *What has been checked* below.

## Why

A session without a `Monitor` watch sees bus mail only when it starts, or when it thinks to read.
Keeping a watch costs a model turn every 30 minutes, because Monitor expires it and the expiry
notice starts a turn (measured 15 Sep 2026; see `docs/plan.md`). The mod reads the inbox from
outside the model, which costs no turns at all.

## What it does

1. At session start it runs `agent-msg-bus whoami --json`, with the session's id in
   `CLAUDE_CODE_SESSION_ID`, and reads mail from the `inbox` field. It never derives an address
   itself, so pins and fallbacks work the way the CLI says they do.
2. It runs `agent-msg-bus read <inbox> --unread --json` once at start and then on a timer. That
   read changes nothing on the broker: no cursor moves and no registration is written.
3. It shows the count in the status line, and clears it at 0. It toasts when the count rises:
   the count, plus the newest message's kind, sender and subject when all of them were read.
4. After the session runs the bus CLI in Bash or PowerShell, it reads again at once, so an `ack`
   clears the count without waiting for the timer. After `pin`, `unpin`, `register` or `migrate`
   it asks `whoami` again first.

An unreachable broker is shown once in the status line, `bus: unreachable`, and never as a toast:
a laptop off the network is the normal case. Nothing waits on the broker: the session's first
prompt is never held up, the reads run beside the session, and a `whoami` that cannot finish is
asked again on the next tick, so a session started off the network finds its inbox once the
network is back. In `watch` mode that tick comes every 5 minutes.

Only two things stop it for good, each with one dim transcript line saying why: a binary too old
for `--json`, and a machine with no bus config.

## What it never does

- **It never acks.** Acking stays the session's deliberate step.
- **It never shows a body.**
- **It never puts mail where the model reads it.** A subject is another session's words. It goes
  to the status line and toasts, which the model does not see, and never to the prompt, the
  context or a transcript row. Subjects are cut to one line of 100 characters, with control
  characters removed, so no message can draw on the terminal.

## Options

Set in `/config`, or under `pluginConfigs` in settings.

| Option | Default | |
|---|---|---|
| `binary` | empty | Path to `agent-msg-bus`. Empty means `%LOCALAPPDATA%\agent-msg-bus\agent-msg-bus.exe` where `LOCALAPPDATA` is set, otherwise `agent-msg-bus` on `PATH`. |
| `mode` | `poll` | `poll` reads on a timer and holds nothing open. `watch` keeps `agent-msg-bus watch` running for the session and reads when a message arrives; see below. |
| `intervalSeconds` | `60` | How often `poll` mode reads, 15 at least. `watch` mode also reads every 5 minutes, as a check on the watch. |

⚠️ **`watch` mode subscribes the session's address.** That is a real change in how the session sits
on the bus. The session shows as live in `peers`. A second session in the same repo binds its own
fallback address, as the CLI intends. `migrate` answers 409 while the watch is held. After an
`update`, the watch runs the old build until the mod reloads. A watch that ends is shown in the
status line and started again after 30 seconds; `watch` reconnects by itself, so it ends only when
it gives up or is stopped.

## Needs

- agent-msg-bus **0.4.27 or later** on the machine, for `whoami --json` and `read --unread --json`.
- A bus config (`~/.agent-msg-bus/config.json`), as for any session that uses the bus.
- A broker at 0.4.23 or later, for `--unread`.

## Install

From a clone of this repository, on the machine the sessions run on:

```bash
claude plugin marketplace add <path to the clone>
claude plugin install agent-msg-bus-inbox@agent-msg-bus
```

A marketplace that is a folder is read from that folder, so a `git pull` and `/reload-plugins`
update the mod with no reinstall. That cuts both ways: every session runs whatever the folder
holds, uncommitted edits included, so install from a clone nobody edits in. To try it in one session only:
`claude --plugin-dir <path to the clone>/claude-code-mod`.

## What has been checked

- `claude plugin validate`: passes.
- `claude plugin test`: 26 tests, against a stand-in for the CLI. They cover finding the inbox, what
  is shown and when, the timer, re-reading after a bus command, and watch mode.
- 19 defects were seeded one at a time; the tests caught all 19.
- `tsc` with the API's own declarations: no errors. A deliberate type error was caught.

Not checked yet:
- a live session;
- whether the status line and toasts reach a session the desktop app hosts, or one used over
  Remote Control. The API declares a longer text limit for remote surfaces, which suggests they
  draw it, but nobody has looked;
- that `$.process`, which the API marks "CLI only", runs in those sessions.

## Developing

```bash
claude plugin validate claude-code-mod
claude plugin test claude-code-mod
```

The tests stand in for the binary through the `process.run` and `process.spawn` events, and for
the clock with `mock.clock`. They never start the real CLI.

_Parts of this repository were drafted with the help of an LLM agent; reviewed and verified locally._
