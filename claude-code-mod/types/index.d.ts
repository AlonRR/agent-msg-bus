/** One unread message as the band lists it: the header, never the body. */
export type Header = { id: string; ts: string; from: string; kind: string; subject: string }

/**
 * What the last read found. `listed` is false when the count is known but the messages are not:
 * their bodies together were too large for one read, so the band gives the count alone.
 */
export type Listed = { addr: string; total: number; listed: boolean; messages: Header[] }

/** The message the person clicked open: its body as drawn, and how many characters were left off. */
export type Opened = { id: string; body: string; left: number }

declare module 'claude-code' {
  interface PluginState {
    'agent-msg-bus-inbox': {
      inbox: Shaped<Listed | null>
      isExpanded: Shaped<boolean>
      opened: Shaped<Opened | null>
    }
  }
}
