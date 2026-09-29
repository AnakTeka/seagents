# Shared scaffolding for tests that need a real desktop: a nested, isolated
# one, so nothing they open, focus or rearrange touches the session you are
# working in. Sourced (not run) by tools/test-restore.sh and
# tools/test-orchestration.sh.
#
# It is a headless weston providing a Wayland socket, a rootful Xwayland on :9
# as the X server, and its own i3 on its own IPC socket; HOME (and the runtime
# dir) point at a scratch directory, so the real cache, snapshot and runtime
# files are never touched.
#
#   . "$(dirname "$0")/lib-nested-desktop.sh"
#   nd_init <terminator|alacritty|alacritty-daemon>   # checks, scratch dirs, env
#   nd_start                                          # the desktop itself
#
# After nd_init: SEA (the release binary under test), ROOT (scratch, removed on
# exit), HOME, TERM_UNDER_TEST, ASOCK (the alacritty daemon's socket), and a
# `seagents` on PATH that is SEA. After nd_start: DISPLAY and I3SOCK point at
# the nested desktop. Cleanup runs on exit: every window in the nested i3 is
# killed, then the desktop; a test adds its own background pids to ND_PIDS.
#
# Requires: weston + Xwayland (both ship with a typical desktop install), i3,
# and the terminal under test.

ND_DISP=:9
ND_WL=wl-seagents-test
ND_SOCK=/tmp/sg-i3-test.sock        # short: UNIX socket paths cap at ~108 bytes
ASOCK=/tmp/sg-alacritty-test.sock
ND_PIDS=""
_ND_WESTON= _ND_XWAYLAND= _ND_I3= _ND_DAEMON=

nd_init() {
  SEA="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/seagents"
  [ -x "$SEA" ] || { echo "build first: cargo build --release"; exit 1; }
  TERM_UNDER_TEST=${1:-terminator}
  local bin=${TERM_UNDER_TEST%-daemon}
  command -v "$bin" >/dev/null || { echo "$bin not installed"; exit 1; }
  export SEAGENTS_TERMINAL=$TERM_UNDER_TEST

  ROOT=$(mktemp -d /tmp/seagents-nested-test.XXXXXX)
  trap nd_cleanup EXIT
  export HOME="$ROOT/home"
  # seagents derives its config and state dirs from these when set; they would
  # point the run (and its layout.json) back at the real ones.
  unset XDG_STATE_HOME XDG_CONFIG_HOME
  # Seed prompts (and weston's socket) live here; keep them out of the real one.
  export XDG_RUNTIME_DIR="$ROOT/run"
  export SEAGENTS_NO_NOTIFY=1
  mkdir -p "$HOME" "$ROOT/bin"
  chmod 700 "$ROOT"; mkdir -m 700 "$XDG_RUNTIME_DIR"
  # Launches a test makes by hand run a bare `seagents` (seagents' own
  # launcher uses its absolute path); make sure it finds the build under test.
  ln -sf "$SEA" "$ROOT/bin/seagents"
  export PATH="$ROOT/bin:$PATH"
}

nd_start() {
  echo "starting nested desktop on $ND_DISP (terminal: $TERM_UNDER_TEST)"
  weston --backend=headless --xwayland --socket=$ND_WL --width=1920 --height=1080 \
    > "$ROOT/weston.log" 2>&1 &
  _ND_WESTON=$!
  sleep 3
  # Rootful, unlike weston's own rootless xwayland module: a rootless X server
  # has no root window for i3 to manage, and weston already owns the WM slot.
  WAYLAND_DISPLAY=$ND_WL Xwayland $ND_DISP -geometry 1920x1080 > "$ROOT/xwayland.log" 2>&1 &
  _ND_XWAYLAND=$!
  sleep 3
  export DISPLAY=$ND_DISP I3SOCK=$ND_SOCK
  printf 'font pango:monospace 8\nnew_window pixel 1\nipc-socket %s\n' "$ND_SOCK" > "$ROOT/i3.conf"
  rm -f "$ND_SOCK"
  i3 -c "$ROOT/i3.conf" > "$ROOT/i3.log" 2>&1 &
  _ND_I3=$!
  sleep 3
  i3-msg -t get_workspaces >/dev/null 2>&1 || {
    echo "nested i3 did not start:"; cat "$ROOT/i3.log"; exit 1; }

  if [ "$TERM_UNDER_TEST" = alacritty-daemon ]; then
    rm -f "$ASOCK"
    alacritty --daemon --socket "$ASOCK" > "$ROOT/daemon.log" 2>&1 &
    _ND_DAEMON=$!
    sleep 3
    [ -S "$ASOCK" ] || { echo "alacritty daemon did not start"; cat "$ROOT/daemon.log"; exit 1; }
    # seagents spawns a bare `alacritty msg`, which finds the daemon through
    # this variable.
    export ALACRITTY_SOCKET="$ASOCK"
  fi
}

# The nested i3's tree, as JSON on stdout.
nd_tree() { i3-msg -t get_tree; }

# Kill every window in the nested desktop — everything in it is the test's.
nd_kill_windows() {
  i3-msg '[class=".*"] kill' >/dev/null 2>&1
  # Deliberately no `pkill -f`: its pattern matches the test script's own
  # command line too, and it will happily kill the shell running the test.
  for p in $ND_PIDS; do kill "$p" 2>/dev/null; done
  ND_PIDS=""
}

# Stop whatever still runs with the scratch HOME — an agent that outlived its
# window, or is still writing its caches on the way out — so nothing recreates
# files under ROOT after it is removed. By environment, not by name: the
# processes are seagents wrappers, agents and whatever those run.
_nd_reap_stragglers() {
  python3 - "$HOME" <<'PY'
import os, signal, sys, time
want = b"HOME=" + sys.argv[1].encode()
def stragglers():
    out = []
    for p in os.listdir("/proc"):
        if not p.isdigit() or int(p) == os.getpid():
            continue
        try:
            env = open(f"/proc/{p}/environ", "rb").read().split(b"\0")
        except OSError:
            continue
        if want in env:
            out.append(int(p))
    return out
for sig, grace in ((signal.SIGTERM, 5), (signal.SIGKILL, 1)):
    for p in stragglers():
        try: os.kill(p, sig)
        except OSError: pass
    deadline = time.time() + grace
    while stragglers() and time.time() < deadline:
        time.sleep(0.2)
PY
}

nd_cleanup() {
  [ -n "$_ND_I3" ] && DISPLAY=$ND_DISP I3SOCK=$ND_SOCK nd_kill_windows
  for pid in $_ND_DAEMON $_ND_I3 $_ND_XWAYLAND $_ND_WESTON; do kill "$pid" 2>/dev/null; done
  sleep 1
  for pid in $_ND_DAEMON $_ND_I3 $_ND_XWAYLAND $_ND_WESTON; do kill -9 "$pid" 2>/dev/null; done
  _nd_reap_stragglers
  rm -rf "$ROOT" "$ND_SOCK" "$ASOCK"
}
