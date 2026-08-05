# agent-msg-bus — design, decisions and phasing

Push-delivery message bus for Claude Code sessions. Replaces the file-based `msgbus`
(`Tools/machine-a/tools/msgbus/`). A full rewrite, not a patch — the old code and scripts were explicitly
treated as replaceable.

**Status: Phase 0 ✅ · Phase 1b ✅ · Phase 1c next. Not deployed. The old bus is still the live one.**

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

## ⛔ Phase 2 blocker — Monitor cannot reach a private IP (found 5 Aug 2026)

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
| 3 | Client CLI: `send` / `peers` / `ack` / `sub-url` | Works on Windows **and** Linux | ✅ `ca7a8d5` — Windows verified; **Linux not yet built** |
| 2 | Deploy to the broker host via the `homelab-add-service` skill | Caddy vhost, firewall, Proxmox notes, homelab manual page | ▫️ needs go-ahead |
| 4 | `SessionStart` hook: register, tell the session its address, instruct arming | New session self-registers with no human step | ▫️ |
| 5 | machine-a cutover, both buses in parallel | Round-trip between two real machine-a sessions | ▫️ |
| 6 | the Linux server Remote Control sessions | Round-trip machine-a ↔ the Linux server | ▫️ |
| 7 | machine-b, including the offline-queue test | Message sent while machine-b is off arrives on reconnect | ▫️ |
| 8 | Retire old msgbus | Code archived, skill rewritten, old hooks removed | ▫️ |
| 9 | Channels adapter | Delivery with no arming step | ▫️ |

Phases 1–4 build nothing user-visible on their own. That is deliberate: each increment is small enough
that a killed session costs one step, per the standing session-limits policy.

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
- Claiming an address already held by a live socket → refused unless forced (1e)
- Two sessions in one repo never share a mailbox or cursor *(the machine-wide whoami bug)* (3/4)
- An address survives the session changing directory (3/4)
- A malformed stored row doesn't break delivery for anything else

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
