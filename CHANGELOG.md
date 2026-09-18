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

While the major version is 0, the **minor** bumps for a change every deployed client has to be
updated for. That is two things, not one: a breaking change to the **wire contract** (the `/sub`,
`/send`, `/register` and `/ack` shapes clients bind to), **or a change to how addresses are
derived** — because a session whose name changes is as unreachable to its peers as one whose
protocol changed. Everything else bumps the patch. The storage behind the wire contract is
explicitly not part of it and can change in a patch.

## [Unreleased]

### Changed

- **Example addresses no longer carry real machine names.** Two of them appeared 43 times across the
  docs, the changelog, doc comments and test data, as the machine half of example bus addresses. They
  are now the `machine-a` / `machine-b` placeholders the rest of the documentation already used, so
  the convention is consistent and the repo carries no real host names. No behaviour change: every
  occurrence was prose or test data, and the addressing format is untouched. Raised by a sweep of the
  repositories planned for release.
- **Container ids are gone from the design doc and one source comment**, 18 of them, replaced by the
  role each was standing for: the broker host, the Linux server, the Gitea host, the MQTT container,
  the reverse proxy. An id identifies this particular installation and means nothing to a reader
  without its inventory; the role is what the sentence was always about. Products stay named — the
  reverse proxy and the hypervisor are named the same way the sessions this bus serves are.
- **`docs/plan.md` now says outright that its addresses and host names are illustrative** — and why
  they are deliberately private-range rather than RFC 5737 documentation addresses: the client-side
  guard being demonstrated refuses an address *for being private*, so a documentation-range example
  would make the quoted refusals nonsense. The same sweep asked whether one of them was a live
  address; it is not, and now the page says so rather than leaving a reader to work it out.

## [0.4.18] — 2026-09-17

### Changed

- **The SessionStart banner no longer tells a session to arm a subscription, and no longer hands it
  a call to paste.** It had done both since the beginning, and 0.4.16 made it worse by adding
  "re-arm on every expiry notice" — which is exactly the recurring cost the standing decision of
  15 Sep 2026 exists to stop: Monitor expires a watch after 30 minutes even with `persistent: true`,
  and each expiry notice starts a paid turn, about 48 a day per subscribed session whether or not
  any mail arrives. A banner carrying a paste-ready `Monitor({…})` call silently reinstated that on
  every new session, whatever anyone had decided. The banner now states that the session is not
  subscribed and why, in the session's own context where the decision actually gets made.
- **The banner says how much mail is waiting.** With nothing subscribed, reading *is* the delivery
  mechanism, so a session needs to know at start whether to look. The count comes from the existing
  `/peers` route — no broker change — and a failure to fetch it is reported as "could not be asked"
  rather than shown as an empty mailbox, because a session that reads "nothing waiting" stops
  looking.
- **Push is still supported, now as a deliberate choice**: the `watch` command stays in the banner
  with its cost attached and the `ws:` warning intact, but the paste-ready arming call is gone. If
  you want it, you have to decide it.
- `docs/usage.md` replaces "arm the subscription at the start of each session" with the read/ack
  routine; the README and the `migrate` and `pin` messages, which all still told sessions to re-arm,
  say the same thing now.

## [0.4.17] — 2026-09-16

### Fixed

- **`update` would silently install an OLDER binary than the one already installed.** Its source
  defaults to this repo's `target/release` build, which is whatever was last compiled on that
  machine — measured on 16 Sep 2026: the repo build was 0.4.8 while the installed binary was 0.4.15,
  so a bare `update` would have put a seven-release-old binary into the path every session and the
  relay depend on, and reported success. An older source is now refused, naming both versions and
  the way past it; `--force` installs it deliberately, which is what a rollback needs. The
  comparison is numeric, because compared as text "0.4.10" sorts below "0.4.9" — wrong in both
  directions at exactly the versions this project is at. A version either side cannot parse is
  never refused: a comparison that cannot be made says nothing rather than guessing.
- **`read --since` accepted a timestamp and silently returned the entire history.** `since` is
  compared as text against message ids, and a timestamp sorts below every id this store can hold, so
  `id > since` matched every row — a session asking what arrived while it was away got everything,
  with no error and no way to tell old mail from new. Reported by a session returning after five
  days. Anything that is not a message id is now refused, by the CLI before the request goes out and
  by `GET /messages` as a 400, in the same shape as the existing cursor refusal. `--since` also has
  help text now, which it never had; the absent description is what made a timestamp a reasonable
  guess in the first place.

## [0.4.16] — 2026-09-15

### Changed

- **The SessionStart banner no longer tells a session to arm its inbox once.** It said the watch
  "is never torn down", which was true of `watch` and stopped being true of the Monitor holding it.
  Measured on 15 Sep 2026: Claude Code now expires a `persistent: true` watch after exactly 30
  minutes and kills its command, and the broker lists the address offline until the session arms it
  again. A session that took the banner at its word went deaf half an hour after it started. The
  banner now says the watch can expire, says to re-arm with the same call on Monitor's expiry
  notice, and says that mail sent in the gap replays rather than being lost. The subscribe paragraph
  moved into its own function so that those claims are pinned by tests.
- `docs/usage.md`, the README, the `watch` help text and `docs/plan.md`'s Phase 0 result now say the
  same. The Phase 0 measurement that `persistent: true` outlived the old one-hour cap is marked
  superseded rather than deleted.

## [0.4.15] — 2026-09-14

### Fixed

- **The relay's "this process was suspended" qualifier could not fire for the outage it was built
  for.** 0.4.9 separated wall clock from retry work so a sleeping laptop's low attempt count would
  be explained instead of reading as a wedged loop. Retry work was the measured duration of each
  connect attempt plus the nominal backoff — and `Instant` keeps advancing while the machine is
  suspended. So a laptop that slept while a connect was in flight booked the whole sleep as one
  attempt's worth of retry work: the quantity built to *exclude* suspension absorbed it, the gap
  between the two clocks collapsed, and the qualifier stayed silent.

  Captured in the field on a real sleep of ~578 minutes, reported as `585m23s, 11 attempts` with no
  qualifier — one attempt per 53 minutes against a 30s cap. It was worse than one missed footnote:
  while awake, retry work and wall clock both grow ~51s per attempt, so a collapsed gap *stays*
  collapsed. One suspension silenced the qualifier for the entire remainder of that outage, across
  every repeat. The same shape recurred later with a clean baseline in front of it: 114 attempts at
  a steady 51.6s, then 2 attempts in 142m57s, and again no qualifier.

  The connect is now wrapped in a **30s timeout**, and the booking is bounded by it. Because no
  attempt that was actually running can exceed the timeout, any longer measurement is by
  construction time the process was not running, and it stays out of retry work. A timeout rather
  than a bare ceiling on the booking, deliberately: a ceiling alone would under-book legitimate
  connects on systems whose default connect timeout is far longer than Windows's ~21s, and that
  under-booking can open a spurious gap on an early repeat. A side effect, and a benign one: a
  connect that would previously have blocked longer now retries after 30s.

  The existing qualifier tests could not have caught this — they construct retry work directly,
  and `outage_detail` and `unexplained_gap` were always correct given their inputs. The defect was
  in what fed them. The new tests exercise the booking itself, including a replay of the field
  numbers, and were shown failing against the old behaviour before the fix.

  Diagnosed by the machine that sleeps, which pointed at the exact line and gave a two-frame proof
  that needed none of the relay's internal state.

- **The SessionStart banner's `Monitor` command could not be run as written.** It interpolated the
  executable path unquoted into a command `Monitor` runs in bash, and bash strips backslashes from
  an unquoted word. Checked in real bash rather than asserted: the old form reaches bash as
  `C:Users…agent-msg-bus.exe` and exits **127**; the corrected form runs. Arming the inbox from the
  banner verbatim therefore always failed on Windows, and only ever worked when whoever copied it
  added quotes by hand.

  The path is now single-quoted in that line. Single rather than double because the command is
  displayed inside the banner's own double-quoted `command: "…"`, where nested double quotes render
  as `""C:\…exe" watch"` and invite the copier to drop them. A path the banner had already
  double-quoted for containing a space is unwrapped first, so bash never receives literal `"`
  characters as part of the filename.

  Scoped to that line on purpose. The banner's `send` and `ack` lines carry the same path, but they
  are not guaranteed to run in bash — in PowerShell a quoted path at the start of a statement is an
  expression, not a command — so quoting them would trade one shell's failure for another's.

  Reported by another session that hit it.

### Changed

- **`docs/plan.md`'s Phase 0 conclusion is narrowed, not reversed.** It said a WebSocket frame
  starts a turn in an idle session, "exactly what no hook can do". A `SessionStart` hook registered
  with `asyncRewake: true` that exits 2 now also starts a turn — which covers the moment a session
  starts, precisely when its subscription is missing. No hook fires for an event arriving mid-session,
  so the frame is still the only thing that delivers a message into a session already running.

## [0.4.14] — 2026-09-12

### Fixed

- **The broker pinged to tell idle from dead, received the pong, and threw it away — so a sleeping
  client read as `live` for up to seventeen minutes.** This is the 0.4.11 relay defect one layer up,
  in `drive()`:

  ```rust
  Some(Ok(_)) => {} // clients are receive-only here; pongs and stray frames are ignored
  ```

  There was no last-seen timestamp anywhere in that loop, leaving a **write-side** error as the
  broker's only notion of death. Writes to a half-open socket do not fail; they sit in the OS send
  buffer until TCP retransmission is exhausted — roughly 15 minutes on Windows.

  Measured on a tunnelled laptop entering Modern Standby, correlated against its own power log:
  the address read `live` for **16m13s** after one sleep and **17m39s** after another.

  Two consequences, both worse than a stale column:

  - **`peers` was the confidently-wrong indicator.** This project has repeatedly asserted that
    broker liveness is trustworthy *because* it is socket state rather than inference. It was socket
    state — of a socket the broker could not tell was dead.
  - **The phantom socket blocked recovery.** `/sub` answered **409** to the legitimate owner's
    reconnect for **15m31s across 35 attempts**, because `Hub::claim` saw an address "already held
    by a live socket". Failing to notice a dead client locked the address against its real holder.

  The broker now tracks the arrival of any incoming frame — pong included — and presumes the socket
  dead after **150s**, releasing the address. Deliberately more generous than the relay's 90s: the
  relay counts frames the broker is *guaranteed* to send, whereas the broker counts pongs a client
  is only *expected* to send, and disconnecting a slow-ponging client every cycle would be a worse
  failure than the one being fixed.

- **This also repairs the premise under 0.4.13.** That fix made `watch` require an affirmative
  `live` answer before announcing recovery, on the assumption that an affirmative answer is ground
  truth. Inside the stale window it was not: the broker said `live` while the session was deaf and
  being 409'd away, so a 0.4.13 watch would have reset its strikes and announced recovery on a
  socket that existed only in the broker's memory. Same door, different key — and only fixable in
  the broker.

  `docs/plan.md` is corrected accordingly, including retiring the "liveness is socket state, not an
  inference" claim as true-of-the-mechanism but misleading as a guarantee.

  Reported by the session it happened to, which measured the window twice independently, labelled
  the ~15-minute TCP mechanism as inferred rather than proven, and corrected its own power-event
  query first — it had filtered Kernel-Power id `566` as "sleep" when that is a session transition,
  missing the real `506` Modern Standby entry, and said so rather than quietly sending the fixed log.

## [0.4.13] — 2026-09-12

### Fixed

- **`subscription_recovered` was announced when the broker could not be reached at all.** The
  recovery branch fired whenever the tick produced no alarm — and "no alarm" includes the case
  where `broker_thinks_live` returned `None` because the broker was unreachable. So the session was
  told *"the broker lists this subscription as live again"*, a specific claim about a broker that
  was never contacted.

  The realistic sequence is not exotic: a subscription dies, the alarm fires correctly, and then the
  machine loses connectivity — which has been measured repeatedly on a tunnelled laptop, four
  sleep cycles in one night and two flaps inside half an hour. The next tick cannot reach the
  broker, and the session is told it has recovered while it is still deaf.

  That is this feature's own failure mode emitted by the feature itself: a component reporting a
  reality it did not verify. Recovery now requires the broker to **affirm** liveness — `Some(true)`,
  not merely the absence of an alarm.

### Added

- **The heartbeat's across-tick decisions are now tested.** Only the per-tick message formatting had
  coverage; the logic that accumulates strikes, resets them, latches the alarm and announces
  recovery lived inline in the reconnect loop with no tests at all — and that is the half that can
  over-suppress a real death or invent a recovery, as it turned out to be doing.

  Extracted into a `HeartbeatWatch` state machine and covered by seven sequence tests: one answer
  never fires, two consecutive fire exactly once and do not repeat, an intervening live answer
  resets the count, an unreachable broker resets the count, an unreachable broker is not evidence of
  recovery, a genuine recovery is announced once, and a second death after a recovery fires again.

  The extraction was made first as a faithful port **including the bug**, so the new test failed
  against it — which proved both that the defect was real and that the port had not quietly changed
  behaviour, before anything was fixed.

  Found by opening the source after asserting its behaviour from memory to a peer and getting it
  backwards. The claim was that a 12-minute outage should raise the alarm; the code says the
  opposite, deliberately, and the grep that settled it was one command away.

## [0.4.12] — 2026-09-11

### Fixed

- **`subscription_dead` fired on every resume, and a resume is not a death.** While a laptop is
  suspended the socket dies unannounced and the broker drops the address — and the relay's own 90s
  staleness check cannot run, because the CPU is stopped. So on waking there is a real window, up to
  that same 90s, where the broker truthfully answers *not live* and nothing is wrong that is not
  already repairing itself. The alarm fired inside it.

  That is the failure this feature exists to prevent, turned on itself: an alarm that cries on a
  healthy resume trains people to dismiss the one that matters. It is the same reasoning that set
  0.4.8's outage repeat to five minutes rather than thirty seconds.

  The alarm now requires the broker to answer *not live* on **two consecutive checks**. A genuine
  death is reported one interval later; measured against the five-hour outage that prompted the
  feature, that is nothing. A broker that cannot be reached resets the count rather than counting
  against the subscription — being unable to ask is not evidence, and the relay is already narrating
  that outage.

  A useful side effect: because a resume transient cannot survive two checks five minutes apart, any
  future firing is by construction a real death rather than a resume artefact — which settles the
  question the field capture could not.

  Reported from the machine that suspends, which correctly diagnosed it as the feature working
  rather than a new bug, and proposed the confirmation requirement.

- Restored a doc comment on `register_bound` that a previous edit had silently detached and left
  attached to an unrelated constant.

### Verified

- **Suspend/resume, listed as untested since Phase 7, is now exercised** — four sleep/resume cycles
  in one night, taken from the machine's own power log rather than inferred, with DHCP churn and a
  VPN client up throughout. The relay held a single pid for 604 minutes across all four, `watch`
  reconnected unaided every time, mail flowed afterwards and nothing was lost. `docs/plan.md` is
  updated in three places, including the one question this did **not** settle: whether the
  subscription is dead for the whole sleep or only across the resume boundary.

## [0.4.11] — 2026-09-10

### Fixed

- **A half-open upstream left a relay blocked for five hours, and it could not close itself.** The
  relay read from its upstream socket with no timeout and no liveness tracking. After a suspend, or
  a VPN client rewriting the route out from under an established connection, that read blocks on a
  socket the broker has already forgotten — and TCP does not notice until keepalive, which defaults
  to two hours.

  The broker pings every 30s precisely so an idle connection is distinguishable from a dead one, and
  the relay *received* those pings. It discarded them as transport noise without recording that they
  had arrived, so it never noticed when they stopped. Their arrival was the signal; throwing it away
  was the bug.

  The relay now treats an upstream that has delivered nothing — message, ping or pong — for three
  ping intervals as dead, and reconnects. Two lost pings are tolerated, because reconnecting on a
  single dropped packet would churn the subscription for every hiccup.

  This turns a five-hour outage into a ninety-second one. Measured in the field on the machine it
  happened to: the relay was never restarted, its own `/health` insisted the address was subscribed,
  and the broker listed it offline the entire time.

  0.4.10 made that failure *visible*; this makes it *recoverable*. The heartbeat is still worth
  having — it reports the case this cannot fix — but a fault the system repairs itself beats one it
  merely announces.

## [0.4.10] — 2026-09-10

### Added

- **`watch` now notices when its own subscription has died.** It was the last layer with no
  self-report, and the only one whose silence *is* the session going deaf. The relay announces its
  upstream outages; the broker knows who is connected; `watch` said nothing after its connect frame,
  forever.

  Reported by a session it happened to. The local indicators were not merely silent — they were
  wrong in a reassuring direction:

  | Indicator | Said | True? |
  |---|---|---|
  | `watch` process | alive, socket open | yes, and irrelevant |
  | relay `/health` | `subscribed: [that address]` | **no** |
  | `peers` | `offline` | **yes** |

  So the check cannot be derived from anything at this end. A heartbeat reporting "still subscribed"
  would have printed happily throughout, because `watch` believed it was subscribed — a third
  confidently-wrong indicator next to the other two, which is worse than none. `watch` now asks the
  **broker** whether it lists this address as live, because the broker's view is socket state rather
  than an inference, and it was the only indicator that was true.

  **Silence is the healthy state.** The check runs every five minutes; it speaks only when the
  broker affirmatively disagrees that the subscription exists, and once more when it recovers. A
  line every interval would be twelve notifications an hour per session, and a stream people learn
  to ignore fails exactly the way silence does — the same reasoning that made 0.4.8's outage repeat
  five minutes rather than thirty seconds.

  Elapsed silence is deliberately **not** a trigger on its own. The relay forwards real messages and
  not the broker's keepalive pings, so a quiet bus legitimately delivers nothing for hours; a
  duration alone cannot distinguish quiet from dead. It appears in the alarm as context, never as
  the cause.

  An unreachable broker stays silent too — the relay already announces upstream outages, and two
  components narrating one network failure is noise rather than redundancy.

## [0.4.9] — 2026-09-10

### Fixed

- **On a laptop, the 0.4.8 outage repeat reported `787m12s, 9 attempts` — which reads as a wedged
  retry loop.** One attempt per 87 minutes is impossible against a 30s cap, so an operator seeing it
  goes debugging the relay. It was not a counter bug: `elapsed` is wall clock and keeps advancing
  while the process is suspended, while `attempts` counts only attempts that actually ran. On a
  machine that sleeps, the two fields describe different universes.

  0.4.8 exists so that "still trying" cannot be mistaken for "dead". Left alone it reintroduced the
  same misreading in a narrower case — and suspension is the ordinary case on any laptop, not an
  edge case.

  The relay now also accumulates **retry time**: measured connect attempts plus the backoff it
  intended to sleep. That advances only while the process runs, so wall clock minus retry time is
  time the process cannot account for. When the gap dwarfs the retry work, the frame says so:

  ```
  unreachable for 787m12s, 9 attempts, next in 30s (only 8m00s of that was spent retrying -
  this process was suspended or descheduled for the other 779m12s, so the attempt count is
  low for honest reasons). Current error: ...
  ```

  The tolerance is deliberately generous — a gap counts only when it exceeds the retry work itself —
  so an uninterrupted outage carries no qualifier at all. A warning that fires when nothing is wrong
  is the same trained-to-ignore failure as chatter, which is why 0.4.8 chose five minutes over
  thirty seconds in the first place.

  Found by a field test on the machine that actually loses its link, not by reasoning: the tunnel
  was toggled by hand, the frames captured verbatim, and the divergence appeared only afterwards
  when the laptop was put to sleep. Confirmed against `Power-Troubleshooter` events — the two
  reported elapsed figures land exactly on the sleep and wake boundaries.

### Verified

- **The 0.4.8 repeat behaved as designed under a real outage**, measured rather than asserted:
  repeat intervals of 306s and 307s against a nominal 300s, `+6` attempts each time, `next in`
  reading 30 on every frame, and recovery 29s after the path returned — inside the 30s the backoff
  cap implies. The reset is clean: a second, short outage produced one frame in the original wording
  with no elapsed, no attempt count and no repeat.

- **Three predictions were wrong, and the measurements are worth keeping.** Attempts accrue at
  ~1.18/min, not ~2/min, because a `WSAETIMEDOUT` connect blocks ~21s before returning, making each
  cycle ~51s rather than 30s. The first frame therefore lands at 83–117s, not the ~30s the backoff
  schedule alone suggests. And `elapsed` runs from **detection**, not from the socket drop — on a
  timeout-shaped outage it under-reports by roughly one connect timeout (34s, measured). That last
  one is a known limitation, not fixed here: the relay sets its outage clock on the first failed
  connect rather than when the upstream closed.

## [0.4.8] — 2026-09-08

### Fixed

- **A long outage announced itself once and then went silent, which is indistinguishable from a
  broker that is never coming back.** The relay retries forever with a backoff capped at 30s, so it
  always knew it was still working — it just stopped saying so. `announced_down` was a latch: set
  when the backoff first crossed the threshold, never cleared while the outage continued. A
  forty-minute outage therefore emitted exactly one `upstream_unreachable` frame at around the
  thirty-second mark and nothing for the remaining thirty-nine and a half minutes.

  From inside a session those two situations produce identical evidence: one frame, then nothing.
  The relay is the only component that can tell them apart, and it was choosing not to.

  It now re-announces every five minutes while the outage continues, carrying the four things
  silence cannot: how long it has been down, how many attempts it has made, when the next one is,
  and **the current error**. That last field is the diagnostic one. On Windows a tunnelled path
  reports `10065` (WSAEHOSTUNREACH — no route) when the route is gone and `10060` (WSAETIMEDOUT —
  route exists, nothing answered) when it is back but the far end is not yet responding, so a code
  that changes mid-outage says the path is moving rather than dead. Watching that sequence is how
  an intermediate hop coming back is distinguished from a broker that is simply down.

  Five minutes, not every retry: at a 30s cadence the latter would be chatter, and an event stream
  people learn to ignore is the same failure wearing a different hat.

  Reported from a machine that hit it — an away laptop reaching the bus through a tunnel, which is
  precisely the topology where "still trying" and "dead" most need telling apart, and where the
  operator had to run route lookups by hand to find out which.

## [0.4.7] — 2026-09-06

### Fixed

- **`ack` on an address nothing answers to reported success instead of refusing.** It resolved
  aliases, then wrote a cursor unconditionally — so a name with no registration behind it got a
  cursor row of its own and the caller got `{"ok": true}`. Now HTTP **404**, with the address named.

  The stray row was never the problem. The confirmation was. The realistic way to reach this is a
  typo: a session acks `machine-a/tool` instead of `machine-a/tools`, is told it worked, and stops looking —
  while its real mailbox keeps every message unacked and replays the whole backlog on every
  reconnect. Every indicator the session can see says healthy, and the one command that would have
  revealed the problem is the one it believes it already ran.

  That is the same shape as the two bugs already fixed in 0.4.2 and 0.4.3, and it is the shape worth
  naming: **an operation that cannot fail is indistinguishable from one that did nothing.** The bus
  is a place where "it worked" is often the only evidence anyone gets, so an acknowledgement that
  acknowledges nothing is worse than an error.

  Aliases still ack normally — a migrated-away name is a legitimate target, which is the entire
  point of the alias, so the check resolves through `mailbox_of` before deciding. A test covers that
  case specifically, because the naive version of this fix breaks migration.

  Found by noticing that `forget` said *"was not registered"* about an address `ack` had just
  reported success for. Two commands disagreeing about whether something exists.

### Changed

- `/ack` now distinguishes its two client errors by status: **400** for a malformed cursor,
  **404** for a well-formed ack aimed at a mailbox that does not exist. A caller can tell "I sent
  nonsense" from "I sent it to the wrong name" without parsing the message.

## [0.4.6] — 2026-09-06

### Fixed

- **A refused `ack` is now HTTP 400 instead of 500, and no longer reports itself as a broker
  fault.** The cursor guard added in 0.4.3 works — it correctly refuses to move a mailbox's cursor
  to a non-id, which would silence that mailbox permanently. But it was implemented by returning a
  `rusqlite::Error`, and the `/ack` handler mapped every store error to 500. Two things followed
  from that, both wrong in the same direction: the caller was told the **server** had broken when
  in fact their own call was malformed, and rusqlite's `Display` glued **`Invalid parameter name:`**
  onto the front of a message that has nothing to do with SQL parameters.

  The combination is worse than either half. A 500 with an internal-looking prefix is the signature
  of a transient server fault, so the reasonable response is to retry — and retrying is exactly what
  cannot work here, because the request will be refused identically every time. The guard was
  telling the one caller who could fix the problem to do the one thing that never fixes it.

  Found by exercising the guard against the freshly-updated broker rather than trusting that a
  passing unit test meant the whole path was right: the store-level test asserted the refusal, and
  said nothing about how the refusal reached a client.

  The check now lives in `store::cursor_refusal`, called by the HTTP layer *before* the store, so
  the refusal is answered as the client error it is. The store keeps its own guard for any caller
  that bypasses HTTP, and both read their text from that one function so the two paths cannot drift
  into telling a caller two different things.

## [0.4.5] — 2026-09-06

### Added

- **`whoami` now names old addresses that will silently strand mail sent to them.** Moving to a
  repo-scoped address does not remove the session-suffixed registration it replaced. Anyone still
  holding the old name sends there, the message queues where nothing is listening, and neither end
  sees a fault — the sender is told "queued for a known address", which is true and useless.

  This is the gap the 0.3.0 rollout opened and nothing reported. Measured on the live bus the day
  after every session moved: **12 such addresses holding 23 unread messages**, and the only aliased
  one belonged to the session that had done it by hand. Two sessions found it independently, each by
  probing their own old name, which is the expensive way to learn something a command could have
  told them.

  `whoami` lists them worst-first with their unread counts and prints the `migrate` command for
  each. Silent when there is nothing to say, and silent for an address that has already been
  migrated — a warning that fires when everything is fine trains people to ignore it.

  Matched on an exact `<address>.<suffix>` prefix, so `machine-a/tools-extra.abc` is never claimed as a
  sibling of `machine-a/tools`, and neither is another machine's copy of the same repo name.

## [0.4.4] — 2026-09-06

Documentation only — no code change. Both entries come from getting something wrong in public and
from a peer proposing something better.

### Changed

- **A body that names its reader must not be broadcast** is now a rule in the etiquette section,
  because this repo's own session broke it: one urgent, personalised body went to seven addresses in
  a loop, and six sessions were told to run `forget` on a seventh's address — which would have
  retired another session's registration and stranded its unread mail. Five refused and flagged it,
  which is the receiving-side rule working exactly as written.

  Recorded with the part that makes it interesting: **the send confirmation added in 0.4.2 cannot
  catch this.** `--to` was correct on all seven sends, so the control built to catch a wrong address
  had nothing to report. A confirmation says *where* a message went, never whether the words were
  written for whoever is there. Right header with a wrong body is a different defect from a stale
  `--to`, and it needs a habit rather than a tool.

- **Checking a cursor now costs one command, not a probe.** A **non-zero `pending` in `peers` is
  positive proof the cursor is healthy**, since a poisoned one matches nothing and reads 0 forever.
  Proposed by a peer session as a zero-risk alternative to the throwaway-address test this repo had
  been recommending, which costs a disconnect. Documented as the one-way test it is: zero pending
  proves nothing, because that is also what an empty mailbox looks like.

  The `ack` section now also carries the poisoned-cursor failure and the `forget`+`register` repair,
  including the warning to check `read --since` first — on a poisoned cursor `pending` reads 0
  whether or not mail is waiting, so the repair can strand exactly what nobody can see.

## [0.4.3] — 2026-09-06

### Fixed

- **`ack` accepted a string that was not an id, reported success, and silenced the mailbox
  permanently.** Reported by a session whose id extraction (`grep "^id"`) matched a line of prose
  inside a message *body*, so `ack` was handed an English sentence — and took it.

  The reporter checked afterwards and believed the outcome was right by luck, because nothing unread
  was in range. It was worse than that, and a test now proves it: cursors are compared
  lexicographically, every real id begins with a digit, and almost any prose sorts above a digit. The
  cursor lands beyond every id that can **ever** be minted, and `pending_for`'s `id > cursor` never
  matches again. Not "a few skipped" — every future message, silently, with nothing in `peers` or
  `pending` to show why.

  `ack` now refuses anything that is not `YYYYMMDDThhmmssmmm-nnnnnnnnn`, with an error saying what it
  refused and why. An error is recoverable; a silenced inbox is not. The guard is in the store, so
  every caller is covered, and the tests were verified to fail without it.

- **The send confirmation is now one self-contained line.** `2>&1 | tail -1` is common in exactly the
  scripted sends most at risk, and it split the two-line form — potentially keeping the half without
  the recipient.

### Changed

- **`docs/usage.md` corrects its own account of the 0.4.2 gap.** It implied no warning existed. The
  **orphan** warning always existed and works — it fires when no registration answers to the
  recipient. But a misaddressed message usually goes to a real, registered address, just the wrong
  one, so nothing is orphaned and that check correctly stays silent. A control scoped to unresolvable
  addresses cannot catch a valid wrong one, and calling them both "the confirmation line" hid the
  difference. Credit to the session that drew the distinction.

## [0.4.2] — 2026-09-06

### Fixed

- **`send` now names the recipient it actually used.** `docs/usage.md` has told senders since 3 Sep
  that the send's output "names the recipient — the only signal that would catch a wrong address".
  It did not. `send` printed the message id and nothing else, so the safeguard the documentation
  pointed at did not exist.

  In the week that advice was in place, the misaddressed send it describes happened **three times**,
  twice between the same pair of sessions. It survives review every time for the same reason: a send
  is built by editing a previous command, the body and subject are rewritten and correct, and only
  `--to` — the one field an edit leaves alone — is stale. A documented safeguard that is absent is
  worse than a missing one, because people stop looking for what it was meant to catch.

  The confirmation goes to **stderr**, not stdout, so `ID=$(agent-msg-bus send …)` — the usual way
  to capture the id for a later `ack` — cannot hide it. Direction is explicit (`from  ->  to`)
  rather than positional, because naming both addresses without saying which is which still lets a
  glance land on the wrong one.

## [0.4.1] — 2026-09-05

### Fixed

- **`whoami` printed a subscribe command for an address it warned against three lines later.**
  Reported from a live session that picked up 0.4.0, read both halves, and correctly refused to
  follow the printed line. The recommendation and the diagnosis were computed independently, so
  nothing stopped them disagreeing — and the whole reason `whoami` prints the command is so the
  reader does not have to arbitrate. Both now derive from one `RegStatus`, and a test asserts that
  no status can both warn and recommend a bare subscribe.

  The reporter's point about why it was not merely cosmetic is the reason it is a release on its
  own: whether the printed line was safe depended on a race they could not see. It was harmless only
  while something else held the address and the fallback fired; had that claimant gone away, the
  same line would have bound them to an unregistered address — a socket that connects, a `peers`
  entry that says live, and mail that never arrives.

- **The advice for an unclaimed address was left over from session-derived addressing.** It said
  *"do not subscribe to it: the inbox is silently dead"*, which was right when an unregistered
  derived address meant a phantom minted from the wrong directory. Since 0.3.0 an unregistered
  repo address is usually just a mailbox nobody has claimed yet — a missing step, not a hazard — so
  the advice now names the fix (`register <addr>`) instead of sending a session away from its own
  correct address. The genuine wrong-directory case, where the same session already has a mailbox
  under another name, keeps its original warning: registering there would mint a second mailbox and
  split the session's mail in two.

## [0.4.0] — 2026-09-05

Nothing on the wire carried a version, so "is the fix deployed over there?" could only be answered by
hashing files on each machine. This release answers it, and adds a way to act on the answer that
cannot take a machine off the bus.

### Added

- **Versions on the wire.** A client sends its build with every `register` — attached by `Client`
  itself, never by a caller, so it cannot disagree with the binary that made the request. `peers`
  shows a version column and names any address on a different build; `/health` on both the broker
  and the relay reports its own. A registration made before this existed reads as `?`, which is
  deliberately distinguishable from a real version rather than rendered as if it were current.

- **`agent-msg-bus update` — replaces the binary without stopping anything.** A running executable
  cannot be overwritten but *can be renamed*, so the installed binary is moved aside (its version
  kept in the filename, so rollback needs no guesswork) and the new one copied into the freed path.
  Processes already running keep executing the renamed file, undisturbed.

  This matters more than it sounds. The installer kills every `agent-msg-bus.exe` before copying —
  correct for a first install, and a bad way to ship an update, because it ends every session's
  `watch` and stops the relay. On a machine whose relay supervisor cannot relaunch it, that is the
  difference between an update and a bus outage with no automatic way back. `update` cannot cause
  that, because it never kills or restarts anything.

  The cost is that long-lived processes stay on the old build until something restarts them, so the
  command **reports exactly which ones** instead of pretending to be finished. It also refuses a
  source binary that cannot report its own version, rather than installing a file it never checked.

- **Sessions are told when their machine is behind, on both paths.** A live session learns from its
  `watch`, which emits a `version_skew` line on its first connect — that is the only channel into an
  already-running session's transcript. A session that was *not* running has no socket to be told on,
  so the SessionStart hook leads its banner with the mismatch instead. Neither ever updates anything
  on its own: replacing a binary is a change to a machine, and this project's own rule is that those
  go to the human.

  A component that answers `/health` without a version is reported as **pre-0.2.0**, not as unknown —
  `--version` is what 0.2.0 added, so silence dates it. One that cannot be reached at all is reported
  as nothing, because it is not evidence about a build and guessing would cry wolf on every session
  started during an outage.

## [0.3.1] — 2026-09-05

### Fixed

- **A session that could bind neither its repo address nor its fallback went deaf silently.** Both
  names held is the one case the fallback cannot rescue, and `watch` reported it only with an
  `eprintln!` — stderr, which the subscribing side turns into nothing, while stdout lines become
  notifications. So the session retried forever, received nothing, and said nothing: the fallback
  wearing the exact disguise this project exists to strip off. It is now announced immediately, on
  stdout, naming both held addresses — and immediately rather than after the reconnect backoff
  grows, because this is not a transient outage and waiting 30 seconds to mention it helps nobody.
  `watch::is_conflict` is public so the condition can be asserted rather than matched on error text.

## [0.3.0] — 2026-09-05

The release where the README's oldest promise — *"an address outlives its session"* — becomes true.

**Deployable as a client-only change.** Two sessions in one repo are on one machine and therefore one
relay, and the relay answers 409 from its own local state before opening anything upstream. So the
identity change works against an unmodified broker; reinstalling clients is enough.

### Changed

- **An address is now the repo's, not the session's**: `<machine>/<repo>`, with no session id in it.
  It used to be `<machine>/<repo>.<session-id>` — a session id sitting in the slot the address format
  reserves for a *role*, which keyed identity to a process lifetime instead of a working context. So
  every restart minted a fresh empty mailbox and the previous one's mail was stranded under a name
  nothing would answer to again. Measured before the change: **27 unread messages across 14 dead
  addresses**, one repo holding five of them.

  The suffix existed to stop two sessions in one repo sharing a mailbox. That is still prevented —
  by the one-socket-per-address rule, which decides on socket state rather than inferring from a
  process id. A second live session in a repo is refused with a 409 at claim time and binds
  `<machine>/<repo>.<session-prefix>` instead, announcing that it did so.

- **The SessionStart banner now hands out `Monitor({command: "… watch …"})`, not `Monitor({ws: …})`.**
  It had been recommending `ws:` while `watch`'s own docstring said not to — a `ws:` watch ends when
  its socket closes and does not retry, so a relay restart left the session silently deaf. The banner
  was simply never updated when `watch` landed. `ws:` still works and is now the deprecated path; it
  cannot fall back, because it cannot react to a 409.

### Added

- `watch --fallback <addr>`: bind this name if the primary is already held. The decision is
  first-connect only and **sticky** — a 409 on reconnect is almost always this watcher's own socket
  not yet released, and falling back twice would change a session's identity mid-life. A **pinned**
  address is deliberately offered no fallback: a pin is an explicit claim, so a collision on one is
  an error to surface, not a cue to answer to a different name.
- `whoami` now reports the **bound** address, read from the relay rather than re-derived, alongside
  the primary and the fallback. Deriving it twice only reproduces the same guess, so a session that
  had fallen back was previously told, confidently, an address nothing was listening on.

### Fixed

- **Subscribing an unregistered address replayed the entire history to it.** `/sub` called only
  `promote`, an UPDATE that does nothing without a row, so such an address had no registry row and no
  cursor — and a missing cursor reads as the empty string, which sorts before every id. Harmless
  while every address came from the hook; the common path as soon as a client can bind a fallback
  name nobody registered. `/sub` now ensures a row and a cursor at the head, without overwriting the
  metadata of an address that already has them.

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
