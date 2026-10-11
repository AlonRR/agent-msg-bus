#!/usr/bin/env bash
# Tests for swap-broker.sh. `cargo test` runs this on Linux (tests/swap_broker.rs); it also runs on
# its own: `bash scripts/test-swap-broker.sh`.
#
# Each case builds a throwaway broker host in a temp directory:
#   - fake broker builds: a script that answers `--version`, and on `serve` runs a small HTTP server
#     whose /health reports a version, so the test can tell which build is actually serving;
#   - a fake systemctl that runs whatever binary is installed at AMB_BIN, as systemd would;
#   - a data directory holding all three database files, each with distinct content.
# Then it runs the real script and checks what is on disk, what the service manager was asked to do,
# and what /health says, read independently of the script's own health check.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SWAP="$HERE/swap-broker.sh"

PY=""
for c in python3 python; do
  if command -v "$c" > /dev/null && "$c" -c 'import http.server' 2> /dev/null; then PY=$(command -v "$c"); break; fi
done
[ -n "$PY" ] || { echo "test-swap-broker: needs python3 for the fake broker"; exit 1; }

PASS=0
FAIL=0
check() { # check <what is being claimed> <command...>
  if "${@:2}"; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "  FAIL: $1"; fi
}

# --- the fake host ---------------------------------------------------------------------------------

new_host() {
  T=$(mktemp -d)
  PORT=$("$PY" -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  mkdir -p "$T/bin" "$T/data" "$T/incoming"
  cat > "$T/fake_broker.py" << 'PYEOF'
import http.server, json, sys
version, port = sys.argv[1], int(sys.argv[2])
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/health":
            self.send_error(404)
            return
        body = json.dumps({"ok": True, "role": "broker", "version": version}, separators=(",", ":")).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PYEOF
  cat > "$T/systemctl" << EOF
#!/usr/bin/env bash
# Fake systemctl: the unit is "\$AMB_BIN serve", tracked by a pid file.
echo "\$*" >> "$T/systemctl.log"
pidf="$T/service.pid"
running() { [ -f "\$pidf" ] && kill -0 "\$(cat "\$pidf")" 2> /dev/null; }
case "\$1" in
  start) running && exit 0
         "\$AMB_BIN" serve < /dev/null >> "$T/service.log" 2>&1 &
         echo \$! > "\$pidf"
         exit 0 ;;
  stop) if running; then
          kill "\$(cat "\$pidf")"
          for _ in \$(seq 1 50); do running || break; sleep 0.1; done
        fi
        rm -f "\$pidf"
        exit 0 ;;
  is-active) running ;;
  *) exit 1 ;;
esac
EOF
  chmod +x "$T/systemctl"
  printf 'main database\n' > "$T/data/bus.db"
  printf 'recent commits not yet checkpointed\n' > "$T/data/bus.db-wal"
  printf 'shared memory index\n' > "$T/data/bus.db-shm"
  export AMB_BIN="$T/bin/agent-msg-bus" AMB_DATA="$T/data" AMB_UNIT=fake.service
  export AMB_HEALTH="127.0.0.1:$PORT" AMB_WAIT=4 SYSTEMCTL="$T/systemctl"
}

# make_build <path> <version it reports> [cannot-run | serves:<version>]
# `touch $T/broken-<version>` makes that build's `serve` exit at once, from then on.
make_build() {
  local path=$1 v=$2 how=${3:-ok} served=$2
  case $how in serves:*) served=${how#serves:} ;; esac
  cat > "$path" << EOF
#!/usr/bin/env bash
case "\$1" in
  --version)
    if [ "$how" = cannot-run ]; then
      echo "agent-msg-bus: /lib/x86_64-linux-gnu/libc.so.6: version 'GLIBC_2.99' not found" >&2
      exit 1
    fi
    echo "agent-msg-bus $v" ;;
  serve)
    [ -e "$T/broken-$v" ] && exit 1
    exec "$PY" "$T/fake_broker.py" "$served" "$PORT" ;;
esac
EOF
  chmod +x "$path"
}

# The host before a swap: 0.0.1 installed and serving.
host_on_001() {
  new_host
  make_build "$AMB_BIN" 0.0.1
  "$SYSTEMCTL" start "$AMB_UNIT"
  wait_for_version 0.0.1 || echo "  SETUP: the old build never came up"
  : > "$T/systemctl.log"
}

served_version() {
  "$PY" -c 'import json, sys, urllib.request
print(json.load(urllib.request.urlopen("http://127.0.0.1:%s/health" % sys.argv[1], timeout=2))["version"])' "$PORT" 2> /dev/null
}
wait_for_version() {
  for _ in $(seq 1 40); do [ "$(served_version)" = "$1" ] && return 0; sleep 0.1; done
  return 1
}
version_of() { "$1" --version 2> /dev/null | awk '{ print $2 }'; }
sha_of() { sha256sum "$1" | cut -c1-64; }
swap() { # swap <new build> [sha]; sets OUT and RC
  OUT=$("$SWAP" "$1" "${2:-$(sha_of "$1")}" 2>&1)
  RC=$?
}
done_host() {
  "$SYSTEMCTL" stop "$AMB_UNIT" > /dev/null 2>&1
  rm -rf "$T"
}
is() { [ "$1" = "$2" ]; }
has() { case $1 in *"$2"*) return 0 ;; esac; return 1; }
lacks() { ! has "$@"; }
same() { cmp -s "$1" "$2"; }
nothing_was_touched() {
  check "exit 1" is "$RC" 1
  check "no stop was issued" lacks "$(cat "$T/systemctl.log")" stop
  check "0.0.1 is still installed" is "$(version_of "$AMB_BIN")" 0.0.1
  check "no staged copy left" test ! -e "$AMB_BIN.new"
  check "no backup made" test ! -e "$AMB_DATA/backup-pre-0.0.2"
  check "0.0.1 still serves" is "$(served_version)" 0.0.1
}

# --- cases -----------------------------------------------------------------------------------------

echo "a good build is swapped in, and /health reports it"
host_on_001
cp -a "$AMB_DATA" "$T/data-before"
make_build "$T/incoming/agent-msg-bus" 0.0.2
swap "$T/incoming/agent-msg-bus"
check "exit 0 ($OUT)" is "$RC" 0
check "0.0.2 is installed" is "$(version_of "$AMB_BIN")" 0.0.2
check "0.0.2 serves" is "$(served_version)" 0.0.2
check "0.0.1 kept as the rollback binary" is "$(version_of "$AMB_BIN.pre-0.0.2")" 0.0.1
for f in bus.db bus.db-wal bus.db-shm; do
  check "backup holds $f, unchanged" same "$T/data-before/$f" "$AMB_DATA/backup-pre-0.0.2/$f"
done
check "no staged copy left" test ! -e "$AMB_BIN.new"
check "stopped, then started" is "$(cut -d' ' -f1 "$T/systemctl.log" | grep -E '^(stop|start)$' | tr '\n' ' ')" "stop start "
done_host

echo "a wrong checksum changes nothing"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2
swap "$T/incoming/agent-msg-bus" 0000000000000000000000000000000000000000000000000000000000000000
nothing_was_touched
check "says the sha is wrong" has "$OUT" "sha256"
done_host

echo "a build this host cannot run changes nothing"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2 cannot-run
swap "$T/incoming/agent-msg-bus"
nothing_was_touched
check "points at glibc" has "$OUT" "glibc"
done_host

echo "a rollback name already taken changes nothing"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2
mkdir "$AMB_DATA/backup-pre-0.0.2"
swap "$T/incoming/agent-msg-bus"
check "exit 1" is "$RC" 1
check "no stop was issued" lacks "$(cat "$T/systemctl.log")" stop
check "0.0.1 is still installed" is "$(version_of "$AMB_BIN")" 0.0.1
check "names the taken path" has "$OUT" "backup-pre-0.0.2 already exists"
done_host

echo "no database where AMB_DATA points changes nothing"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2
rm "$AMB_DATA/bus.db"
swap "$T/incoming/agent-msg-bus"
check "exit 1" is "$RC" 1
check "no stop was issued" lacks "$(cat "$T/systemctl.log")" stop
check "says where it looked" has "$OUT" "no bus.db"
done_host

echo "a build that never comes up is rolled back"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2
touch "$T/broken-0.0.2"
swap "$T/incoming/agent-msg-bus"
check "exit 1 ($OUT)" is "$RC" 1
check "0.0.1 is installed again" is "$(version_of "$AMB_BIN")" 0.0.1
check "0.0.1 serves again" is "$(served_version)" 0.0.1
check "the failed build is kept" is "$(version_of "$AMB_BIN.failed-0.0.2")" 0.0.2
check "the rollback name is free again" test ! -e "$AMB_BIN.pre-0.0.2"
check "the database copy is kept" test -e "$AMB_DATA/backup-pre-0.0.2/bus.db"
check "says ROLLED BACK" has "$OUT" "ROLLED BACK"
done_host

echo "a build that answers with another version is rolled back"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2 serves:0.0.9
swap "$T/incoming/agent-msg-bus"
check "exit 1 ($OUT)" is "$RC" 1
check "0.0.1 is installed again" is "$(version_of "$AMB_BIN")" 0.0.1
check "0.0.1 serves again" is "$(served_version)" 0.0.1
check "names what it saw" has "$OUT" "version 0.0.9"
done_host

echo "when neither build comes up, it asks for a person and says how to restore the database"
host_on_001
make_build "$T/incoming/agent-msg-bus" 0.0.2
touch "$T/broken-0.0.2" "$T/broken-0.0.1"
swap "$T/incoming/agent-msg-bus"
check "exit 2 ($OUT)" is "$RC" 2
check "0.0.1 is back in place" is "$(version_of "$AMB_BIN")" 0.0.1
check "says NEEDS A PERSON" has "$OUT" "NEEDS A PERSON"
check "the restore removes the WAL first" has "$OUT" "rm -f $AMB_DATA/bus.db-wal $AMB_DATA/bus.db-shm && cp -a $AMB_DATA/backup-pre-0.0.2/. $AMB_DATA/"
done_host

echo
echo "$((PASS + FAIL)) checks, $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] && [ "$PASS" -gt 0 ]
