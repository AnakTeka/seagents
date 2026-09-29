#!/bin/bash
# Regression test for `seagents snapshot` / `seagents restore`.
#
# Restore rearranges i3 workspaces and opens terminal windows, so it cannot be
# exercised against the session you are working in. This runs the whole cycle
# inside a **nested, isolated desktop**: a headless weston providing a Wayland
# socket, a rootful Xwayland on :9 as the X server, and its own i3 on its own
# IPC socket, with HOME pointed at a scratch directory so the real cache and
# snapshot are never touched.
#
#   tools/test-restore.sh [terminator|alacritty|alacritty-daemon]
#
# The terminal argument matters: the stored layout is terminal-agnostic and the
# swallow criterion is chosen at restore time (window_role for Terminator, a
# WM_CLASS instance for alacritty), so both paths need exercising.
#
# Requires: weston + Xwayland (both ship with a typical desktop install), i3,
# and the terminal under test. Agents are `sleep`, so nothing here launches a
# real agent.
#
# Exits non-zero and prints a diff if the restored tree differs from the
# captured one.
set -u

SEA="$(cd "$(dirname "$0")/.." && pwd)/target/release/seagents"
[ -x "$SEA" ] || { echo "build first: cargo build --release"; exit 1; }
TERM_UNDER_TEST=${1:-terminator}
TERM_BIN=${TERM_UNDER_TEST%-daemon}
command -v "$TERM_BIN" >/dev/null || { echo "$TERM_BIN not installed"; exit 1; }
ASOCK=/tmp/sg-alacritty-test.sock
export SEAGENTS_TERMINAL=$TERM_UNDER_TEST

DISP=:9
WL=wl-seagents-test
SOCK=/tmp/sg-i3-test.sock          # short: UNIX socket paths cap at ~108 bytes
ROOT=$(mktemp -d /tmp/seagents-restore-test.XXXXXX)
WESTON= XWAYLAND= I3= DAEMON=

# Terminals differ in which property carries the handle, so ask i3 on both.
# Deliberately no `pkill -f`: its pattern matches this script's own command
# line too, and it will happily kill the shell running the test.
AGENT_PIDS=""
kill_agents() {
  for crit in 'window_role="^seagents-t"' 'instance="^seagents-t"'; do
    DISPLAY=$DISP I3SOCK=$SOCK i3-msg "[$crit] kill" >/dev/null 2>&1
  done
  for p in $AGENT_PIDS; do kill "$p" 2>/dev/null; done
}

cleanup() {
  kill_agents
  for pid in $DAEMON $I3 $XWAYLAND $WESTON; do kill "$pid" 2>/dev/null; done
  sleep 1
  for pid in $DAEMON $I3 $XWAYLAND $WESTON; do kill -9 "$pid" 2>/dev/null; done
  rm -rf "$ROOT" "$SOCK" "$ASOCK"
}
trap cleanup EXIT

export HOME="$ROOT/home"
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export SEAGENTS_NO_NOTIFY=1
mkdir -p "$HOME" "$ROOT/bin"
# The launches below run a bare `seagents` (restore itself uses its own
# absolute path); make sure it finds the build under test.
ln -sf "$SEA" "$ROOT/bin/seagents"
export PATH="$ROOT/bin:$PATH"

echo "starting nested desktop on $DISP (terminal: $TERM_UNDER_TEST)"
weston --backend=headless --xwayland --socket=$WL --width=1920 --height=1080 \
  > "$ROOT/weston.log" 2>&1 &
WESTON=$!
sleep 3
# Rootful, unlike weston's own rootless xwayland module: a rootless X server
# has no root window for i3 to manage, and weston already owns the WM slot.
WAYLAND_DISPLAY=$WL Xwayland $DISP -geometry 1920x1080 > "$ROOT/xwayland.log" 2>&1 &
XWAYLAND=$!
sleep 3
export DISPLAY=$DISP I3SOCK=$SOCK
printf 'font pango:monospace 8\nnew_window pixel 1\nipc-socket %s\n' "$SOCK" > "$ROOT/i3.conf"
rm -f "$SOCK"
i3 -c "$ROOT/i3.conf" > "$ROOT/i3.log" 2>&1 &
I3=$!
sleep 3
i3-msg -t get_workspaces >/dev/null 2>&1 || {
  echo "nested i3 did not start:"; cat "$ROOT/i3.log"; exit 1; }

if [ "$TERM_UNDER_TEST" = alacritty-daemon ]; then
  rm -f "$ASOCK"
  alacritty --daemon --socket "$ASOCK" > "$ROOT/daemon.log" 2>&1 &
  DAEMON=$!
  sleep 3
  [ -S "$ASOCK" ] || { echo "alacritty daemon did not start"; cat "$ROOT/daemon.log"; exit 1; }
  # `restore` spawns a bare `alacritty msg`, which finds the daemon through
  # this variable.
  export ALACRITTY_SOCKET="$ASOCK"
fi

dump() {  # the shape of workspace 7: nesting, layouts, percentages
  i3-msg -t get_tree | python3 -c "
import json,sys
def walk(n,ws=None,d=0):
    if n.get('type')=='workspace': ws=n['name']
    if ws=='7' and n.get('type')!='workspace':
        print('  '*d + '%s pct=%.2f %s' % (n.get('layout') or 'leaf',
              n.get('percent') or 0, 'WINDOW' if n.get('window') else ''))
    for c in n.get('nodes',[])+n.get('floating_nodes',[]): walk(c,ws,d+1 if ws=='7' else d)
walk(json.load(sys.stdin))"
}

launch() {  # handle, cwd, tag [, parent] — the handle is what `restore` mints: seagents-<tag>
  local inner="seagents run --tag '$3' ${4:+--parent '$4'} -- sleep 3600"
  case $TERM_UNDER_TEST in
    terminator) terminator -u --role "$1" --working-directory="$2" \
                  -x /bin/sh -c "$inner" >/dev/null 2>&1 &
                AGENT_PIDS="$AGENT_PIDS $!" ;;
    alacritty)  alacritty --class "Alacritty,$1" --working-directory "$2" \
                  -e /bin/sh -c "$inner" >/dev/null 2>&1 &
                AGENT_PIDS="$AGENT_PIDS $!" ;;
    # A daemon client runs in the foreground and exits immediately; there is no
    # process of ours to record. Recording $! here would capture the last
    # background job — the daemon — and kill_agents would shoot it.
    alacritty-daemon)
                alacritty msg --socket "$ASOCK" create-window \
                  --class "Alacritty,$1" --working-directory "$2" \
                  -e /bin/sh -c "$inner" 2>/dev/null ;;
    *) echo "no launch recipe for $TERM_UNDER_TEST"; return 1 ;;
  esac
  for _ in $(seq 1 60); do
    i3-msg -t get_tree | grep -q "seagents-$3" && return 0
    sleep 0.25
  done
  echo "timed out waiting for window $1"; return 1
}

# Identity check: `run` must find its own window with no --tag handle and
# focus parked elsewhere — from ALACRITTY_WINDOW_ID, or failing that from /proc
# ancestry. Terminator supplies neither (no window-id variable, and one process
# behind every window), so it falls back to focus capture and this is skipped.
if [ "$TERM_UNDER_TEST" != terminator ]; then
  i3-msg 'workspace --no-auto-back-and-forth "8"' >/dev/null
  case $TERM_UNDER_TEST in
    alacritty) alacritty --class "Alacritty,seagents-t9" --working-directory /tmp \
                 -e /bin/sh -c "seagents run -- sleep 3600" >/dev/null 2>&1 &
               AGENT_PIDS="$AGENT_PIDS $!" ;;
    alacritty-daemon)
               alacritty msg --socket "$ASOCK" create-window \
                 --class "Alacritty,seagents-t9" --working-directory /tmp \
                 -e /bin/sh -c "seagents run -- sleep 3600" 2>/dev/null ;;
  esac
  for _ in $(seq 1 60); do
    i3-msg -t get_tree | grep -q "seagents-t9" && break
    sleep 0.25
  done
  sleep 3
  i3-msg 'workspace --no-auto-back-and-forth "7"' >/dev/null   # park focus elsewhere
  sleep 1
  want=$(i3-msg -t get_tree | python3 -c "
import json,sys
def walk(n):
    wp=n.get('window_properties') or {}
    if wp.get('instance')=='seagents-t9': print(n['id']); return
    for c in n.get('nodes',[])+n.get('floating_nodes',[]): walk(c)
walk(json.load(sys.stdin))")
  got=$(python3 -c "
import json,glob,os
for f in glob.glob(os.environ['HOME']+'/.cache/seagents/*.meta'):
    d=json.load(open(f))
    if d.get('cwd')=='/tmp': print(d.get('con_id'))" | head -1)
  if [ -n "$want" ] && [ "$want" = "$got" ]; then
    echo "PASS: run() identified its own window with no handle or focus (con_id $got)"
  else
    echo "FAIL: identity got '$got', window is '$want'"; exit 1
  fi
  kill_agents; sleep 2
  AGENT_PIDS=""
fi

i3-msg 'workspace --no-auto-back-and-forth "7"' >/dev/null
launch seagents-t1 /tmp t1
launch seagents-t2 "$HOME" t2
i3-msg 'split v' >/dev/null
# t3 is t1's child: restore must bring the lineage back too.
launch seagents-t3 /etc t3 t1
sleep 1

before=$(dump)
echo "=== captured layout ==="; echo "$before"
"$SEA" snapshot >/dev/null || { echo "snapshot failed"; exit 1; }

# Kill every agent: this is the post-reboot state.
kill_agents
sleep 3
# Guard: if anything survived, a "restore" that does nothing would compare
# equal and pass for the wrong reason.
alive=$(i3-msg -t get_tree | grep -o 'seagents-t[0-9]' | wc -l)
[ "$alive" -eq 0 ] || { echo "FAIL: $alive agent windows survived the kill"; exit 1; }

out=$("$SEA" restore --stagger=0) || { echo "restore failed"; exit 1; }
echo "$out"
echo "$out" | grep -qE 'restored 3 windows' || {
  echo "FAIL: restore did not launch 3 windows"; exit 1; }
sleep 4
after=$(dump)
echo "=== restored layout ==="; echo "$after"

if [ "$before" != "$after" ]; then
  echo "FAIL ($TERM_UNDER_TEST): restored tree differs"
  diff <(echo "$before") <(echo "$after")
  exit 1
fi
echo "PASS ($TERM_UNDER_TEST): restored tree matches the captured one"

# The restored agents are the captured ones by name, lineage included.
tags=$("$SEA" status --json | python3 -c "
import json,sys
print(' '.join(sorted('%s:%s:%s' % (s['tag'], s['parent'], s['depth']) for s in json.load(sys.stdin))))")
want="t1:None:0 t2:None:0 t3:t1:1"
[ "$tags" = "$want" ] || { echo "FAIL: restored tags '$tags', want '$want'"; exit 1; }
echo "PASS: restored agents carry their captured tags and lineage ($tags)"

# Idempotent by tag: everything captured is live, so a second restore is a no-op.
out=$("$SEA" restore --stagger=0) || { echo "second restore failed"; exit 1; }
echo "$out" | grep -qE 'restored 0 windows' && echo "$out" | grep -q '3 already running' || {
  echo "FAIL: second restore was not a no-op:"; echo "$out"; exit 1; }
echo "PASS: a second restore relaunches nothing"
exit 0
