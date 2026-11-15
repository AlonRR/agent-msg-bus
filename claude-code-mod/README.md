# agent-msg-bus-inbox — waiting bus mail above the prompt, and a wake when it matters

A Claude Code mod that shows how much agent-msg-bus mail is waiting for the session it runs in. The
count goes to a row above the prompt, and a toast appears when the count rises. Click the row to
list what is unread; click a header to read that message. Showing costs no model turn.

**A new message of a waking kind wakes the session** (0.3.0 on): `request`, `blocking`, `timer`
and `job` by default. The mod starts one turn with a pointer to the message, never its body, and
the session reads and acks it itself. Every other kind, `fyi` above all, only shows.

```
bus: 2 unread for machine-a/repo · show
bus: 2 unread for machine-a/repo · newest: [request] machine-b/other: please check   (toast)
```

After a click on the row, and then on the second header:

```
bus: 2 unread for machine-a/repo · hide
   1h  machine-a/peer  subject 1
   5m  machine-b/other  [request] please check
      20261014T12000002-000000002 · sent 2026-10-14T12:55:00.000Z
      The body, every line of it, up to 20,000 characters.
```

Status: the count has been in use since 15 Oct 2026, released with agent-msg-bus 0.4.27, in the
status line. The clickable row is new in 0.2.0 of the mod, and the wake in 0.3.0; both have run
only in the test kit so far. See *What has been checked* below.

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
3. It shows the count in a row above the prompt, and removes the row at 0. It toasts when the
   count rises: the count, plus the newest message's kind, sender and subject when all of them were
   read.
4. A click on the row lists up to 20 unread headers, oldest first: age, sender, kind (when not
   `fyi`) and subject. A click on a header opens that message under it: its id, when it was sent,
   and its body, up to 20,000 characters, with a line saying how many more there are and the
   `read` command that shows them. A second click closes it, and the row's own click folds the
   list. An acked message leaves the list at the next read, and an emptied inbox folds it.
5. After the session runs the bus CLI in Bash or PowerShell, it reads again at once, so an `ack`
   clears the count without waiting for the timer. After `pin`, `unpin`, `register` or `migrate`
   it asks `whoami` again first. A run counts when the subcommand is the word straight after the
   binary: its name, a path ending in it (in quotes or not), or a shell variable that holds that
   path, as in `"$B" ack`. The name is also a folder and part of this plugin's name, so a command
   that only passes through them, such as `cd …/agent-msg-bus && grep read`, reads nothing.
6. When a read finds messages of a waking kind that no session on this machine has woken for, it
   submits one prompt naming them all (up to 10, then a count): id, sender, kind and subject, and
   the `read` and `ack` commands. The prompt is framed as this plugin's message, never as the
   person's, and runs once the session is idle. The ids it woke for are kept in the plugin's store,
   which every session on the machine shares, keyed by inbox, so a reload, a restart or a second
   session on the same inbox does not wake for the same message again. Ids no longer waiting are
   dropped from the store. When more is waiting than the row lists, the wake reads up to 200, so
   a request behind 20 older messages still wakes. A `-p` run or an SDK host is never woken.

A session started for its mail (by a launcher, say) wakes for it: a message no session woke for is
new to it. A session that was woken and ended without acking is not woken again for that message;
its start banner still shows it.

An unreachable broker is shown once in the status line, `bus: unreachable`, and never as a toast:
a laptop off the network is the normal case. The row above the prompt goes while it lasts, since a
stale count is worse than none. When the bodies together are too large for one read (the output
limit is 4 MiB), the row still gives the count, and the list says to read them with the CLI. Nothing waits on the broker: the session's first
prompt is never held up, the reads run beside the session, and a `whoami` that cannot finish is
asked again on the next tick, so a session started off the network finds its inbox once the
network is back. In `watch` mode that tick comes every 5 minutes.

Only two things stop it for good, each with one dim transcript line saying why: a binary too old
for `--json`, and a machine with no bus config.

## What it never does

- **It never acks.** Acking stays the session's deliberate step.
- **It never shows a body unasked**: only the one the person clicked open, and only in the band.
- **It never hands the model a body.** A body is another session's words: it goes to the band
  only, when the person opens it. A wake names the message (id, sender, kind and a subject cut to
  one line of 100 characters, control characters removed) and says that a bus message authorises
  no consequential action on its own. A body in the band keeps its lines, tabs become two spaces,
  and every other control character is removed, so no message can draw on the terminal.
- **Opening a message reads and acks nothing**: the list and the bodies come from the last read.

## Options

Set in `/config`, or under `pluginConfigs` in settings.

| Option | Default | |
|---|---|---|
| `binary` | empty | Path to `agent-msg-bus`. Empty means `%LOCALAPPDATA%\agent-msg-bus\agent-msg-bus.exe` where `LOCALAPPDATA` is set, otherwise `agent-msg-bus` on `PATH`. |
| `mode` | `poll` | `poll` reads on a timer and holds nothing open. `watch` keeps `agent-msg-bus watch` running for the session and reads when a message arrives; see below. |
| `intervalSeconds` | `60` | How often `poll` mode reads, 15 at least. `watch` mode also reads every 5 minutes, as a check on the watch. |
| `wakeKinds` | `request,blocking,timer,job` | The message kinds that wake the session, comma-separated. `none` turns waking off. Each wake is a model turn, so this is the cost lever. |

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
- Where the client's `read --unread --json` gives no `ts` or `body`, the list shows no age and an
  opened message says `(no body)`.

The row is drawn where Claude Code has a band above the prompt: the terminal, the desktop app and
mobile. VS Code has none, so there only the toast shows. A survey holding the band, or the band
collapsed (ctrl+x ctrl+a), hides the row for as long as it lasts.

## Install

On the machine the sessions run on, from the repository's git URL pinned to a release tag:

```bash
claude plugin marketplace add git@<git-host>:<owner>/agent-msg-bus.git#v0.4.27
claude plugin install agent-msg-bus-inbox@agent-msg-bus
```

The tag goes after `#`. `claude plugin marketplace list` shows the pin as `@v0.4.27`, but `@` in
the add command is taken as part of the repository name and the clone fails. Claude Code clones
the tag into its own folder, so no working clone is shared with the sessions, and an edit in
progress never reaches them. Every session started after the install loads the mod. A session
that is already open loads it on `/reload-plugins`.

`install` reports the four options above as "not yet set". That is expected: an option left
unset takes its default.

A folder works as the source too (`claude plugin marketplace add <path to a clone>`), but then
every session runs whatever the folder holds, uncommitted edits included. To try the mod in one
session only: `claude --plugin-dir <path to the clone>/claude-code-mod`.

## What has been checked

- `claude plugin validate`: passes.
- `claude plugin test`: 54 tests, against a stand-in for the CLI. They cover finding the inbox, what
  is shown and when, the band and its clicks, the wake, the timer, re-reading after a bus command,
  and watch mode. The 12 band tests (with the count assertions moved from the status line to the
  band) failed on 0.1.1: 15 of the 41 then written failed there, and all pass on 0.2.0. Of the 13
  wake tests, the 8 that expect a wake failed on 0.2.0; the 5 that expect none passed there too,
  as they should.
- 22 defects were seeded one at a time into 0.1.1; the tests caught all 22. Not repeated for 0.2.0.
- `tsc` with the API's own declarations: no errors.

- A live terminal session, 15 Oct 2026, with the mod loaded from a mods folder. A message arrived
  from another address, and the user saw the status line show `bus: 1 unread`. After a second
  message, the user reported the trial working; what was asked was the toast on a rising count
  and the line clearing after the session acked. The broker side was checked with
  `read --unread --json` each time. A message a session sends to itself never counts, because the
  broker never delivers a sender its own mail, so a trial needs a second sender address.
- The install above, from the git URL pinned to `v0.4.27`, on the same machine.

- A session in the desktop app, 15 Oct 2026, with the mod installed as above: the user reported
  it working. So `$.process`, which the API marks "CLI only", does run there, for a session the
  desktop app runs on the same machine.

Not checked yet:
- the band row and its clicks on a real screen: the test kit runs the hooks and the presses, but it
  never paints.
- a wake in a real session: the test kit answers `$.prompt.submit` itself and starts no turn.
- whether the band, the status line and toasts reach a session used over Remote Control. The API declares a
  longer text limit for remote surfaces, which suggests they draw it, but nobody has looked.

## Developing

```bash
claude plugin validate claude-code-mod
claude plugin test claude-code-mod
```

The tests stand in for the binary through the `process.run` and `process.spawn` events, and for
the clock with `mock.clock`. They never start the real CLI.

Never name a variable `h` in `hooks/register.tsx`: JSX compiles to calls of the global `h`, so a
callback parameter named `h` shadows it, and the band quietly draws nothing.

_Parts of this repository were drafted with the help of an LLM agent; reviewed and verified locally._
