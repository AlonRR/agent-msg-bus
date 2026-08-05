# agent-msg-bus — design, decisions and phasing

Push-delivery message bus for Claude Code sessions. Replaces the file-based `msgbus`
(`Tools/machine-a/tools/msgbus/`). A full rewrite, not a patch — the old code and scripts were explicitly
treated as replaceable.

**Status: deployed and carrying real traffic.** Phases 0–7 complete: broker live on the broker host, relays
running as services on machine-a, the Linux server and machine-b, sessions self-register at startup, and messages have
been delivered across machines into idle sessions unprompted. **The durable queue is now proven
against a machine that leaves the bus** — see Phase 7.

**Remaining:** Phase 8 (retire the old bus), Phase 9 (Channels). The old file-based `msgbus` is still
running in parallel and untouched.

---

## Decisions (settled — do not re-litigate)

| Decision | Choice | Why |
|---|---|---|
| Transport | **Purpose-built WebSocket broker**, frozen wire contract, swappable core | Monitor's `ws:` source consumes it with zero client install |
| Wake path | **Monitor `ws:` now, Channels MCP adapter later** | Monitor is stable and needs no launch flag; Channels is research preview |
| Autonomy | **Act immediately; surface consequential actions first** | |
| Scope | **machine-a + Linux server sessions + machine-b** | |
| WAN | **LAN-only v1**; durable queue covers machine-b while away | No VPN exists yet — see below |
| Broker language | **Rust** | Single binary, matches `onedrive-sync`, strongest guarantees for the concurrency this system has historically got wrong |
| Repo | **`alon/agent-msg-bus`** — its own repo, not inside `machine-a` or `homelab` | Spans three machines; it is not homelab-only infrastructure |
| Old stuck mail | **Leave exactly as-is** until Phase 8 | No running session gets interrupted; the backlog is dealt with at retirement |

**There is no WAN path into the lab today.** `homelab/docs/manual/firewall-segmentation.md` lists the
SSL-VPN piece as *"Out of scope for this phase"* and the lab's firewall rollout is still an unchecked
checklist. machine-b therefore reaches the broker only on the LAN in v1; the durable queue means it
receives everything on reconnect rather than losing it. **Do not design as if a VPN exists.**

### Why NATS/MQTT were rejected despite being better messaging systems

Monitor's `ws` source is **receive-only** and reports binary frames as `[binary frame, N bytes]`.
NATS-over-WebSocket is **binary-only** (*"the server always sending in Binary and clients MUST send in
Binary too"*) and needs an `INFO → CONNECT → SUB` handshake. MQTT likewise needs a client handshake.
So neither can be consumed by Monitor directly — both would force an adapter process onto every
machine on day one, which is the thing the WebSocket choice buys out of.

This is why "reuse the existing MQTT container" was considered and dropped: reusing existing infrastructure
stops being the cheap option once it costs a client install everywhere.

**The escape hatch is the frozen wire contract, not the storage.** If hand-rolled durability
disappoints, the core can become NATS behind the same `/sub` `/send` `/register` `/ack` surface and
no client changes.

---

## Why the old bus is being replaced

Three defects, all confirmed with evidence on 5 Aug 2026:

1. **Delivery requires manually arming a watcher.** The intended safety net — `SessionStart` →
   `watchPaths` → `FileChanged` hook — never fires. `hook-session-start.ps1` registers a **glob**
   (`…/outbox/*.jsonl`); nothing in the docs says globs expand in `watchPaths`, and the bus sits
   outside every project root. Every failure mode is silent by construction, so it looked healthy
   from 31 Jul onward.
2. **No hook can make an idle session act.** `additionalContext` is passive — *"Claude reads the
   reminder on the next model request."* The entire file-watch approach was therefore capped at
   pre-loading a message, never at acting on one. This is why the rewrite changes mechanism rather
   than fixing the hook.
3. **Addresses die with sessions, silently.** `peers.json` holds three schema generations at once;
   one entry has no `procStart`, so liveness is PID-existence only — a false "live" waiting to happen
   on a box with ~25 stale `claude` processes. Mail to a dead address is orphaned while the sender
   sees success.

**Evidence: 5 messages sat undelivered**, oldest 3 Aug, two of them to sessions that were still
running. One was titled *"UPDATE YOURSELF: … restart your watcher and re-register"* — undelivered by
defect 1.

---

## Phase 0 — verify the premise. This gated everything.

**One unverified assumption underpinned the whole plan:** that a Monitor `ws:` event wakes a
*genuinely idle* session — one sitting at the prompt with no turn running — rather than only landing
mid-turn. That is the same class of assumption that broke the old system, so it was tested before any
code was written.

Probe: [`tools/phase0_ws.py`](../tools/phase0_ws.py), a stdlib-only WebSocket server on
`127.0.0.1:9444` pushing text frames at +60 s / +180 s / +330 s after connect, then holding the socket
open for an hour. No dependencies deliberately — an install step is a second thing that can fail and
would muddy a negative result.

### Results (5 Aug 2026)

| Question | Result |
|---|---|
| Does Monitor perform a real WS handshake? | ✅ **Yes.** `CONNECT` + `HANDSHAKE OK` within seconds of arming |
| Does the query string survive? | ✅ **Yes** — server saw `GET /sub?addr=machine-a/homelab.probe&token=phase0test`. Confirms the auth design, since the `ws` schema has no headers field |
| Does a frame wake a genuinely **idle** session? | ✅ **YES.** Frame 1 at 15:16:21 (+60 s). The session had ended its turn and was waiting on the user; the frame re-invoked it unprompted, payload intact, no human input |
| Does a **second** frame on the same socket wake it again? | ✅ **YES.** Frame 2 at 15:18:21 (+180 s), same connection. The subscription is not one-shot |
| Does `persistent: true` outlive the old 1 h cap? | ✅ **YES.** Connected 15:15:21, still live at 16:20:51 — **65 min 30 s**, including **60 minutes of total silence** (15:20:51 → 16:20:51, no frames, no pings). It ended only because the probe's own `sleep(3600)` expired |
| Is a dropped subscription visible or silent? | ✅ **Visible.** Monitor surfaced `[WebSocket closed: 1006 Connection ended]` as an event — the session is *told* it went deaf |
| Does the socket survive the machine sleeping? | ⚠️ **Not tested.** Do not assume either way |

**The premise holds.** A WebSocket frame from an external process starts a turn in a session sitting
idle at the prompt — exactly what no hook can do, and exactly what the old bus needed.
`Monitor({ws:…, persistent:true})` is a real subscription: no polling, no arming per message, no 5 s
latency floor.

The repeat-delivery result shapes the broker: **one long-lived socket per session carries many
messages**, so the broker holds a connection rather than re-establishing one per delivery.

**Scope of the evidence:** this ran on `127.0.0.1`. It proves the mechanism and its duration, not its
behaviour across the LAN through a firewall, and not across a suspend/resume. Both are Phase 2 checks.

### Three requirements Phase 0 handed to the design

1. **The client must re-arm on close.** `persistent: true` ends when the socket ends. The close is
   surfaced as an event, so it is actionable — but something must act on it, or the session is
   silently deaf from then on. That is the old bus's defining failure mode and it must not be
   recreated. Re-arming belongs in the skill, with a `Stop` hook as backstop.
2. **The broker must close cleanly.** The probe just exited, producing `1006` (abnormal, no close
   handshake). A real broker sends `1001 going away` on shutdown so a client can distinguish an
   orderly restart from a network fault.
3. **Keepalives are prudent but not proven necessary.** 60 minutes of silence did not kill a loopback
   socket. A LAN path through the lab's firewall may reap idle connections where loopback does not, so the
   broker should ping — but that is a precaution against an untested path, **not** a fix for an
   observed failure. Do not let the code comment claim otherwise.

---

## ✅ Phase 2 blocker — RESOLVED by the loopback relay (5 Aug 2026)

Fixed by `agent-msg-bus relay`. Verified end-to-end: a message sent to the broker host across the LAN was
pushed down the relay's upstream socket, re-served on `127.0.0.1:9451`, and woke this session
through Monitor.

```
20260805T180057450-000000000   lab-server/server.peer -> machine-a/homelab.build   delivered, unprompted
```

Two design properties confirmed live rather than assumed:

- **Lazy upstream works.** With the relay running but no Monitor attached, the broker reported the
  address `offline`; it flipped to `live` the moment Monitor subscribed. So "no local subscriber"
  really does mean "queue it", which is what makes the offline story honest.
- **The relay absorbs reconnection**, retiring Phase 0's requirement 1. The *local* socket survives
  upstream outages, so Monitor never sees a close and never needs re-arming.

The original diagnosis is kept below, because the constraint itself has not gone away — anything
future that points Monitor at a LAN address will hit it again.

---

## ⛔ The blocker itself — Monitor cannot reach a private IP (found 5 Aug 2026)

**Monitor's `ws:` source refuses to connect to any RFC1918 / link-local / cloud-metadata address.**
Both attempts were rejected by Monitor itself, before any network traffic:

```
ws://<broker-ip>:9450   -> "Monitor cannot open a WebSocket to <broker-ip>: the address is in
                              a private, link-local, or cloud-metadata range."
wss://msgbus.example.internal    -> "msgbus.example.internal resolves to <proxy-ip>, which is in a private,
                              link-local, or cloud-metadata range"
```

This is a **client-side SSRF guard**, not a network, firewall, TLS or CA problem:

- HTTP from machine-a to `<broker-ip>:9450` works (`register`, `peers` both fine over the LAN).
- `https://msgbus.example.internal/health` returns `200` from machine-a — Caddy's internal CA *is* trusted here.
- Loopback is allowed: Phase 0 and the local broker both ran on `127.0.0.1` without complaint.

**Consequence: a session cannot subscribe directly to a central broker.** The broker deployment is
sound and verified, but no session on machine-a, machine-b or the Linux server can hold a subscription to it as built.

Phase 0 could not have caught this — it ran on loopback by construction. The plan said so explicitly
(*"this ran on 127.0.0.1 … not its behaviour across the LAN — that is a Phase 2 check"*), and the
Phase 2 check is what found it.

### Options

| Option | Shape | Cost |
|---|---|---|
| **Loopback relay** (recommended) | `agent-msg-bus relay` runs per machine, holds the LAN connection to the broker host, re-serves on `127.0.0.1`. Monitor connects to loopback, which is permitted | ~1 phase of work; one supervised process per machine; owns reconnect logic, which requirement 1 needed anyway |
| **SSH tunnel** | `ssh -N -L 9450:127.0.0.1:9450 lab-server`, Monitor hits loopback | No new code, but a tunnel to supervise per machine, with no reconnect or backoff of its own |
| **Channels** | The MCP server is an ordinary local subprocess with no SSRF guard, so it can reach the LAN directly | Removes the blocker *and* the arming step, but is research preview, needs a launch flag on every session, and Node/Bun everywhere |

The relay and Channels are not exclusive: the relay is the near-term fix, and Channels later removes
both the relay and the arming step. Either way **the broker, the wire contract and the store are
unaffected** — this is a last-hop problem, which is exactly what freezing the wire contract was meant
to contain.

---

## Architecture

```
    machine-a session         machine-b session        server claude-remote-control@<repo>
         │                     │                            │
         └──── Monitor ws ─────┴────────────────────────────┘
                    ws://<host>:PORT/sub?addr=…&token=…
                                  │
                          ┌───────▼────────┐
                          │ agent-msg-bus  │   the broker host
                          │  SQLite store  │   messages · cursors · registry
                          └───────▲────────┘
                                  │  POST /send  /register  /ack
         ┌────────────────────────┼────────────────────────┐
    agent-msg-bus CLI     SessionStart hook        Channels adapter (Phase 9)
```

CT hostname and Caddy vhost are a Phase 2 decision, deliberately not fixed here.

### Wire contract — FROZEN. This is the stable interface.

| Endpoint | Purpose |
|---|---|
| `POST /register` | `{addr, session_id, machine, repo, cwd, pid}` → claims an address |
| `GET /sub?addr=…&token=…` | WebSocket. Server pushes JSON **text** frames. Replays unacked backlog on connect |
| `POST /send` | `{to, subject, body, kind, reply_to}` → `{id}` |
| `POST /ack` | `{addr, up_to_id}` |
| `GET /peers` | live + known addresses |

**One message per text frame.** Monitor turns each *frame* into one notification, so batching several
messages into one frame would collapse them into a single event.

```json
{"id":"…","ts":"…","from":"machine-a/homelab.loop","to":"machine-a/machine-a.fixes",
 "kind":"fyi|request|blocking","subject":"…","body":"…","reply_to":""}
```

**Auth rides in the query string** because Monitor's `ws` schema is `{url, protocols}` with **no
headers field** — verified, not assumed. Per-machine token, mode-600 local config, never committed.
Query-string tokens get logged by proxies: acceptable on a LAN-only service, and a specific reason not
to expose this over WAN without revisiting it.

### Addressing and liveness — the real fix for defect 3

Address shape stays `<machine>/<repo>.<role>`; the reserved-generic-names rule from the old bus was
sound and carries over. What changes:

- **The registry lives in the broker, not a synced file.** No `peers.json`, no schema drift, no
  OneDrive fighting over one file, no `msgbus-whoami.d`.
- **Liveness is WebSocket connection state.** A socket is open or it is not. No PID guessing, no
  `procStart` heuristics, no false "live".
- **An address outlives its session.** Mail to a disconnected address queues; whoever next claims the
  address drains it. That is the orphaned-mail fix.

### Durability

Cursor-based: a message is stored once with its `to` pattern, each address keeps a cursor, pending =
matching messages with `id > cursor`. **The recipient acks; the broker never assumes delivery** — the
peek→emit→confirm lesson from the old bus, moved server-side where it is transactional and testable.

The broker is the **only writer**, which removes the entire class of cross-process append races the
old file design had to defend against.

### Autonomy contract

`kind` is sender-declared: `fyi` (fold in, act if read-only/local), `request` (do it if within this
session's remit), `blocking` (reply with status even if the answer is "not yet").

**The invariant that actually enforces safety:** `kind` comes from another agent and is only as
trustworthy as the sender, so **no value of `kind` authorises a consequential action**. Writes outside
the repo, infra changes, deletions, pushes, and anything that spends money get surfaced to the human
first regardless of what the sender claimed. Every delivered message is labelled as another agent's
words, never the user's.

---

## Phases — each is one commit, leaves the tree coherent, and is independently useful

| # | Phase | Done when | Status |
|---|---|---|---|
| 0 | Verify Monitor wakes an idle session | Result recorded above | ✅ passed |
| 1a | Repo skeleton, `.gitattributes`, README, pushed | Builds | ✅ `d072e2c` |
| 1b | Store: messages, cursors, registry | 12 tests green | ✅ `56b8d63` |
| 1c | HTTP `/register` `/send` `/ack` `/peers` | Tests green | ✅ `ca7a8d5` |
| 1d | WebSocket `/sub` + replay on reconnect | Tests green | ✅ `ca7a8d5` |
| 1e | Token auth | Tests green | ✅ `ca7a8d5` |
| 2 | Deploy the broker to its host | Service live, firewall, Caddy vhost, Proxmox notes, homelab docs | ✅ live on the broker host |
| 2b | Loopback relay (**unplanned** — forced by the private-IP guard) | Cross-host delivery verified | ✅ `1875442`, made per-machine in `2856a43` |
| 3 | Client CLI: `send` / `peers` / `ack` / `forget` / `whoami` | Works on Windows **and** Linux | ✅ both — 24 tests green on each |
| 4 | `SessionStart` hook: register + tell the session its address | New session self-registers with no human step | ✅ `2856a43`, in the binary rather than per-OS scripts |
| 5 | machine-a cutover, both buses in parallel | Round-trip between two real machine-a sessions | ✅ relay is a Scheduled Task; a session is live on the bus |
| 6 | the Linux server Remote Control sessions | Round-trip machine-a ↔ the Linux server | ✅ a message from the Linux server woke an machine-a session |
| 7 | machine-b, including the offline-queue test | Message sent while machine-b is off arrives on reconnect | ✅ **passed** — 2 messages queued while offline, both replayed in order on reconnect, none lost |
| 8 | Retire old msgbus | Code archived, skill rewritten, old hooks removed | ▫️ |
| 9 | Channels adapter | Delivery with no arming step | ▫️ |

Phases 1–4 build nothing user-visible on their own. That is deliberate: each increment is small enough
that a killed session costs one step, per the standing session-limits policy.

### Phase 7 — machine-b: done on 5 Aug 2026, and what it cost

`machine-b` is **not reachable over SSH from machine-a**; the `claude-config-sync` skill says so outright
(*"run the command on that machine"*), so this had to be run on the machine itself.

**Correction to the DNS claim previously recorded here — now measured from machine-a too, and the
original claim was simply wrong.**

This section used to say `ping machine-b` succeeds because the name resolves through wildcard DNS to
the reverse proxy (`<proxy-ip>`). The machine-b session flagged that as too broad; measuring from machine-a shows it
was worse than that — it was a misreading.

Measured from machine-a:

```
nslookup machine-b                    -> *** can't find machine-b: Non-existent domain
nslookup nonsense-xyz123          -> *** Non-existent domain          (bare names: NXDOMAIN)
nslookup nonsense-xyz123.example.internal -> <proxy-ip>                   (wildcard is *.example.internal only)
ping machine-b                        -> <machine-b-ip>   <- machine-b ITSELF, not Caddy
```

**Where the error came from:** `nslookup` prints the *resolver's* `Server:`/`Address:` before the
answer. `<proxy-ip>` was the DNS server (the reverse proxy) being quoted back, not the result for
`machine-b`. The bare name never resolved via DNS at all — `ping` found machine-b over NetBIOS/LLMNR, and
`<machine-b-ip>` is genuinely machine-b.

So, corrected:

- The wildcard trap is **real but scoped to `*.example.internal`**, exactly as the machine-b session said. It is
  the same trap `homelab/docs/manual/rc-panel.md` documents for SSH aliases.
- **machine-b is network-reachable** from machine-a. The ping was honest.
- The reason it cannot be onboarded remotely is **not DNS — it has no SSH server**:
  `Test-NetConnection <machine-b-ip> -Port 22` → `False`, and `ssh machine-b` times out. That is what the
  `claude-config-sync` skill means by "not reachable over SSH".

The conclusion (run the bootstrap on that machine) never changed; the stated reason was wrong, and a
wrong reason is worth correcting because it sends the next person to fix DNS, or to distrust pings in
general, when neither is the problem.

Phase 7 is one command, run **on machine-b**, with that machine's own token:

```powershell
# get machine-b's token (on any machine that can reach the homelab host):
ssh <broker-host> 'cat /etc/agent-msg-bus/tokens.json'

# then, on machine-b, from a clone of this repo after `cargo build --release`:
.\scripts\bootstrap-client.ps1 -Machine machine-b -Token <machine-b's token>
```

Build and tests on machine-b: `cargo build --release` clean, **24 tests green** (12 store + 8 integration
+ 4 relay), matching machine-a and the Linux server.

#### ✅ The offline-queue test — PASSED

The one property this design had never been tested against a machine that genuinely leaves.

| Step | What was done | Result |
|---|---|---|
| a | Marker A sent while relay up | ✅ pushed within ~1 s, woke an idle session unprompted; acked so it could not be confused with a replay |
| b | Relay stopped, machine-b off the bus | ✅ broker flipped every machine-b address to `offline`; Monitor surfaced `[WebSocket closed: 1006 Connection ended]` |
| c | **Two** messages sent while offline | ✅ broker held them — `machine-b/agent-msg-bus.1956ec12  offline  2 pending` |
| d | Relay restarted, subscription re-armed | ✅ **both replayed, oldest-first, payloads intact**; ack advanced the cursor to `0 pending` |

Two were sent rather than one deliberately: one message cannot distinguish "the queue works" from
"the queue keeps only the newest". Both arrived, in order.

**Scope of the evidence — read this before citing the result.** The offline window was created by
stopping the relay process, not by suspending the machine. From the broker's side that is a true
offline window (socket closed, address `offline`, mail queued), which is what the durable queue
claims to handle. It is **not** a suspend/resume test: the laptop never slept and the NIC never went
down. Suspend/resume therefore remains untested — see Known limitations, which is unchanged on that
point.

#### ⚠️ `Stop-ScheduledTask` did NOT stop the relay before `d1dc268` — it produced a false pass

**Superseded by `d1dc268`, kept because the failure it caused is instructive.** Against the
fire-and-forget shim, the obvious way to run step (b) —
`Stop-ScheduledTask -TaskName 'agent-msg-bus relay'` — did nothing to the relay. Measured on machine-b:

```
relay pid before : 14988
task state after : Ready
relay pid after  : 14988      <- same process, still alive
relay /health    : still 200, still subscribed
```

Cause: `bootstrap-client.ps1` registered `wscript.exe` against `relay-hidden.vbs` calling
`WScript.Shell.Run(..., 0, False)` — fire-and-forget. `wscript.exe` exited the instant it spawned the
relay, so the task had already completed and owned no child to kill.

This mattered more than a papercut: anyone running the offline test that way sends a message, watches
it arrive, and concludes the queue works — **while the machine was never offline at all**. A false
pass on the exact property Phase 7 exists to prove. The Phase 7 run above therefore used
`Stop-Process -Name agent-msg-bus`, which is what made the offline window real.

`d1dc268` changes the shim to `Run(..., 0, True)` + `WScript.Quit rc`, so `wscript.exe` now stays
alive as the relay's parent for the task's lifetime. **`Stop-ScheduledTask` should therefore be
effective from `d1dc268` onward — but that has not been re-measured on machine-b**, which was still
running the old shim when this was written. Do not treat it as verified until someone stops the task
and confirms the pid is gone. Until then `Stop-Process -Name agent-msg-bus` remains the lever known
to work.

#### Other findings worth not rediscovering

- **A running relay is not a reachable address.** After restart, `/health` reported `"subscribed":[]`
  and the broker still showed `offline / 2 pending` until Monitor re-attached. The lazy upstream
  (`relay.rs`) is working as designed, but "the relay is up" is not the same as "this machine is
  receiving" — do not use process liveness as the health signal. `agent-msg-bus peers` is the honest one.
- **Monitor must be re-armed after a relay restart.** Killing the relay kills the local socket, so
  `persistent: true` ends — exactly Phase 0's requirement 1. The close *is* surfaced (`1006`), so it
  is actionable, but a human or the skill has to act on it. The relay absorbs *upstream* outages, not
  its own death.
- **Rapid replay can coalesce into one notification.** MARKER-B and MARKER-C arrived as two separate
  frames (the wire contract's one-message-per-frame rule held) but landed in a single Monitor
  notification, because Monitor batches events within ~200 ms. Both were intact and individually
  parseable, so nothing was lost — but a session that counts *notifications* rather than parsing
  frames would undercount a burst.
- **`bootstrap-client.ps1` writes `settings.json` with a UTF-8 BOM** under Windows PowerShell 5.1,
  where `Set-Content -Encoding UTF8` emits one. Confirmed by attribution: the pre-bootstrap backup has
  no BOM, the post-run file does (`EF BB BF`). Claude Code tolerated it — the hook fired and other
  sessions on machine-b registered normally — so this is a latent wart, not a live break. Worth switching
  to `utf8NoBOM` / `[IO.File]::WriteAllText` before a stricter parser meets it.
- **Onboarding needs two things the bootstrap does not cover:** a `Host git-remote` block in
  `~/.ssh/config` (machine-b had none — `git-remote` did not resolve, and Gitea is on its own host at
  `<gitea-ip>`), and a per-machine SSH key registered with Gitea. Neither is in any script; both
  cost time on this machine. The next machine onboarded will hit both.

---

## Tests — port the invariants, not the code

The old `test-msgbus.ps1` / `test-identity.ps1` encode invariants learned from real bugs that a
single-writer smoke test passes happily without. Each is re-expressed against the new system, **written
failing first**.

Done in 1b (12 green):

- N concurrent senders → N messages, 0 lost *(the `FileShare.ReadWrite` write-loss bug: 40 writers all reported success and produced 35 lines)*
- Unacked messages replay; reading never consumes *(the at-most-once bug that ate a real reply)*
- Ack never moves the cursor backwards
- A sender never receives its own broadcast
- A brand-new address gets no history; a *returning* address keeps its cursor
- Wildcard and exact addressing
- Pending is ordered oldest-first

Still to cover in later phases:

- Disconnect mid-delivery → unacked messages replay on reconnect (1d)
- Bad or missing token → rejected, and the rejection is **visible, not silent** (1e)
- A relay whose upstream is unreachable **announces it** rather than going quietly deaf (2b — implemented, not yet covered by a test)
- Claiming an address already held by a live socket → refused unless forced (1e)
- Two sessions in one repo never share a mailbox or cursor *(the machine-wide whoami bug)* (3/4)
- An address survives the session changing directory (3/4)
- A malformed stored row doesn't break delivery for anything else

---

## Relay supervision on Windows — what actually recovers a dead relay

**The repetition trigger is the supervisor. `-RestartCount` does not recover a crashed relay**, and
the config reads as though it does. This was measured on machine-b, twice, by killing the relay rather
than reading the settings:

```
kill 23:30:37 -> back 23:33:38 = 184s, landing exactly on the 5-minute repetition grid
earlier kill  -> back 23:28:37 = same grid
LastTaskResult = 267009 (SCHED_S_TASK_RUNNING) throughout; never a failure code
RestartCount=999, RestartInterval=PT1M present the whole time
```

If restart-on-failure were firing, recovery would have been ~60s. It was on the repetition grid both
times.

**Why:** Task Scheduler's restart-on-failure fires when a task ends *unexpectedly* — fails to start,
or is terminated by the service. An action that exits non-zero is recorded in `LastTaskResult`, but
the task counts as having completed normally, so no restart is scheduled. Making the shim propagate
the exit code (`WScript.Quit rc`) therefore buys **observability, not supervision** — a claim
previously made in this repo that was half right and is now corrected.

**Consequence:** the repetition interval *is* the worst-case inbound-delivery outage. It is now
**1 minute** (was 5). The only cost of the short interval is a `wscript` spawn per minute that exits
immediately, because the relay returns 0 on `AddrInUse` when one is already running.

`-RestartCount`/`-RestartInterval` are kept for the case they genuinely cover — the task failing to
start at all — and are deliberately not relied on for crash recovery.

The blocking shim is still worth having independently: the task now stays `Running` for the relay's
lifetime with `wscript` as its live parent, which is what makes `Stop-ScheduledTask` actually stop
the relay.

**The Linux server is unaffected throughout.** systemd `Restart=always` supervises properly; every problem in
this section is Windows-only.

---

## Known limitations (accepted for v1, written down so they are not rediscovered as surprises)

- **A token authenticates a machine, not an address.** Any holder of a valid token can `send` with
  any `from` value, so a compromised client could impersonate another session. Acceptable on a
  LAN-only bus where every token holder is already trusted, but it means `from` is an attribution
  hint, not proof — and it is a reason the receiving-side rule ("no `kind` authorises a consequential
  action") does the real safety work.
- **`tokens.json` is read once at startup.** Adding or revoking a token needs
  `systemctl restart agent-msg-bus`. Revocation is therefore not instant unless you restart.
- **Monitor's WS client sends no headers**, so the token rides in the query string and will appear in
  proxy logs. Fine for LAN-only; revisit before any WAN exposure.
- **Suspend/resume across a laptop sleeping is still untested** for both the relay and Monitor.
  Phase 7 did *not* close this: its offline window was made by stopping the relay process, which is a
  genuine broker-side offline window but leaves the NIC up and the machine awake. What a real suspend
  adds — a socket that dies without a clean close, a clock jump, and DHCP/ARP churn on resume — is
  exactly what is still unproven. machine-b remains the machine that could prove it.
- **Killing the relay ends the session's Monitor subscription** and nothing re-arms it automatically.
  The close is visible (`1006`), not silent, so it is actionable — but until the skill acts on it,
  recovery is a human step. The relay absorbs upstream outages; it cannot absorb its own restart.
- **`Stop-ScheduledTask` did not stop the relay before `d1dc268`** (detached `wscript` shim — see
  Phase 7). Any runbook that used it to simulate an outage was testing nothing. `d1dc268` makes the
  shim wait, which should fix this; **not yet re-measured on machine-b**, so `Stop-Process` stays the
  lever known to work until someone confirms.
- **`wss://` from Monitor is untested** — and moot for now, since the private-IP guard blocks the
  vhost anyway. Caddy's internal CA *is* trusted by machine-a's cert store (verified over HTTPS).

---

## Rollback

The old bus stays running and untouched until Phase 8. The two share no state and use entirely
different mechanisms, so they run in parallel with no interference. Rollback at any phase = stop using
this one.

---

## Deep reference

Homelab context (separate repo): `docs/manual/service-user.md` (Remote Control) ·
`docs/manual/rc-panel.md` · `docs/services.md` (CT allocation) ·
`docs/manual/firewall-segmentation.md` (why there is no WAN path).
Old system: `Tools/machine-a/tools/msgbus/README.md`.
