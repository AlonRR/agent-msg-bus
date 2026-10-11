#!/usr/bin/env bash
# Swap a new agent-msg-bus build into a running broker, on the broker's own host, keeping a way back.
#
#   sudo scripts/swap-broker.sh <new binary> <its sha256>
#
# Each step stops the swap if it fails:
#   1. The new binary's sha256 matches, in its final directory, and it runs. `--version` is also the
#      glibc check: a build linked against a newer glibc than this host's fails here, while the old
#      broker is still serving.
#   2. No rollback name for this version is taken yet: <bin>.pre-<v>, <bin>.failed-<v> and
#      <data>/backup-pre-<v>. A previous attempt left them, and a person should look first.
#   3. The service stops. All three database files (bus.db, bus.db-wal, bus.db-shm) are copied while
#      it is stopped, and each copy's hash is checked. The WAL is not always checkpointed at shutdown,
#      so bus.db alone can be an older but plausible-looking history.
#   4. The installed binary is renamed to <bin>.pre-<v>, the new one takes its place, and the service
#      starts.
#   5. /health must report the new version within AMB_WAIT seconds. "Active" is not enough: a unit
#      can be active while an old process still holds the port, or while the new one has not yet
#      bound it. If the check fails, the old binary goes back and is started again.
#
# The database is NOT restored automatically, because the new build may already have written to it.
# If the old build does not come back either, the script prints the command that restores the copy.
#
# Exit status: 0 swapped and healthy; 1 refused, or rolled back with the old build healthy again;
# 2 rolled back but /health does not answer, so a person has to look.
#
# Settings, from the environment:
#   AMB_BIN     installed binary              default /usr/local/bin/agent-msg-bus
#   AMB_DATA    the broker's data directory   default /var/lib/agent-msg-bus
#   AMB_UNIT    its systemd unit              default agent-msg-bus.service
#   AMB_HEALTH  host:port to ask for /health  default 127.0.0.1:9450
#   AMB_WAIT    seconds to wait for /health   default 15
#   SYSTEMCTL   the service manager           default systemctl (the tests put a fake here)
#
# Needs bash (for /dev/tcp), sha256sum and timeout. No curl or sqlite3, which a minimal broker host
# often lacks. Tests: scripts/test-swap-broker.sh, run by `cargo test` on Linux.
set -uo pipefail

BIN=${AMB_BIN:-/usr/local/bin/agent-msg-bus}
DATA=${AMB_DATA:-/var/lib/agent-msg-bus}
UNIT=${AMB_UNIT:-agent-msg-bus.service}
HEALTH=${AMB_HEALTH:-127.0.0.1:9450}
WAIT=${AMB_WAIT:-15}
SYSTEMCTL=${SYSTEMCTL:-systemctl}
DBFILES="bus.db bus.db-wal bus.db-shm"

say() { printf '%s\n' "$@"; }
sha() { sha256sum "$1" | cut -c1-64; }

# Before the service is touched: undo the staged copy and leave everything else as it was.
refuse() {
  rm -f "$BIN.new"
  say "REFUSED: $1" "Nothing was changed. The running broker was not touched."
  exit 1
}

# After the stop but before the swap: the old binary is still in place, so start it again.
abort_and_restart() {
  rm -f "$BIN.new"
  say "ABORTED: $1" "Starting the old build again; the binary was not swapped."
  "$SYSTEMCTL" start "$UNIT"
  exit 1
}

# The version /health reports, printed; fails if nothing healthy answers. HTTP/1.0, so the server
# closes the connection after one reply and `cat` ends.
health_version() {
  local host=${HEALTH%:*} port=${HEALTH##*:} reply
  reply=$(timeout 3 bash -c '
    exec 3<>"/dev/tcp/$1/$2" || exit 1
    printf "GET /health HTTP/1.0\r\nHost: %s\r\n\r\n" "$1" >&3
    cat <&3' _ "$host" "$port" 2>/dev/null) || return 1
  case $reply in *'"ok":true'*) ;; *) return 1 ;; esac
  printf '%s\n' "$reply" | grep -o '"version":"[^"]*"' | head -n 1 | cut -d'"' -f4
}

# Waits for /health to report version $1. An empty $1 accepts any healthy answer, for a build too
# old to report its version.
LAST_SEEN="no answer"
wait_health() {
  local want=$1 deadline=$((SECONDS + WAIT)) got
  while [ "$SECONDS" -lt "$deadline" ]; do
    if got=$(health_version); then
      if [ -z "$want" ] || [ "$got" = "$want" ]; then return 0; fi
      LAST_SEEN="an answer from version ${got:-unknown}"
    fi
    sleep 0.5
  done
  return 1
}

[ $# -eq 2 ] || { say "usage: $0 <new binary> <its sha256>" "Settings and steps: the comment at the top of this file."; exit 1; }
NEW=$1
EXPECT=$2

[ -f "$NEW" ] || refuse "no file at $NEW"
[ -x "$BIN" ] || refuse "no installed binary at $BIN. This script replaces one; it does not install the first. Set AMB_BIN if it lives elsewhere."
[ "$(readlink -f "$NEW")" != "$(readlink -f "$BIN")" ] || refuse "$NEW is the installed binary itself"
[ -e "$DATA/bus.db" ] || refuse "no bus.db in $DATA. Set AMB_DATA to the broker's data directory."

# 1. Integrity and "does it run here", checked in the directory it will run from.
got=$(sha "$NEW")
[ "$got" = "$EXPECT" ] || refuse "sha256 of $NEW is $got, expected $EXPECT"
cp "$NEW" "$BIN.new" && chmod 0755 "$BIN.new" || refuse "cannot stage the new binary at $BIN.new"
got=$(sha "$BIN.new")
[ "$got" = "$EXPECT" ] || refuse "the staged copy $BIN.new has sha256 $got, expected $EXPECT"
out=$("$BIN.new" --version 2>&1) || refuse "the new binary does not run on this host: $out
A 'GLIBC_x.y not found' here means it was built against a newer glibc than this host has (docs/operations.md, section 1)."
V=$(printf '%s\n' "$out" | awk 'NR == 1 { print $2 }')
[[ $V =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]] || refuse "cannot read a version from: $out"
OLDV=$("$BIN" --version 2>/dev/null | awk 'NR == 1 { print $2 }')

# 2. Rollback names.
for p in "$BIN.pre-$V" "$BIN.failed-$V" "$DATA/backup-pre-$V"; do
  [ -e "$p" ] && refuse "$p already exists. An earlier swap to $V left it. Check it, then move it aside."
done
say "installed: ${OLDV:-unknown}   new: $V   sha256 ok, runs here"

# 3. Stop, then a cold copy of all three database files.
"$SYSTEMCTL" stop "$UNIT"
if "$SYSTEMCTL" is-active --quiet "$UNIT"; then
  rm -f "$BIN.new"
  say "ABORTED: $UNIT is still active after stop. Nothing was swapped."
  exit 1
fi
say "stopped $UNIT"
mkdir "$DATA/backup-pre-$V" || abort_and_restart "cannot create $DATA/backup-pre-$V"
for f in $DBFILES; do
  [ -e "$DATA/$f" ] || continue
  cp -a "$DATA/$f" "$DATA/backup-pre-$V/$f" || abort_and_restart "cannot copy $f"
  [ "$(sha "$DATA/$f")" = "$(sha "$DATA/backup-pre-$V/$f")" ] || abort_and_restart "the copy of $f does not match the original"
done
say "database copied to $DATA/backup-pre-$V: $(cd "$DATA/backup-pre-$V" && echo *)"

# 4. Swap and start.
mv "$BIN" "$BIN.pre-$V" || abort_and_restart "cannot rename $BIN"
if ! mv "$BIN.new" "$BIN"; then
  mv "$BIN.pre-$V" "$BIN"
  abort_and_restart "cannot move the new binary into place"
fi
"$SYSTEMCTL" start "$UNIT"

# 5. The new build has to say so itself.
if wait_health "$V"; then
  say "SWAPPED: /health on $HEALTH reports $V." \
    "Rollback binary: $BIN.pre-$V   database copy: $DATA/backup-pre-$V"
  exit 0
fi

say "The new build did not report $V on /health within ${WAIT}s (last seen: $LAST_SEEN)." \
  "Rolling back to ${OLDV:-the previous build}."
"$SYSTEMCTL" stop "$UNIT"
mv "$BIN" "$BIN.failed-$V" && mv "$BIN.pre-$V" "$BIN"
"$SYSTEMCTL" start "$UNIT"
LAST_SEEN="no answer"
if wait_health "$OLDV"; then
  say "ROLLED BACK: /health on $HEALTH reports ${OLDV:-a healthy broker} again." \
    "The failed build is kept as $BIN.failed-$V. Its log: journalctl -u $UNIT -n 50"
  exit 1
fi
say "NEEDS A PERSON: the previous build is back in place, but /health does not answer (last seen: $LAST_SEEN)." \
  "The new build may have changed the database before it failed. To restore the copy taken before the swap:" \
  "  $SYSTEMCTL stop $UNIT && rm -f $DATA/bus.db-wal $DATA/bus.db-shm && cp -a $DATA/backup-pre-$V/. $DATA/ && $SYSTEMCTL start $UNIT" \
  "The rm matters: a WAL left beside the restored bus.db would be replayed into it."
exit 2
