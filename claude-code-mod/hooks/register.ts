// agent-msg-bus-inbox: how much bus mail is waiting for this session, shown without a model turn.
//
// - The session's address comes from `agent-msg-bus whoami --json`, so this module knows nothing of
//   how addresses are derived, pinned or fallen back. It reads mail from the `inbox` field.
// - The count comes from `read <inbox> --unread --json`: read-only on the broker, no cursor moves.
// - It goes to the status line, and a toast when it rises. Nothing else.
//
// Three things it never does, by design:
// - It never acks. Acking is the session's deliberate step.
// - It never shows a body.
// - It never puts mail where the model reads it. A subject is another session's words, so it goes to
//   the person's screen only: no prompt, no context, no transcript row.

import type { EngineInterface, Register } from 'claude-code'

type Header = { id: string; from: string; kind: string; subject: string }
type Inbox = { addr: string; total: number; messages: Header[] }
type Me = { address: string; inbox: string; pinned: boolean; fallback: string | null }
type Ran = { ok: boolean; code: number | undefined; stdout: string; why: string }

/** What one load of the module knows. A reload starts it over, and `session.start` fires again. */
type State = {
  bin: string
  session: string
  mode: 'poll' | 'watch'
  me: Me | undefined
  /** No inbox will ever be found by this load: a binary too old for --json, or no bus config. */
  gaveUp: boolean
  watching: boolean
  /** The count last shown; undefined until the first read, so a first read that finds mail toasts. */
  last: number | undefined
  busy: boolean
  again: boolean
  down: boolean
  explained: boolean
}

const MIN_INTERVAL_S = 15
const WATCH_CHECK_MS = 5 * 60_000
const WATCH_RESTART_MS = 30_000
const RUN_TIMEOUT_MS = 15_000
const SHOWN = 20
const SUBJECT_MAX = 100

/**
 * The subcommands that may change what is waiting, or which address is ours. One counts only as
 * the word after the binary: its name or a path ending in it, maybe closed by a quote, or a shell
 * variable holding that path. The name is also a folder and part of this plugin's name, so a word
 * anywhere else in a command that only passes through them is not a run of the CLI.
 */
const SUBCOMMAND = '(ack|read|send|pin|unpin|register|migrate|forget)'
const DIRECT = new RegExp(`agent-msg-bus(?:\\.exe)?["']?\\s+${SUBCOMMAND}\\b`, 'g')
const ASSIGNED = /\$?([A-Za-z_]\w*)\s*=\s*["']?[^"'\s;]*agent-msg-bus(?:\.exe)?["']?(?=[\s;]|$)/g
const CHANGES_ADDRESS = new Set(['pin', 'unpin', 'register', 'migrate'])

/** The bus subcommands `command` runs, in no particular order. */
export function busSubcommands(command: string): string[] {
  const words = (re: RegExp) => [...command.matchAll(re)].flatMap(m => (m[1] ? [m[1]] : []))
  const found = words(DIRECT)
  for (const name of words(ASSIGNED)) {
    found.push(...words(new RegExp(`\\$\\{?${name}\\}?["']?\\s+${SUBCOMMAND}\\b`, 'g')))
  }
  return found
}

/** One line of at most SUBJECT_MAX characters, with nothing in it that could act on a terminal. */
export function clip(text: string): string {
  const flat = text.replace(/[\u0000-\u001f\u007f-\u009f]+/g, ' ').replace(/\s+/g, ' ').trim()
  return flat.length > SUBJECT_MAX ? `${flat.slice(0, SUBJECT_MAX - 1)}…` : flat
}

export function toastText(inbox: Inbox): string {
  const head = `bus: ${inbox.total} unread for ${inbox.addr}`
  // Oldest first: the newest is the last one, and only when everything waiting was shown.
  const newest = inbox.total <= inbox.messages.length ? inbox.messages.at(-1) : undefined
  if (newest === undefined) return head
  const kind = newest.kind && newest.kind !== 'fyi' ? `[${clip(newest.kind)}] ` : ''
  return `${head} · newest: ${kind}${clip(newest.from)}: ${clip(newest.subject)}`
}

function parse(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return undefined
  }
}

function isInbox(v: unknown): v is Inbox {
  const o = v as Partial<Inbox> | undefined
  return typeof o?.total === 'number' && typeof o.addr === 'string' && Array.isArray(o.messages)
}

function isMe(v: unknown): v is Me {
  const o = v as Partial<Me> | undefined
  return typeof o?.inbox === 'string' && o.inbox !== '' && typeof o.address === 'string'
}

async function run($: EngineInterface, s: State, args: string[]): Promise<Ran> {
  try {
    const r = await $.process.run([s.bin, ...args], {
      env: { CLAUDE_CODE_SESSION_ID: s.session },
      timeoutMs: RUN_TIMEOUT_MS,
    })
    const why = r.stderr.trim().split('\n')[0] ?? ''
    return { ok: r.exitCode === 0, code: r.exitCode, stdout: r.stdout, why: why || `exit ${r.exitCode}` }
  } catch (err) {
    // Could not start, or still running at the timeout.
    return { ok: false, code: undefined, stdout: '', why: err instanceof Error ? err.message : String(err) }
  }
}

/**
 * Whether a failed `whoami --json` will fail the same way every time. Exit 2 is a usage error: a
 * binary that predates --json. Output that is not the JSON expected is a binary that disagrees with
 * this module. No config means no bus on this machine. Anything else, a timeout above all, is a
 * broker or network that may come back.
 */
function isPermanent(r: Ran): boolean {
  return r.ok || r.code === 2 || /no config/i.test(r.why)
}

/** Once per load, a dim transcript line the model does not read, saying why nothing shows. */
function explain($: EngineInterface, s: State, text: string): void {
  if (s.explained) return
  s.explained = true
  $.ui.log(`agent-msg-bus-inbox: ${text}`)
}

async function identify($: EngineInterface, s: State): Promise<boolean> {
  const r = await run($, s, ['whoami', '--json'])
  const v = r.ok ? parse(r.stdout) : undefined
  if (!isMe(v)) {
    s.me = undefined
    if (isPermanent(r)) {
      s.gaveUp = true
      $.ui.status(undefined)
      const why = clip(r.ok ? 'its output was not the JSON expected' : r.why)
      explain($, s, `no inbox to show: \`${s.bin} whoami --json\` failed (${why}). It needs agent-msg-bus 0.4.27 or later and a bus config on this machine.`)
    } else {
      // Asked again on the next tick: a laptop off the network is the normal case.
      if (!s.down) $.ui.status('bus: unreachable')
      s.down = true
    }
    return false
  }
  if (s.me?.inbox !== v.inbox) s.last = undefined
  s.me = v
  return true
}

async function readOnce($: EngineInterface, s: State): Promise<void> {
  if (s.me === undefined) return
  const r = await run($, s, ['read', s.me.inbox, '--unread', '--json', '--limit', String(SHOWN)])
  const inbox = r.ok ? parse(r.stdout) : undefined
  if (!isInbox(inbox)) {
    // Said once, in the status line, and never as a toast: an unreachable broker on a laptop that
    // is off the network is the normal case, not news every minute.
    if (!s.down) $.ui.status('bus: unreachable')
    s.down = true
    return
  }
  s.down = false
  $.ui.status(inbox.total > 0 ? `bus: ${inbox.total} unread` : undefined)
  if (inbox.total > (s.last ?? 0)) $.ui.toast(toastText(inbox))
  s.last = inbox.total
}

/** One read at a time. A request during a read runs one more read after it, not one each. */
async function refresh($: EngineInterface, s: State): Promise<void> {
  if (s.busy) {
    s.again = true
    return
  }
  s.busy = true
  try {
    do {
      s.again = false
      await readOnce($, s)
    } while (s.again)
  } finally {
    s.busy = false
  }
}

async function reidentify($: EngineInterface, s: State): Promise<void> {
  if (await identify($, s)) await refresh($, s)
}

function onWatchLine($: EngineInterface, s: State, line: string): void {
  const frame = parse(line) as Record<string, unknown> | undefined
  if (frame === undefined || frame === null || typeof frame !== 'object') return
  const state = frame._watch
  if (typeof state === 'string') {
    // `connected` names the address actually bound, which may be the fallback: ask again.
    if (state === 'connected') void reidentify($, s)
    else if (state !== 'version_skew') $.ui.status(`bus: watch ${clip(state)}`)
    return
  }
  void refresh($, s)
}

async function watchOnce($: EngineInterface, s: State): Promise<void> {
  if (s.me === undefined) return
  const args = ['watch', s.me.address]
  if (s.me.fallback !== null && !s.me.pinned) args.push('--fallback', s.me.fallback)
  let buf = ''
  try {
    const child = $.process.spawn({ argv: [s.bin, ...args], env: { CLAUDE_CODE_SESSION_ID: s.session } })
    for await (const piece of child) {
      if (piece.stream !== 'stdout') continue
      buf += piece.text
      for (let nl = buf.indexOf('\n'); nl >= 0; nl = buf.indexOf('\n')) {
        onWatchLine($, s, buf.slice(0, nl).trim())
        buf = buf.slice(nl + 1)
      }
    }
  } catch {
    // Could not start, or ended badly: handled as an end, below.
  }
}

/** `watch` reconnects by itself, so its ending means it gave up or was stopped: start another. */
function startWatch($: EngineInterface, s: State): void {
  s.watching = true
  void watchOnce($, s).then(() => {
    $.ui.status('bus: watch stopped; restarting')
    $.clock.after(WATCH_RESTART_MS, () => startWatch($, s))
  })
}

/** Find the inbox if it is not known yet, start the watch if one is wanted, then read. */
async function tick($: EngineInterface, s: State): Promise<void> {
  if (s.gaveUp) return
  if (s.me === undefined && !(await identify($, s))) return
  if (s.mode === 'watch' && !s.watching) startWatch($, s)
  await refresh($, s)
}

async function begin($: EngineInterface, s: State, binary: unknown, intervalMs: number): Promise<void> {
  if (typeof binary === 'string' && binary !== '') s.bin = binary
  else {
    const local = await $.env.get('LOCALAPPDATA')
    if (local) s.bin = `${local}\\agent-msg-bus\\agent-msg-bus.exe`
  }
  s.session = await $.session.id()
  await tick($, s)
  if (s.gaveUp) return
  // The same tick retries `whoami` until it answers, so a session started off the network finds
  // its inbox once the network is back. Watch mode reads on its own; its tick is a check.
  $.clock.every(s.mode === 'watch' ? WATCH_CHECK_MS : intervalMs, () => void tick($, s))
}

/** After a shell command that ran the bus CLI, read again: an `ack` should clear the count now. */
function afterCommand($: EngineInterface, s: State, tool: string, command: unknown): void {
  if (tool !== 'Bash' && tool !== 'PowerShell') return
  if (typeof command !== 'string') return
  const ran = busSubcommands(command)
  if (ran.length === 0) return
  if (ran.some(c => CHANGES_ADDRESS.has(c))) void reidentify($, s)
  else void refresh($, s)
}

export const register: Register = (on, options) => {
  const seconds = Number(options.intervalSeconds)
  const intervalMs = Math.max(MIN_INTERVAL_S, Number.isFinite(seconds) ? seconds : 60) * 1000
  const s: State = {
    bin: 'agent-msg-bus',
    session: '',
    mode: options.mode === 'watch' ? 'watch' : 'poll',
    me: undefined,
    gaveUp: false,
    watching: false,
    last: undefined,
    busy: false,
    again: false,
    down: false,
    explained: false,
  }

  // Started, not awaited: a `whoami` that waits on an unreachable broker must not hold up the
  // session's first prompt. The timers it sets run until the module reloads.
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    void begin($, s, options.binary, intervalMs)
    return started
  })

  // It never blocks or changes the call: the result goes back as it came, the reads it starts run
  // after, and a failure here is skipped rather than turned into a refusal.
  on('tool.call', async ($, e, next) => {
    const result = await next(e)
    afterCommand($, s, e.tool, (e as { command?: unknown }).command)
    return result
  }).catch(($, e, next) => next(e))
}
