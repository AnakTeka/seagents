---
name: seagents-orchestrate
description: Delegate work to other coding agents, each in its own terminal window, with the seagents CLI (spawn, wait, send, read, stop). Use when you want to fan out, parallelize, or hand off an independent piece of work (a review, a second approach, a long investigation, one task per module) to another agent and collect its result, instead of doing everything in this conversation.
---

# Orchestrating agents with seagents

`seagents` launches agents in real terminal windows the human can watch and
type into, and lets you drive the ones **you** spawned. Every command prints
JSON on stdout (read that, not stderr) and exits with a meaningful code.
`seagents <cmd> --help` is the authoritative reference.

## Spawn, or do it yourself?

Spawn when the piece of work is **independent** (it needs no back-and-forth
with you mid-way), **substantial** (minutes of work, not one command), and
ideally **parallel** with other work. Do it yourself when it is small, when it
edits the same files you are editing, or when explaining it would take longer
than doing it. A child starts with **none of your context**: no conversation,
no files you read, no decisions you made.

## The seed prompt

Write it for a stranger. It must say:

- the task, and what **done** means (the exact deliverable, where it goes);
- the relevant paths, commands, constraints and decisions — everything it
  needs, since it cannot ask you;
- its tag and yours: "You are `<tag>`, spawned by `<your tag>` via seagents";
- that **its final message is its report**: "End with a self-contained
  summary of what you did, what you found, and anything left undone — that
  final message is all I receive." Do not rely on it writing a file for you;
- with `--worktree`: the tree is a fresh checkout on branch `seagents/<tag>`
  with **none of your uncommitted changes** and a **cold build directory**
  (the first build is slow); say whether it should commit there.

Long or multi-line prompts go in a file: `--prompt-file F`.

## The loop

```sh
seagents spawn --tag review-auth --cwd "$PWD" --prompt-file /tmp/seed.md
# {"tag":"review-auth","child_pid":…,"wrapper_pid":…,"cwd":…,"parent":"<you>","depth":1,"stops":0}

seagents wait review-auth --after 0 --timeout 100s
# {"tag":"review-auth","reason":"turn","stops":1,"state":"idle","rule":null,
#  "last_message":"<its report>","transcript_path":…}
```

1. **spawn** returns once the agent is registered, not once it has worked.
   `stops` is its count of finished turns: 0 now.
2. **wait `--after N`** blocks until the turn after N ends. N is 0 after
   spawn, and the `stops` that `send` returned after a send.
3. **Timeouts are normal.** Keep `--timeout` below your shell tool's limit
   (Claude Code's Bash tool: 2 min by default) and on `reason: "timeout"`
   (exit 2) call the **same** `wait … --after N` again. Nothing is consumed.
4. Act on **`last_message`** — the child's final text for that turn.
5. Follow up with **send**; it waits until the child is idle, submits, and
   returns the new baseline:
   ```sh
   seagents send review-auth "Also check the token refresh path."
   # {"ok":true,"tag":"review-auth","stops":1,"paired":true}
   seagents wait review-auth --after 1 --timeout 100s
   ```
6. When `last_message` is not enough, **read** the conversation:
   `seagents read review-auth --tail 20` (text) or `--json` (one record per
   line). Its last line/record is a cursor; `seagents read review-auth --from
   '<cursor>'` later gives only what came after it.
7. **stop** it when you are done with it: `seagents stop review-auth` →
   `{"ok":true,"tag":"review-auth"}` once it has exited (its window closes).

`seagents status --json` lists every agent with `tag`, `parent`, `depth`,
`state`, `stops`, `in_turn`.

## What `wait` returns

| reason | exit | do |
|---|---|---|
| `turn` | 0 | read `last_message`; continue |
| `idle` | 0 | (no `--after`) nothing running; `last_message` is its last result |
| `timeout` | 2 | call the same `wait` again |
| `blocked` | 3 | **stop and tell the human**: which child (tag), and the gate (`rule`, e.g. a permission prompt or the folder-trust dialog). Never answer it yourself. Then wait again with the same N |
| `exit` | 4 | the child is gone (crashed, stopped, or someone closed it); `last_message` is its last turn's result. `wait` and `read` keep answering for an exited child for a day (until its tag is reused); `send`/`stop` exit 1 |
| `interrupted` | 5 | a human pressed Esc on that turn; it has no report. Ask the human, or `send` again |

A restored agent (after a reboot) starts again at `stops: 0`: call `wait`
without `--after` first to get your bearings.

## Refusals and limits

- `spawn` exits 1 with the reason on stderr. **Caps**: depth ≤ 3, ≤ 4 live
  children per parent, ≤ 12 spawned agents in total. The message names the
  children in the way: `wait` on one and `stop` it before spawning more.
  A taken `--tag`: pick another (or omit it; one is minted).
- `send`/`stop` exit 5 (`forbidden`): you may only drive **your own
  descendants** — never your parent, a sibling, or the human's own agents.
  Reading (`wait`, `read`, `status`) works on anyone.
- `send` exit 3 (`blocked`): the child waits at a gate; tell the human.
  Exit 2 (`busy_timeout`): it stayed busy for all of `--timeout`; nothing was
  sent — wait for its turn, then send again. Exit 4: it exited.
- A child's permission mode is yours by default and can never be higher.
- **Never pass `--answer-permission`** (refused for agents anyway: gates are the
  human's) **or `--now`** (it breaks the pairing between your prompt and the
  turn `wait` returns) **without stating the reason** to the human.

## Fan-out

```sh
for part in api db ui; do
  seagents spawn --tag "audit-$part" --prompt-file "/tmp/seed-$part.md"
done
for part in api db ui; do
  seagents wait "audit-$part" --after 0 --timeout 100s   # re-call on exit 2
done
```

Waiting one at a time is fine: a child that finished while you waited on
another returns at once. Handle each result as its `wait` returns (a
`blocked` one goes to the human while the others keep working), combine the
reports yourself, then `stop` each child. With `--worktree`, tell the human
which `seagents/<tag>` branches hold work to merge.
