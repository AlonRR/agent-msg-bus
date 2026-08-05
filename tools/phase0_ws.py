"""Phase 0 probe — the experiment this whole project rests on.

Question: does a Monitor `ws:` frame wake a GENUINELY IDLE Claude Code session (one sitting at the
prompt with no turn running), or does it only land mid-turn?

That mattered because **no hook can start a turn** — `additionalContext` is passive, so the previous
file-watch design could only ever pre-load a message, never act on one. If the answer here had been
"no", the WebSocket approach would have been dead and Channels would have been mandatory.

Result (5 Aug 2026): frames at +60 s and +180 s each woke an idle session, unprompted, on one
long-lived socket; the connection stayed up 65 min 30 s including 60 minutes of total silence. Full
write-up in `docs/plan.md`.

Kept in the repo so the claim is reproducible rather than remembered. stdlib only, deliberately: an
install step is a second thing that can fail and would muddy a negative result.

    python tools/phase0_ws.py            # listens on 127.0.0.1:9444, logs beside itself

Then, from a Claude Code session:

    Monitor({ws: {url: "ws://127.0.0.1:9444/sub?addr=test/probe&token=x"},
             persistent: true, description: "phase0 probe"})

...then end the turn and leave the session alone.
"""
import base64
import datetime
import hashlib
import json
import os
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 9444
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
LOG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "phase0_log.txt")

# Offsets are measured from CONNECT, not from process start: the clock that matters is how long the
# session has been sitting idle holding the socket.
SCHEDULE = [60, 180, 330]
HOLD_SECONDS = 3600  # then exit, which is what ends the subscription


def log(msg):
    line = f"{datetime.datetime.now().isoformat(timespec='seconds')}  {msg}"
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(line + "\n")
    print(line, flush=True)


def encode_text(payload: str) -> bytes:
    """Server->client text frame, unmasked (RFC 6455: servers must not mask)."""
    data = payload.encode("utf-8")
    header = bytearray([0x81])  # FIN + opcode 0x1 (text)
    n = len(data)
    if n < 126:
        header.append(n)
    elif n < (1 << 16):
        header.append(126)
        header += struct.pack(">H", n)
    else:
        header.append(127)
        header += struct.pack(">Q", n)
    return bytes(header) + data


def handshake(conn) -> bool:
    req = b""
    conn.settimeout(10)
    while b"\r\n\r\n" not in req:
        chunk = conn.recv(4096)
        if not chunk:
            return False
        req += chunk
    key = None
    for line in req.decode("latin-1").split("\r\n"):
        if line.lower().startswith("sec-websocket-key:"):
            key = line.split(":", 1)[1].strip()
    if not key:
        log("HANDSHAKE FAILED - no Sec-WebSocket-Key")
        log("  raw: " + req.decode("latin-1")[:400].replace("\r\n", " | "))
        return False
    accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
    conn.send(
        ("HTTP/1.1 101 Switching Protocols\r\n"
         "Upgrade: websocket\r\n"
         "Connection: Upgrade\r\n"
         f"Sec-WebSocket-Accept: {accept}\r\n\r\n").encode()
    )
    # Logging the request line is not incidental: it is how we confirmed the query string survives
    # intact, which is the entire basis for putting the auth token there. Monitor's ws schema is
    # {url, protocols} with no headers field, so there is nowhere else to put it.
    log("HANDSHAKE OK - request line: " + req.decode("latin-1").split("\r\n")[0])
    return True


def serve():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((HOST, PORT))
    srv.listen(1)
    log(f"=== phase0 probe listening on ws://{HOST}:{PORT} ===")

    conn, addr = srv.accept()
    log(f"CONNECT from {addr}")
    if not handshake(conn):
        conn.close()
        return

    conn.settimeout(None)
    t0 = time.time()
    for i, offset in enumerate(SCHEDULE, start=1):
        delay = offset - (time.time() - t0)
        if delay > 0:
            time.sleep(delay)
        frame = json.dumps({
            "probe": "agent-msg-bus-phase0",
            "seq": i,
            "sent_at": datetime.datetime.now().isoformat(timespec="seconds"),
            "idle_seconds_at_send": offset,
            "note": f"Frame {i} of {len(SCHEDULE)}. If this reached you without you doing "
                    f"anything, Monitor woke an idle session and Phase 0 passes.",
        })
        try:
            conn.send(encode_text(frame))
            log(f"SENT frame {i} at +{offset}s")
        except Exception as e:  # a dead socket is itself a result worth recording
            log(f"SEND FAILED on frame {i}: {e!r}")
            return

    log(f"all frames sent; holding socket open {HOLD_SECONDS}s to test persistence")
    # NOTE: exiting here drops the TCP connection with no close handshake, which the client sees as
    # 1006 (abnormal). That is a limitation of the probe, not of the mechanism - and it is why the
    # real broker must send 1001 "going away" on shutdown, so a client can tell an orderly restart
    # from a network fault. See docs/plan.md, "Three requirements Phase 0 handed to the design".
    time.sleep(HOLD_SECONDS)
    log("hold elapsed; exiting (client will observe a 1006 close)")


if __name__ == "__main__":
    try:
        serve()
    except KeyboardInterrupt:
        log("interrupted")
    except Exception as e:
        log(f"FATAL: {e!r}")
        sys.exit(1)
