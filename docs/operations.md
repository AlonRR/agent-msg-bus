# Running a broker

Everything needed to deploy and operate `agent-msg-bus`. Like [`usage.md`](usage.md), this page is
**deliberately free of any particular deployment's addresses, hostnames or machine names** — it
describes how to run the software, not where one instance of it happens to live. Substitute your own
values for `<BROKER_HOST>`, `<LAN_CIDR>` and the like.

**Architecture in one line:** one **broker** (HTTP + WebSocket, SQLite) that every machine reaches
over the LAN, and one **relay** per client machine that re-serves the broker on loopback, because
`Monitor` will not open a WebSocket to a private IP. See
[usage.md → The one thing to understand first](usage.md).

---

## 1. Build

```bash
cargo build --release      # target/release/agent-msg-bus
cargo test                 # run before shipping
```

One static-ish binary; no runtime dependencies beyond glibc.

> ⚠️ **Check glibc before you ship the binary, not after.** It is dynamically linked, and a build
> host with a newer glibc than the target produces a binary the target cannot load. Verify:
>
> ```bash
> objdump -T agent-msg-bus | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1   # required
> ldd --version                                                             # available on target
> ```
>
> The required version must be **≤** the target's. Build on the older platform, or statically link.

**There is no upstream release to download** — first-party code means integrity is a checksum you
carry through every hop rather than a published `checksums.txt`. `sha256sum` at build, after copy,
and again in its final location; the last hop is the one that matters.

---

## 2. Broker

Run it as a **dedicated unprivileged user**, never root.

| | |
|---|---|
| Binary | `/usr/local/bin/agent-msg-bus`, mode `755` |
| Tokens | `/etc/agent-msg-bus/tokens.json`, owned by the service user, mode **`600`** |
| Data | `/var/lib/agent-msg-bus/bus.db` (SQLite, WAL — also `-wal` and `-shm`) |
| Listen | `:9450` |

### Tokens

**Generate on-box and never echo them.** One token per client machine:

```bash
umask 077
openssl rand -hex 24        # per machine, written straight into tokens.json
chown <svcuser>:<svcuser> /etc/agent-msg-bus/tokens.json
chmod 600 /etc/agent-msg-bus/tokens.json
```

> 🔴 **`tokens.json` is read once, at startup.** Adding *or revoking* a token requires
> `systemctl restart agent-msg-bus`. **Revocation is not instant otherwise** — this is the one to
> remember, because a revoked-but-still-working token is a silent failure of exactly the control you
> were trying to exercise.

### systemd, and the hardening trap

Standard hardening is correct here, with one caveat that will cost you an afternoon:

> ⚠️ **`ProtectSystem=strict` makes the entire filesystem read-only, including your data directory.**
> The service starts *successfully*, answers `/health`, and then fails the moment it tries to write
> to SQLite. You must grant the data path back explicitly:
>
> ```ini
> ProtectSystem=strict
> ReadWritePaths=/var/lib/agent-msg-bus
> ```
>
> "Healthy but cannot write" is the shape of this bug. The check that catches it is listing the data
> directory and confirming `bus.db` plus its `-wal` and `-shm` siblings actually exist — not a
> `systemctl is-active`.

---

## 3. Network

Allow **`9450/tcp` from your client subnet** (`<LAN_CIDR>`).

This can reasonably be **wider than a service whose only access control is network position.** Every
request here carries a per-machine bearer token and a bad one gets a visible `401`. Pinning to
single source IPs is also actively awkward when clients are DHCP workstations — the rule breaks on
lease change. Decide deliberately; do not copy a single-source-IP rule from a service that has no
auth of its own.

### Reverse proxy — optional, and humans only

A vhost in front of the broker is fine for opening `/health` or `/peers` in a browser. **Sessions
cannot subscribe through it**: the proxy resolves to a private address too, so `Monitor` rejects it
exactly as it rejects the broker. Do not present a vhost as the client endpoint for a *session*.
A machine's *relay* can use one, which is what the next section is for.

### A TLS front, for machines away from the broker's network

The broker speaks plain HTTP and never TLS. A machine that has to reach it from outside its network
(a laptop on a VPN or an overlay network, say) goes through a reverse proxy that terminates TLS and
forwards to `9450`. From 0.4.26 both client legs use it: the CLI and hook over `https://`, and the
relay over `wss://`. Sessions are unaffected: they still subscribe to their own machine's relay on
loopback.

On the client machine:

```json
{ "url": "https://<FRONT_HOST>:<PORT>", "machine": "<machine-name>", "token": "<this machine's token>",
  "ca_file": "<path to the private root's PEM>" }
```

- **`ca_file`** is needed only when the front's certificate comes from a private root. It names one
  PEM file, which is trusted *alongside* the public web roots. **The operating system's certificate
  store is not consulted**, so installing the root into the Windows store does not make this tool
  trust it. `--ca-file` and `AMB_CA_FILE` override the config, as `--url` and `AMB_URL` do.
  - **Check the root before naming it.** A root downloaded over plain HTTP can be swapped by anyone
    on the path, and whoever holds the swapped one can then read the token. Compare its SHA-256
    fingerprint (`openssl x509 -noout -fingerprint -sha256 -in <file>`) with one read on the host
    that holds the root, obtained over a channel you already trust.
  - A `ca_file` that is unreadable or holds no certificate is an error at once, naming the file. The
    relay refuses to start; a CLI command exits 1; the hook says so in the session's banner.
  - An untrusted certificate is reported with the setting that fixes it, rather than as a bare
    `UnknownIssuer`.
- **The proxy must pass WebSocket upgrades through** on `/sub`. Most do by default.
- **The token never appears in a URL.** A front usually writes every request URL to its access log.
  The CLI has always sent the token as `Authorization: Bearer`. From 0.4.26 the relay does too, and
  for an `https://` broker it sends it *only* there. The log still shows addresses (`/sub?addr=…`),
  which are not secret.

> 🔴 **Upgrade the broker first.** Brokers before 0.4.26 read the token on `/sub` only from the
> query string. They refuse a relay that sends it only in the header, which is exactly what a relay
> pointed at an `https://` URL does. A relay on plain `http://` still sends the query token as well,
> so upgrading a relay ahead of its broker breaks nothing until its `url` changes to `https://`.

> ⚠️ **The SessionStart hook gives each broker call 3 s.** TLS plus every proxy hop has to fit in
> that. If it does not, the session still starts, with a banner saying registration is UNCONFIRMED.
> Time a `/health` through the front before relying on it.

---

## 4. Client machines

Each machine that hosts sessions needs **its own token, its own config, and a relay running as a
service.**

```json
{ "url": "http://<BROKER_HOST>:9450", "machine": "<machine-name>", "token": "<this machine's token>" }
```

at `~/.agent-msg-bus/config.json`, **mode 600 / owner-only ACL**. A machine that reaches the broker
through a TLS front uses an `https://` URL and may need `ca_file` (see
[A TLS front](#a-tls-front-for-machines-away-from-the-brokers-network)).

Then the relay — `agent-msg-bus relay --listen 127.0.0.1:9451` — as a **service**:

- **Windows:** a Scheduled Task at logon. Use a `wscript` shim so no console window appears.
- **Linux:** a systemd unit.

> 🔴 **On systemd, set `Environment=HOME=/home/<user>` explicitly.** systemd does **not** derive
> `HOME` from `User=`, and the binary looks for `$HOME/.agent-msg-bus/config.json`. Without it the
> relay starts happily with **no config, therefore no token and no broker** — running, healthy-looking
> and completely disconnected. Same family as the `ReadWritePaths` trap: everything looks fine until
> the one path that matters is exercised.

> ⚠️ **Never let a session start the relay.** A session-started relay dies with that session and is
> invisible when it fails.

### SessionStart hook

```json
{ "type": "command", "timeout": 15,
  "command": "\"$LOCALAPPDATA/agent-msg-bus/agent-msg-bus.exe\" session-start" }
```

> ⚠️ **Use the POSIX `$VAR` form with forward slashes — not `%VAR%`, even on Windows.** Hook commands
> run through a POSIX shell where the cmd-style form does not expand, and **the hook then fails
> silently**, which looks exactly like a bus with no traffic rather than like a broken hook.

---

## 5. Verify

```bash
# WHICH BUILD IS THIS? Ask before reading anything else — every check below can pass on a binary
# that predates the fix you came here to confirm, and nothing on the wire reports a version.
agent-msg-bus --version

systemctl is-active agent-msg-bus
curl -fsS http://<BROKER_HOST>:9450/health

# it can actually WRITE — this is the check that catches ReadWritePaths
ls -la /var/lib/agent-msg-bus/            # expect bus.db, bus.db-wal, bus.db-shm

# auth is real
curl -s -o /dev/null -w '%{http_code}\n' -H 'Authorization: Bearer wrong' \
  http://<BROKER_HOST>:9450/peers          # expect 401

# on a client machine
agent-msg-bus --version                    # clients and broker are updated separately
curl -fsS http://127.0.0.1:9451/health     # shows which addresses it is relaying
agent-msg-bus peers
```

> The clients on a bus do not have to match the broker's version — the wire contract is frozen for
> exactly that reason — but `peers` will not tell you what anyone is running, so a version question
> has to be asked of each machine's own binary.

---

## 6. Backup and restore

The **only** state is `bus.db` and `tokens.json`. Code is in git.

Losing the host loses **message history and the tokens, not the system**: rebuild from this page,
regenerate tokens, redistribute them.

Message history is deliberately not precious — the bus carries handoffs, and anything that matters
is supposed to point at a committed artefact rather than live in a message body.

> ⚠️ If you do back up `bus.db`, it is **SQLite in WAL mode**. A plain file copy captures the main
> database without recent commits and **does not look corrupt — it looks like an older, plausible
> history.** Use the online `.backup()` API, or checkpoint first.
