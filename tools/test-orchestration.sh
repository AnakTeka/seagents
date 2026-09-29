#!/bin/bash
# Integration test for `seagents spawn` / `wait` / `send` / `read` / `stop` /
# `status` / `focus`, with a real Claude Code, inside a nested, isolated desktop
# (tools/lib-nested-desktop.sh: headless weston, rootful Xwayland on :9, its
# own i3, HOME at a scratch directory), so no window it opens reaches your
# session and the real cache is never touched.
#
#   tools/test-orchestration.sh [alacritty|terminator|alacritty-daemon] [--codex]
#
# --codex adds the same loop with a real Codex (skipped, with a note, when
# `codex` is not installed): a scratch CODEX_HOME holding only a link to the
# real auth.json and a config that trusts the work directory and picks the
# cheapest model, so the user's own ~/.codex config is never read or written.
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

# Launch as if from inside a Claude Code session — an agent calling `spawn`,
# which is also how the orchestrator runs this — so the launcher, not this
# script, must keep that session's variables from the agents under test.
# Only the ones not already set get stand-ins.
SESSION_VARS="CLAUDECODE CLAUDE_CODE_ENTRYPOINT CLAUDE_CODE_MESSAGING_SOCKET CLAUDE_CODE_MESSAGING_TOKEN
  CLAUDE_CODE_EXECPATH CLAUDE_CODE_SESSION_ID CLAUDE_CODE_CHILD_SESSION CLAUDE_CODE_SESSION_ATTENDED
  CLAUDE_PID CLAUDE_EFFORT AI_AGENT"
for v in $SESSION_VARS; do
  [ -n "${!v+x}" ] || export "$v=sg-test-stand-in"
done

TERMINAL=alacritty
CODEX=
for a in "$@"; do
  case $a in
    --codex) CODEX=1 ;;
    *) TERMINAL=$a ;;
  esac
done
# Codex's auth, found before HOME moves.
REAL_CODEX_HOME=${CODEX_HOME:-$HOME/.codex}

. "$REPO/tools/lib-nested-desktop.sh"
nd_init "$TERMINAL"
if [ -n "${REAL_HOME:-}" ]; then
  ln -s "$REAL_HOME/.claude" "$HOME/.claude"
  ln -s "$REAL_HOME/.claude.json" "$HOME/.claude.json"
fi
nd_start

WORK="$REPO/target/sg-it/orchestration"
# Codex runs in a directory of its own: its Linux sandbox leaves empty `.git`,
# `.codex` and `.agents` mount points in its cwd, and Claude reads that `.git`
# as the root of a separate, never-trusted project — every later Claude run in
# the shared directory would open on the folder-trust dialog.
CODEX_WORK="$REPO/target/sg-it/orchestration-codex"
for d in "$WORK" "$CODEX_WORK"; do
  mkdir -p "$d"
  for m in .git .codex .agents; do rmdir "$d/$m" 2>/dev/null; done
done
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
# `default`, not this machine's bypass: a prompt sent by reference must be
# readable without a permission prompt, and step 3d needs a real gate.
out=$(cd "$WORK" && "$SEA" spawn --tag it-a --permission-mode default "${HAIKU[@]}" -- 'Reply with exactly: ALPHA') \
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
[ -z "$(ls "$XDG_RUNTIME_DIR/seagents" | grep '^seed-')" ] || fail "seed file left behind"
pass "the window is in the nested i3 as con_id $got; recorded argv is the user's ($argv); seed file gone"

# 2. wait for the seeded turn.
out=$("$SEA" wait it-a --after 0 --timeout 90s); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['reason']")" = turn ] || fail "wait: rc=$rc $out"
echo "$out" | jq_ "d['last_message']" | grep -q ALPHA || fail "wait: last_message: $out"
pass "wait --after 0 returned the turn (rc 0): $out"

# 2a. The agent started clean of the calling session's variables (the
#     launcher's doing), kept the user's config dir, and so wrote a transcript.
anchor=$(meta_of it-a | jq_ "d['child_pid']")
agent=$(cat /proc/$anchor/task/$anchor/children | awk '{print $1}')
[ -n "$agent" ] || fail "no agent process under the anchor $anchor"
leaked=$(tr '\0' '\n' < /proc/$agent/environ | cut -d= -f1 | grep -xF "$(echo $SESSION_VARS | tr ' ' '\n')")
[ -z "$leaked" ] || fail "the agent (pid $agent) inherited: $leaked"
[ -z "${CLAUDE_CONFIG_DIR:-}" ] || tr '\0' '\n' < /proc/$agent/environ | grep -qxF "CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR" \
  || fail "the agent lost CLAUDE_CONFIG_DIR"
tp=$(echo "$out" | jq_ "d['transcript_path']")
[ -f "$tp" ] || fail "no transcript at '$tp'"
pass "the agent (pid $agent) has none of the calling session's variables, keeps CLAUDE_CONFIG_DIR, and writes $tp"

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
echo "$s" | jq_ "' '.join(d['capabilities'])" | grep -q "send stop" || fail "capabilities: $s"
pass "status --json lists it-a: tag, depth 0, stops 1, capabilities $(echo "$s" | jq_ "d['capabilities']")"

# 3b. send: a follow-up into the idle agent returns the baseline, and the next
#     turn is its answer.
out=$("$SEA" send it-a 'Reply with exactly: BRAVO'); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['ok'], d['stops'], d['paired']")" = "True 1 True" ] \
  || fail "send: rc=$rc $out"
out=$("$SEA" wait it-a --after 1 --timeout 90s); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q BRAVO || fail "wait after send: rc=$rc $out"
pass "send returned stops 1, paired; wait --after 1 got BRAVO"
# A body Claude would file as quoted <pasted_content> (more than 2 lines, or
# more than 700 characters) goes by reference: written to a prompt file in the
# agent's cwd, which it is told in one line to read — so it is carried out.
printf 'First line with "double" and '"'"'single'"'"' quotes,\n  an indented $HOME `line`,\n\nand after a blank: reply with exactly: CHARLIE' \
  > "$ROOT/body.txt"
wpid=$(meta_of it-a | jq_ "d['wrapper_pid']")
out=$("$SEA" send it-a --prompt-file "$ROOT/body.txt"); rc=$?
[ $rc -eq 0 ] || fail "multi-line send: rc=$rc $out"
out=$("$SEA" wait it-a --after 2 --timeout 90s); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q CHARLIE || fail "multi-line wait: rc=$rc $out"
pfile=$(ls "$WORK/.seagents/prompts/$wpid-2-"*.md 2>/dev/null)
[ -n "$pfile" ] && cmp -s "$pfile" "$ROOT/body.txt" || fail "the prompt file ($pfile) is not the body byte for byte"
[ "$(stat -c %a "$pfile")" = 600 ] || fail "the prompt file is not 0600"
echo "$out" | python3 -c "
import json,sys
d=json.load(sys.stdin)
users=[json.loads(l) for l in open(d['transcript_path'])]
texts=[r['message']['content'] for r in users if r.get('type')=='user' and isinstance(r.get('message',{}).get('content'),str)]
want='Read the file .seagents/prompts/$(basename "$pfile") (relative to your working directory)'
sys.exit(0 if any(t.startswith(want) and 'by the user.' in t and 'pasted_content' not in t for t in texts) else 1)" \
  || fail "the transcript lacks the one-line reference to $pfile"
pass "a 4-line body with quotes went by reference ($(basename "$pfile"), byte for byte, 0600) and was carried out: CHARLIE"

# 3c. A long multi-line body (> 700 chars), in --permission-mode default: read
#     without a permission prompt, and done.
python3 -c "
for i in range(1, 12): print(f'Step {i}: this line is filler describing an imaginary task, and you can safely ignore it.')
print('Reply with exactly: LONGOK')" > "$ROOT/long.txt"
[ "$(wc -c < "$ROOT/long.txt")" -gt 700 ] || fail "the long body is not long"
out=$("$SEA" send it-a --prompt-file "$ROOT/long.txt"); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['stops']")" = 3 ] || fail "long send: rc=$rc $out"
out=$("$SEA" wait it-a --after 3 --timeout 90s); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q LONGOK || fail "long wait: rc=$rc $out"
pass "a $(wc -c < "$ROOT/long.txt")-char, 12-line body was carried out in default mode: LONGOK"

# 3d. A gate: in default mode a Bash command stops at Claude's permission
#     prompt, which wait reports as blocked. A human answers it with a key,
#     and a wait issued the instant that returns sees the turn go on to its
#     end — not the gate it just answered.
rm -f "$WORK/gate-ok.txt"
out=$("$SEA" send it-a 'Use the Bash tool to run exactly this command: touch gate-ok.txt — then reply with exactly: GATED'); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['stops']")" = 4 ] || fail "gate send: rc=$rc $out"
out=$("$SEA" wait it-a --after 4 --timeout 90s); rc=$?
[ $rc -eq 3 ] && [ "$(echo "$out" | jq_ "d['reason']")" = blocked ] || fail "wait at the gate: rc=$rc $out"
rule=$(echo "$out" | jq_ "d['rule']")
out=$("$SEA" send it-a 'never'); rc=$?
[ $rc -eq 3 ] || fail "a plain send into the gate: rc=$rc $out"
out=$("$SEA" send it-a --answer-permission --keys 1 --no-submit); rc=$?
[ $rc -eq 0 ] || fail "answering the gate: rc=$rc $out"
out=$("$SEA" wait it-a --after 4 --timeout 90s); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q GATED || fail "wait right after the answer: rc=$rc $out"
[ -e "$WORK/gate-ok.txt" ] || fail "the gated command never ran"
pass "wait stopped at the gate ($rule, rc 3), a plain send was refused (rc 3); answered with --keys 1, an immediate wait returned the turn: GATED"

# 3e. read: the conversation so far as NDJSON — every answer, the gated Bash
#     call, the by-reference prompt as the user's line, a cursor last — and,
#     after one more turn, `--from` that cursor gives that turn and nothing
#     before it. Text mode ends with a cursor line `--from` takes whole.
echo "$(status_of it-a)" | jq_ "' '.join(d['capabilities'])" | grep -qw read || fail "no read capability: $(status_of it-a)"
"$SEA" read it-a --json > "$ROOT/read.ndjson"; rc=$?
[ $rc -eq 0 ] || fail "read --json: rc=$rc"
token=$(python3 -c "
import json,sys
recs=[json.loads(l) for l in open('$ROOT/read.ndjson')]
cur=recs.pop()['cursor']
said=' '.join(r['text'] for r in recs if r['kind']=='assistant')
for w in ['ALPHA','BRAVO','CHARLIE','LONGOK','GATED']:
    assert w in said, (w, said)
assert any(r['kind']=='tool_use' and r['name']=='Bash' and 'gate-ok' in r['input'].get('command','') for r in recs)
assert any(r['kind']=='user' and r['text'].startswith('Read the file .seagents/prompts/') for r in recs)
assert all(r['uuid'] and r['kind'] in ('user','assistant','thinking','tool_use','tool_result') for r in recs)
assert cur['leaf'] and cur['offset'] > 0 and cur['token'].startswith(f\"{cur['offset']}:{cur['leaf']}:\")
print(cur['token'])") || fail "read --json: $(tail -c 600 "$ROOT/read.ndjson")"
last=$("$SEA" read it-a --tail 4 | tail -1)
case $last in "-- cursor: "*) ;; *) fail "text read's last line is not a cursor: $last" ;; esac
out=$("$SEA" send it-a 'Reply with exactly: DELTA'); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['stops']")" = 5 ] || fail "delta send: rc=$rc $out"
out=$("$SEA" wait it-a --after 5 --timeout 90s); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q DELTA || fail "delta wait: rc=$rc $out"
"$SEA" read it-a --json --from "$token" > "$ROOT/read2.ndjson" || fail "read --from: rc=$?"
python3 -c "
import json
recs=[json.loads(l) for l in open('$ROOT/read2.ndjson')]
assert 'cursor' in recs.pop()
kinds=[r.get('kind') for r in recs]
assert None not in kinds, recs  # no rewound/reset
said=' '.join(r['text'] for r in recs if r['kind'] in ('user','assistant'))
assert 'DELTA' in said and 'GATED' not in said and 'ALPHA' not in said, said
" || fail "read --from gave more or less than the new turn: $(cat "$ROOT/read2.ndjson")"
new=$("$SEA" read it-a --from "$last") || fail "text read --from: rc=$?"
echo "$new" | grep -q "^DELTA" && ! echo "$new" | grep -q GATED || fail "text read --from: $new"
pass "read --json: all five answers, the gated Bash call, the by-reference line, a cursor; --from it after one more turn gave only DELTA ($(($(wc -l < "$ROOT/read2.ndjson") - 1)) records); the text cursor line works as --from"

# 4. Lineage: a spawn run under a wrapper is that wrapper's child, capped, and
#    (its parent reporting no mode) launched at the least privileged mode.
"$SEA" run --tag boss -- sh -c "
  cd '$WORK'
  seagents spawn --tag kid ${HAIKU[*]} -- 'Reply with exactly: KID' > '$ROOT/kid.json' 2> '$ROOT/kid.err'
  SEAGENTS_MAX_CHILDREN=1 seagents spawn --tag kid2 -- 'never' > /dev/null 2> '$ROOT/kid2.err'
  echo \$? > '$ROOT/kid2.rc'
  seagents wait kid --after 0 --timeout 90s > /dev/null
  seagents send kid 'Reply with exactly: KIDTWO' > '$ROOT/kidsend.json' 2>&1
  echo \$? > '$ROOT/kidsend.rc'
  seagents send it-a 'never' > '$ROOT/stranger.json' 2>&1
  echo \$? > '$ROOT/stranger.rc'
  seagents stop kid > '$ROOT/kidstop.json' 2>&1
  echo \$? > '$ROOT/kidstop.rc'
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

# 4b. The lineage rule in the wrapper: boss may send to and stop its child,
#     not an agent it did not spawn.
for _ in $(seq 1 480); do [ -s "$ROOT/kidstop.rc" ] && break; sleep 0.25; done
[ "$(cat "$ROOT/kidsend.rc" 2>/dev/null)" = 0 ] || fail "boss → kid send: $(cat "$ROOT/kidsend.json")"
[ "$(cat "$ROOT/stranger.rc" 2>/dev/null)" = 5 ] && grep -q '"forbidden"' "$ROOT/stranger.json" \
  || fail "boss → it-a should be forbidden: rc=$(cat "$ROOT/stranger.rc") $(cat "$ROOT/stranger.json")"
[ "$(cat "$ROOT/kidstop.rc" 2>/dev/null)" = 0 ] || fail "boss → kid stop: $(cat "$ROOT/kidstop.json")"
pass "boss sent to and stopped its child kid; sending to it-a was forbidden (rc 5)"

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

# 5b. A gate before any turn: Claude in a never-trusted directory opens on
#     the folder-trust dialog before its first prompt. A plain wait (no
#     baseline, no turn open) says blocked, as --after would — not idle.
#     No model call is made; stop takes a gated agent straight to SIGTERM.
UNTRUSTED=$(mktemp -d /tmp/sg-untrusted.XXXXXX)
(cd "$UNTRUSTED" && "$SEA" spawn --tag it-t "${HAIKU[@]}" -- 'never' > /dev/null) || fail "untrusted spawn"
for _ in $(seq 1 60); do [ "$(status_of it-t | jq_ "d['state']")" = blocked ] && break; sleep 0.25; done
out=$("$SEA" wait it-t --timeout 5s); rc=$?
[ $rc -eq 3 ] && [ "$(echo "$out" | jq_ "d['reason'], d['stops']")" = "blocked 0" ] || fail "plain wait at the trust dialog: rc=$rc $out"
"$SEA" stop it-t > /dev/null || fail "stopping it-t"
rm -rf "$UNTRUSTED"
pass "a plain wait on an agent at the trust dialog, no turn open, is blocked (rc 3, $(echo "$out" | jq_ "d['rule']"))"

# 6. stop: the agent exits (Claude's /exit), a waiter on its exit (already
#    waiting, as a caller would be) says so, its control socket is gone, and
#    the window closes with it — a spawned window keeps no shell behind.
sock="$XDG_RUNTIME_DIR/seagents/$(meta_of it-a | jq_ "d['wrapper_pid']").sock"
[ -S "$sock" ] || fail "it-a has no control socket at $sock"
"$SEA" wait it-a --for exit --timeout 30s > "$ROOT/exit.json" &
waiter=$!
sleep 1
out=$("$SEA" stop it-a); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['ok']")" = True ] || fail "stop: rc=$rc $out"
wait $waiter; rc=$?
out=$(cat "$ROOT/exit.json")
[ $rc -eq 4 ] && [ "$(echo "$out" | jq_ "d['reason']")" = exit ] || fail "exit wait: rc=$rc $out"
[ ! -e "$sock" ] || fail "the control socket outlived the agent"
for _ in $(seq 1 20); do [ -z "$(window_of it-a)" ] && break; sleep 0.25; done
[ -z "$(window_of it-a)" ] || fail "it-a's window stayed open after the agent exited"
[ -z "$(ls "$WORK/.seagents/prompts/$wpid-"* 2>/dev/null)" ] || fail "it-a's prompt files outlived it"
pass "stop it-a: wait --for exit returned exit (rc 4), the socket and its prompt files are gone, the window closed"

# 6b. After the reap (any status does it), it-a is a tombstone: wait still
#     answers for it — exit, or the turn it was asked about — and read still
#     reads its transcript; driving it needs a live agent.
"$SEA" status > /dev/null
[ -z "$(meta_of it-a)" ] && [ -f "$CACHE/it-a.exited" ] || fail "it-a was not reaped into a tombstone"
out=$("$SEA" wait it-a --timeout 5s); rc=$?
[ $rc -eq 4 ] && [ "$(echo "$out" | jq_ "d['reason'], d['stops']")" = "exit 6" ] \
  && echo "$out" | jq_ "d['last_message']" | grep -q DELTA || fail "wait on a reaped agent: rc=$rc $out"
out=$("$SEA" wait it-a --after 5 --timeout 5s); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['reason']")" = turn ] || fail "wait --after 5 on a reaped agent: rc=$rc $out"
"$SEA" read it-a --tail 2 | grep -q DELTA || fail "read of a reaped agent"
"$SEA" send it-a 'never' > /dev/null 2>&1; rc=$?
[ $rc -eq 1 ] || fail "send to a reaped agent: rc=$rc"
pass "reaped it-a: wait says exit (rc 4, last_message DELTA), --after 5 still gets its turn, read works, send refuses"

# 7. Codex: the same loop through its adapter — hooks injected as one trusted
#    `-c hooks=…` (no "hooks need review" dialog, or no turn would land), the
#    rollout read as the transcript, an Esc read as an interrupt, pastes of
#    any size as the user's words, `/exit` to stop.
[ -n "$CODEX" ] || exit 0
if ! command -v codex >/dev/null; then
  echo "SKIP: codex is not installed"
  exit 0
fi
export CODEX_HOME="$HOME/.codex-it"
mkdir -p "$CODEX_HOME"
ln -s "$REAL_CODEX_HOME/auth.json" "$CODEX_HOME/auth.json"
cat > "$CODEX_HOME/config.toml" <<EOF
model = "gpt-5.6-luna"
model_reasoning_effort = "low"

[projects."$REPO"]
trust_level = "trusted"
EOF
out=$(cd "$CODEX_WORK" && "$SEA" spawn --agent codex --tag it-c -- 'Reply with exactly: CHARLIE') \
  || fail "codex spawn exited $?: $out"
argv=$(meta_of it-c | jq_ "d['argv']")
[ "$argv" = "['codex']" ] || fail "codex recorded argv: $argv"
out=$("$SEA" wait it-c --after 0 --timeout 3m); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | jq_ "d['reason']")" = turn ] || fail "codex wait: rc=$rc $out"
echo "$out" | jq_ "d['last_message']" | grep -q CHARLIE || fail "codex last_message: $out"
tp=$(echo "$out" | jq_ "d['transcript_path']")
case $tp in "$CODEX_HOME"/sessions/*/rollout-*.jsonl) ;; *) fail "codex transcript: $tp" ;; esac
[ -f "$tp" ] || fail "no rollout at $tp"
pass "codex spawned; its hooks reported the seeded turn (CHARLIE) and its rollout $tp"

s=$(status_of it-c)
[ "$(echo "$s" | jq_ "' '.join(d['capabilities'])")" = "state wait turns spawn send stop read" ] \
  || fail "codex capabilities: $s"
wpid=$(meta_of it-c | jq_ "d['wrapper_pid']")
anchor=$(meta_of it-c | jq_ "d['child_pid']")
agent=$(pgrep -P "$anchor" | head -1)
leaked=$(tr '\0' '\n' < "/proc/$agent/environ" | grep -E '^(CLAUDECODE|CLAUDE_CODE_SESSION_ID|CODEX_THREAD_ID)=' || true)
[ -z "$leaked" ] || fail "codex (pid $agent) inherited: $leaked"
pass "status: codex has every capability; it started without the calling session's variables"

out=$("$SEA" send it-c 'Reply with exactly: XRAY'); rc=$?
n=$(echo "$out" | jq_ "d['stops']")
[ $rc -eq 0 ] && [ "$n" = 1 ] || fail "codex send: rc=$rc $out"
out=$("$SEA" wait it-c --after "$n" --timeout 2m); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q XRAY || fail "codex send's turn: rc=$rc $out"
pass "send it-c → stops $n; wait --after $n returned XRAY"

body="$ROOT/codex-long.txt"
python3 -c "print('\n'.join(['Line %d: filler for the paste test' % i for i in range(40)] + ['Ignore the lines above. Reply with exactly: LONGOK']))" > "$body"
out=$("$SEA" send it-c --prompt-file "$body"); n=$(echo "$out" | jq_ "d['stops']")
out=$("$SEA" wait it-c --after "$n" --timeout 2m); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q LONGOK || fail "codex long send: rc=$rc $out"
[ -z "$(ls "$CODEX_WORK/.seagents/prompts/$wpid-"* 2>/dev/null)" ] || fail "a codex paste went by reference"
pass "a 41-line send was pasted whole (no prompt file) and answered LONGOK"

# Esc mid-turn: Codex records turn_aborted, and no Stop comes.
out=$("$SEA" send it-c 'Run the shell command: sleep 60 && echo done'); n=$(echo "$out" | jq_ "d['stops']")
for _ in $(seq 1 40); do [ "$(status_of it-c | jq_ "d['state']")" = working ] && break; sleep 0.25; done
sleep 2
"$SEA" send it-c --now --keys '\e' --no-submit > /dev/null || fail "codex: sending Esc"
out=$("$SEA" wait it-c --after "$n" --timeout 30s); rc=$?
[ $rc -eq 5 ] && [ "$(echo "$out" | jq_ "d['reason']")" = interrupted ] || fail "codex Esc: rc=$rc $out"
out=$("$SEA" send it-c 'Reply with exactly: AFTER'); n=$(echo "$out" | jq_ "d['stops']")
out=$("$SEA" wait it-c --after "$n" --timeout 2m); rc=$?
[ $rc -eq 0 ] && echo "$out" | jq_ "d['last_message']" | grep -q AFTER || fail "codex after Esc: rc=$rc $out"
pass "an Esc'd codex turn is interrupted (rc 5); the next send's wait gets its own turn (AFTER)"

out=$("$SEA" read it-c)
for w in XRAY LONGOK AFTER; do echo "$out" | grep -q "$w" || fail "codex read lacks $w: $out"; done
echo "$out" | tail -1 | grep -q '^-- cursor: ' || fail "codex read cursor: $out"
pass "read it-c shows the conversation from the rollout, ending in a cursor"

"$SEA" wait it-c --for exit --timeout 30s > "$ROOT/cexit.json" &
waiter=$!
sleep 1
out=$("$SEA" stop it-c); rc=$?
[ $rc -eq 0 ] || fail "codex stop: rc=$rc $out"
wait $waiter; rc=$?
[ $rc -eq 4 ] || fail "codex exit wait: rc=$rc $(cat "$ROOT/cexit.json")"
pass "stop it-c: codex exited on /exit (wait --for exit rc 4)"
exit 0
