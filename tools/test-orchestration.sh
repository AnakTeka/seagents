#!/bin/bash
# Integration test for `seagents spawn` / `wait` / `status` / `focus`, with a
# real Claude Code, inside a nested, isolated desktop
# (tools/lib-nested-desktop.sh: headless weston, rootful Xwayland on :9, its
# own i3, HOME at a scratch directory), so no window it opens reaches your
# session and the real cache is never touched.
#
#   tools/test-orchestration.sh [alacritty|terminator|alacritty-daemon]
#
# Claude runs as `--model haiku` on one-line prompts, so a run costs a few
# cents. It keeps the real CLAUDE_CONFIG_DIR (auth lives there) and works in
# target/sg-it/orchestration — a subdirectory of the repo, whose folder trust
# it inherits, so no trust dialog appears.
#
# Prints PASS/FAIL per step; exits non-zero on the first failure.
set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
# Claude's auth and config must survive the scratch HOME.
if [ -n "${CLAUDE_CONFIG_DIR:-}" ]; then
  export CLAUDE_CONFIG_DIR
else
  REAL_HOME=$HOME
fi
command -v claude >/dev/null || { echo "claude is not on PATH"; exit 1; }

. "$REPO/tools/lib-nested-desktop.sh"
nd_init "${1:-alacritty}"
if [ -n "${REAL_HOME:-}" ]; then
  ln -s "$REAL_HOME/.claude" "$HOME/.claude"
  ln -s "$REAL_HOME/.claude.json" "$HOME/.claude.json"
fi
nd_start

WORK="$REPO/target/sg-it/orchestration"
mkdir -p "$WORK"
CACHE="$HOME/.cache/seagents"
HAIKU=(--arg --model --arg haiku)

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; exit 1; }
# Fields of the JSON object on stdin, by python expression over `d`,
# space-separated.
jq_() { python3 -c "
import json,sys; d=json.load(sys.stdin); r=($1)
print(*r) if isinstance(r, tuple) else print(r)"; }
# The live `status --json` entry for tag $1, as JSON.
status_of() {
  "$SEA" status --json | python3 -c "
import json,sys
print(json.dumps(next((s for s in json.load(sys.stdin) if s['tag']=='$1'), None)))"
}
# The .meta of tag $1, as JSON.
meta_of() {
  python3 -c "
import json,glob
for f in glob.glob('$CACHE/*.meta'):
    d=json.load(open(f))
    if d.get('tag')=='$1': print(json.dumps(d)); break"
}
# The con_id of the window whose handle is seagents-$1, in any property.
window_of() {
  nd_tree | python3 -c "
import json,sys
def walk(n):
    wp=n.get('window_properties') or {}
    if 'seagents-$1' in (wp.get('instance'), wp.get('class'), wp.get('window_role')): print(n['id'])
    for c in n.get('nodes',[])+n.get('floating_nodes',[]): walk(c)
walk(json.load(sys.stdin))"
}

# 1. spawn: a handle back, a window in the nested i3, and the .meta naming it.
out=$(cd "$WORK" && "$SEA" spawn --tag it-a "${HAIKU[@]}" -- 'Reply with exactly: ALPHA') \
  || fail "spawn exited $?: $out"
[ "$(echo "$out" | jq_ "d['tag'], d['depth'], d['stops'], d['parent']")" = "it-a 0 0 None" ] \
  || fail "spawn handle: $out"
pass "spawn returned the handle ($out)"
want=""
for _ in $(seq 1 40); do want=$(window_of it-a); [ -n "$want" ] && break; sleep 0.25; done
got=$(meta_of it-a | jq_ "d.get('con_id')")
[ -n "$want" ] && [ "$want" = "$got" ] || fail "window: i3 has '$want', .meta has '$got'"
argv=$(meta_of it-a | jq_ "d['argv']")
case $argv in *ALPHA*|*--settings*) fail "recorded argv carries the seed or our settings: $argv" ;; esac
[ -z "$(ls -A "$XDG_RUNTIME_DIR/seagents" 2>/dev/null)" ] || fail "seed file left behind"
pass "the window is in the nested i3 as con_id $got; recorded argv is the user's ($argv); seed file gone"

# 2. wait for the seeded turn.
out=$("$SEA" wait it-a --after 0 --timeout 90s); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['reason']")" = turn ] || fail "wait: rc=$rc $out"
echo "$out" | jq_ "d['last_message']" | grep -q ALPHA || fail "wait: last_message: $out"
pass "wait --after 0 returned the turn (rc 0): $out"

# 2b. Nothing to wait for without a baseline; a baseline not yet passed times out.
out=$("$SEA" wait it-a --timeout 5s); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['reason']")" = idle ] || fail "idle wait: rc=$rc $out"
out=$("$SEA" wait @it-a --after 1 --timeout 2s); rc=$?
[ $rc -eq 2 ] && [ "$(echo "$out" | jq_ "d['reason']")" = timeout ] || fail "timeout wait: rc=$rc $out"
pass "wait with no baseline returns idle at once; --after 1 times out with rc 2"

# 3. status lists it.
s=$(status_of it-a)
[ "$(echo "$s" | jq_ "d['tag'], d['depth'], d['stops'], d['in_turn'], d['parent']")" = "it-a 0 1 False None" ] \
  || fail "status: $s"
echo "$s" | jq_ "' '.join(d['capabilities'])" | grep -q "turns" || fail "capabilities: $s"
pass "status --json lists it-a: tag, depth 0, stops 1, capabilities $(echo "$s" | jq_ "d['capabilities']")"

# 4. Lineage: a spawn run under a wrapper is that wrapper's child, capped, and
#    (its parent reporting no mode) launched at the least privileged mode.
"$SEA" run --tag boss -- sh -c "
  cd '$WORK'
  seagents spawn --tag kid ${HAIKU[*]} -- 'Reply with exactly: KID' > '$ROOT/kid.json' 2> '$ROOT/kid.err'
  SEAGENTS_MAX_CHILDREN=1 seagents spawn --tag kid2 -- 'never' > /dev/null 2> '$ROOT/kid2.err'
  echo \$? > '$ROOT/kid2.rc'
  sleep 60" < /dev/null > /dev/null 2>&1 &
ND_PIDS="$ND_PIDS $!"
for _ in $(seq 1 60); do [ -s "$ROOT/kid2.rc" ] && break; sleep 0.25; done
[ "$(jq_ "d['tag'], d['parent'], d['depth']" < "$ROOT/kid.json" 2>/dev/null)" = "kid boss 1" ] \
  || fail "lineage: $(cat "$ROOT/kid.json" "$ROOT/kid.err")"
meta_of kid | jq_ "d['argv']" | grep -q "'--permission-mode', 'plan'" \
  || fail "kid's mode: $(meta_of kid | jq_ "d['argv']")"
[ "$(cat "$ROOT/kid2.rc")" = 1 ] && grep -q "kid" "$ROOT/kid2.err" && grep -q "SEAGENTS_MAX_CHILDREN" "$ROOT/kid2.err" \
  || fail "breadth cap: rc=$(cat "$ROOT/kid2.rc") $(cat "$ROOT/kid2.err")"
pass "spawn under a wrapper records parent boss, depth 1, mode plan; a second child past the cap is refused: $(cat "$ROOT/kid2.err")"

# 5. focus.
"$SEA" focus it-a || fail "focus exited $?"
sleep 0.5
focused=$(nd_tree | python3 -c "
import json,sys
def walk(n):
    if n.get('focused'): print(n['id'])
    for c in n.get('nodes',[])+n.get('floating_nodes',[]): walk(c)
walk(json.load(sys.stdin))")
[ "$focused" = "$got" ] || fail "focus: focused is $focused, it-a is $got"
pass "focus it-a focused con_id $got"

# 6. The agent exits on its own: wait --for exit (already waiting, as a
#    caller would be) says so, and the window closes with it — a spawned
#    window keeps no shell behind.
"$SEA" wait it-a --for exit --timeout 30s > "$ROOT/exit.json" &
waiter=$!
sleep 1
lead=$(meta_of it-a | jq_ "d['child_pid']")
pkill -TERM -P "$lead" || fail "no agent process under __lead $lead"
wait $waiter; rc=$?
out=$(cat "$ROOT/exit.json")
[ $rc -eq 4 ] && [ "$(echo "$out" | jq_ "d['reason']")" = exit ] || fail "exit wait: rc=$rc $out"
for _ in $(seq 1 20); do [ -z "$(window_of it-a)" ] && break; sleep 0.25; done
[ -z "$(window_of it-a)" ] || fail "it-a's window stayed open after the agent exited"
pass "wait --for exit returned exit (rc 4), and the window closed with the agent"
exit 0
