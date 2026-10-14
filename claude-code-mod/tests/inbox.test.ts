// The mod against a stand-in for the agent-msg-bus binary: every `$.process.run` lands in the
// test's hook, which answers as the real CLI does. What the person would see is what the test
// reads: the status line, the toasts and the dim log lines.

import type { On } from 'claude-code'
import { describe, expect, mock, test } from 'claude-code/testing'
import type { Engine } from 'claude-code/testing'

const ME = {
  address: 'machine-a/repo',
  pinned: false,
  fallback: 'machine-a/repo.0123abcd',
  bound: null,
  inbox: 'machine-a/repo',
  registered: true,
  broker: 'http://192.0.2.1:9450',
  relay: '127.0.0.1:9451',
}

type Header = { id: string; from: string; kind: string; subject: string }

/** What the stand-in binary knows, and everything the mod did to the screen. */
type World = {
  whoami: string | null
  waiting: Header[]
  readFails: boolean
  runs: string[][]
  envs: Record<string, string>[]
  statuses: (string | undefined)[]
  toasts: string[]
  logs: string[]
  clock: ReturnType<typeof mock.clock>
}

function header(n: number, extra: Partial<Header> = {}): Header {
  return { id: `20261014T1200000${n}-00000000${n}`, from: 'machine-a/peer', kind: 'fyi', subject: `subject ${n}`, ...extra }
}

function world(on: On): World {
  const w: World = {
    whoami: JSON.stringify(ME),
    waiting: [],
    readFails: false,
    runs: [],
    envs: [],
    statuses: [],
    toasts: [],
    logs: [],
    clock: mock.clock(on),
  }
  mock.env(on, { LOCALAPPDATA: 'C:\\L' })
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  // A call on `$` is answered with `{ value }`, a void one with `{ value: undefined }`.
  on('session.id', () => ({ value: '0123abcd-0000-0000-0000-000000000000' }))
  on('process.run', ($, e) => {
    w.runs.push([...e.argv])
    w.envs.push({ ...(e.init?.env ?? {}) })
    const answer = (exitCode: number, stdout: string, stderr = '') => ({
      value: { exitCode, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false },
    })
    if (e.argv[1] === 'whoami') {
      return w.whoami === null ? answer(2, '', "error: unexpected argument '--json' found\n") : answer(0, w.whoami)
    }
    if (e.argv[1] === 'read') {
      if (w.readFails) return answer(1, '', 'agent-msg-bus: cannot reach the broker\n')
      const limit = Number(e.argv[e.argv.indexOf('--limit') + 1])
      return answer(0, JSON.stringify({ addr: e.argv[2], total: w.waiting.length, messages: w.waiting.slice(0, limit) }))
    }
    return answer(1, '', 'not stood in for\n')
  })
  on('ui.status', ($, e) => {
    w.statuses.push(e.text)
    return { value: undefined }
  })
  on('ui.toast', ($, e) => {
    w.toasts.push(e.text)
    return { value: undefined }
  })
  on('ui.log', ($, e) => {
    w.logs.push(e.text)
    return { value: undefined }
  })
  on('tool.call', () => ({ result: { stdout: '', stderr: '' } }))
  return w
}

async function start($: Engine): Promise<void> {
  await $.session.start({ cwd: 'C:\\w\\repo', surface: 'terminal', isInteractive: true })
}

const reads = (w: World) => w.runs.filter(r => r[1] === 'read').length

describe('finding the inbox', () => {
  test('asks the binary for the address, with the session id, and reads the inbox it names', async ($, on) => {
    const w = world(on)
    await start($)
    expect(w.runs[0]).toEqual(['C:\\L\\agent-msg-bus\\agent-msg-bus.exe', 'whoami', '--json'])
    expect(w.envs[0]).toEqual({ CLAUDE_CODE_SESSION_ID: '0123abcd-0000-0000-0000-000000000000' })
    expect(w.runs[1]).toEqual(['C:\\L\\agent-msg-bus\\agent-msg-bus.exe', 'read', 'machine-a/repo', '--unread', '--json', '--limit', '20'])
  })

  test('reads the bound name when the session fell back', async ($, on) => {
    const w = world(on)
    w.whoami = JSON.stringify({ ...ME, bound: 'machine-a/repo.0123abcd', inbox: 'machine-a/repo.0123abcd' })
    await start($)
    expect(w.runs[1]?.[2]).toBe('machine-a/repo.0123abcd')
  })

  test('a binary option replaces the default path', { options: { binary: '/opt/amb/agent-msg-bus' } }, async ($, on) => {
    const w = world(on)
    await start($)
    expect(w.runs[0]?.[0]).toBe('/opt/amb/agent-msg-bus')
  })

  test('an old binary without --json: one dim line saying why, then nothing', async ($, on) => {
    const w = world(on)
    w.whoami = null
    await start($)
    await w.clock.advance(10 * 60_000)
    expect(w.logs.length).toBe(1)
    expect(w.logs[0]).toContain('0.4.27')
    expect(reads(w)).toBe(0)
    expect(w.toasts).toEqual([])
  })
})

describe('what the person sees', () => {
  test('nothing waiting: no status line and no toast', async ($, on) => {
    const w = world(on)
    await start($)
    expect(w.statuses).toEqual([undefined])
    expect(w.toasts).toEqual([])
  })

  test('mail waiting at the start: the count, and one toast naming the newest', async ($, on) => {
    const w = world(on)
    w.waiting = [header(1), header(2, { kind: 'request', from: 'machine-b/other', subject: 'please check' })]
    await start($)
    expect(w.statuses.at(-1)).toBe('bus: 2 unread')
    expect(w.toasts).toEqual(['bus: 2 unread for machine-a/repo · newest: [request] machine-b/other: please check'])
  })

  test('a toast only when the count rises, and the status clears when it falls to 0', async ($, on) => {
    const w = world(on)
    w.waiting = [header(1)]
    await start($)
    expect(w.toasts.length).toBe(1)

    await w.clock.advance(60_000)
    expect(w.toasts.length).toBe(1)

    w.waiting = [header(1), header(2)]
    await w.clock.advance(60_000)
    expect(w.toasts.length).toBe(2)
    expect(w.statuses.at(-1)).toBe('bus: 2 unread')

    w.waiting = []
    await w.clock.advance(60_000)
    expect(w.toasts.length).toBe(2)
    expect(w.statuses.at(-1)).toBe(undefined)
  })

  test('more waiting than shown: the toast gives the count and does not guess the newest', async ($, on) => {
    const w = world(on)
    w.waiting = Array.from({ length: 25 }, (_, i) => header(i))
    await start($)
    expect(w.toasts).toEqual(['bus: 25 unread for machine-a/repo'])
  })

  test('a subject cannot put a second line or a control sequence on screen', async ($, on) => {
    const w = world(on)
    w.waiting = [header(1, { subject: 'line one\nline two\u001b[2J' + 'x'.repeat(300) })]
    await start($)
    const toast = w.toasts[0] ?? ''
    expect(toast).not.toContain('\n')
    expect(toast).not.toContain('\u001b')
    expect(toast.length).toBeLessThan(200)
  })

  test('an unreachable broker: said once in the status line, never as a toast', async ($, on) => {
    const w = world(on)
    w.readFails = true
    await start($)
    await w.clock.advance(5 * 60_000)
    expect(w.statuses.filter(s => s === 'bus: unreachable').length).toBe(1)
    expect(w.toasts).toEqual([])

    w.readFails = false
    w.waiting = [header(1)]
    await w.clock.advance(60_000)
    expect(w.statuses.at(-1)).toBe('bus: 1 unread')
  })
})

describe('timing', () => {
  test('poll mode reads once a minute by default', async ($, on) => {
    const w = world(on)
    await start($)
    expect(reads(w)).toBe(1)
    await w.clock.advance(59_000)
    expect(reads(w)).toBe(1)
    await w.clock.advance(1_000)
    expect(reads(w)).toBe(2)
    await w.clock.advance(3 * 60_000)
    expect(reads(w)).toBe(5)
  })

  test('an interval below 15 seconds is raised to 15', { options: { intervalSeconds: 1 } }, async ($, on) => {
    const w = world(on)
    await start($)
    await w.clock.advance(14_000)
    expect(reads(w)).toBe(1)
    await w.clock.advance(1_000)
    expect(reads(w)).toBe(2)
  })
})

describe('after the session runs the bus CLI', () => {
  test('an ack reads again at once, so the count clears without waiting a minute', async ($, on) => {
    const w = world(on)
    w.waiting = [header(1)]
    await start($)
    w.waiting = []
    await $.tool.call({ tool: 'Bash', command: 'agent-msg-bus ack machine-a/repo 20261014T12000001-000000001' })
    await w.clock.advance(0)
    expect(reads(w)).toBe(2)
    expect(w.statuses.at(-1)).toBe(undefined)
  })

  test('PowerShell and the .exe name count too', async ($, on) => {
    const w = world(on)
    await start($)
    await $.tool.call({ tool: 'PowerShell', command: '& "C:\\L\\agent-msg-bus\\agent-msg-bus.exe" ack machine-a/repo x' })
    await w.clock.advance(0)
    expect(reads(w)).toBe(2)
  })

  test('other commands read nothing', async ($, on) => {
    const w = world(on)
    await start($)
    await $.tool.call({ tool: 'Bash', command: 'git status' })
    await $.tool.call({ tool: 'Bash', command: 'agent-msg-bus --version' })
    await w.clock.advance(0)
    expect(reads(w)).toBe(1)
  })

  test('a pin asks for the address again before reading', async ($, on) => {
    const w = world(on)
    await start($)
    w.whoami = JSON.stringify({ ...ME, address: 'machine-a/repo.review', inbox: 'machine-a/repo.review', pinned: true, fallback: null })
    await $.tool.call({ tool: 'Bash', command: 'agent-msg-bus pin machine-a/repo.review' })
    await w.clock.advance(0)
    expect(w.runs.filter(r => r[1] === 'whoami').length).toBe(2)
    expect(w.runs.at(-1)?.[2]).toBe('machine-a/repo.review')
  })

  test('the tool result comes back unchanged', async ($, on) => {
    world(on)
    await start($)
    const result = await $.tool.call({ tool: 'Bash', command: 'agent-msg-bus ack machine-a/repo x' })
    expect(result).toEqual({ result: { stdout: '', stderr: '' } })
  })
})

describe('watch mode', () => {
  type Spawned = { argv: string[]; out: string[] }

  /** Each spawn of `watch` prints the next script's pieces, then exits. */
  function watching(on: On, scripts: string[][]): Spawned[] {
    const spawned: Spawned[] = []
    on('process.spawn', async function* ($, e) {
      const out = scripts[spawned.length] ?? []
      spawned.push({ argv: [...e.argv], out })
      for (const text of out) yield { stream: 'stdout' as const, text }
      return { value: { code: 0, signal: null } }
    })
    return spawned
  }

  const WATCH = { options: { mode: 'watch' } }
  const connected = (bound: string) =>
    JSON.stringify({ _watch: 'connected', at: '2026-10-14T12:00:00Z', detail: `subscribed as ${bound} via 127.0.0.1:9451` }) + '\n'
  const message = (n: number) =>
    JSON.stringify({ id: `20261014T1200000${n}-00000000${n}`, from: 'machine-a/peer', to: 'machine-a/repo', kind: 'fyi', subject: `subject ${n}` }) + '\n'

  test('runs watch on the address, with the fallback name', WATCH, async ($, on) => {
    const w = world(on)
    const spawned = watching(on, [[]])
    await start($)
    await w.clock.advance(0)
    expect(spawned[0]?.argv).toEqual(['C:\\L\\agent-msg-bus\\agent-msg-bus.exe', 'watch', 'machine-a/repo', '--fallback', 'machine-a/repo.0123abcd'])
  })

  test('a pinned address gets no fallback', WATCH, async ($, on) => {
    const w = world(on)
    w.whoami = JSON.stringify({ ...ME, pinned: true, fallback: null })
    const spawned = watching(on, [[]])
    await start($)
    await w.clock.advance(0)
    expect(spawned[0]?.argv).toEqual(['C:\\L\\agent-msg-bus\\agent-msg-bus.exe', 'watch', 'machine-a/repo'])
  })

  test('a message line reads the inbox again, even split across pieces', WATCH, async ($, on) => {
    const w = world(on)
    const line = message(1)
    watching(on, [[line.slice(0, 10), line.slice(10)]])
    await start($)
    w.waiting = [header(1)]
    await w.clock.advance(0)
    expect(reads(w)).toBe(2)
    expect(w.toasts.length).toBe(1)
  })

  test('connected asks for the address again, since it may have bound the fallback', WATCH, async ($, on) => {
    const w = world(on)
    watching(on, [[connected('machine-a/repo.0123abcd')]])
    await start($)
    w.whoami = JSON.stringify({ ...ME, bound: 'machine-a/repo.0123abcd', inbox: 'machine-a/repo.0123abcd' })
    await w.clock.advance(0)
    expect(w.runs.filter(r => r[1] === 'whoami').length).toBe(2)
    expect(w.runs.at(-1)?.[2]).toBe('machine-a/repo.0123abcd')
  })

  test('a watch that ends is said in the status line and started again after 30 s', WATCH, async ($, on) => {
    const w = world(on)
    const spawned = watching(on, [[], []])
    await start($)
    await w.clock.advance(0)
    expect(w.statuses.at(-1)).toBe('bus: watch stopped; restarting')
    expect(spawned.length).toBe(1)
    await w.clock.advance(30_000)
    expect(spawned.length).toBe(2)
  })
})
