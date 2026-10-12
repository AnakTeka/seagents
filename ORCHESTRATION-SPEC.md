# seagents orchestration — spec

Design spec for `seagents spawn` / `wait` / `send` / `stop` / `read`: the ability
for you (or one running agent) to **launch, wait on, drive, and inspect other
coding agents** that seagents itself started.

> **Partly superseded (2026-10-12) by [`HERDR-BACKEND-SPEC.md`](./HERDR-BACKEND-SPEC.md).**
> Agents now run in a herdr server and seagents drives them through herdr's
> CLI: there is no pty wrapper, no control socket and **no lineage**. These
> sections describe what was built before and no longer hold: *Threat model*,
> *Tags and lineage* (tags remain, now `[a-z][a-z0-9_-]{0,31}`; lineage and
> its ancestry rule are gone), *Fan-out caps*, *Permission mode: a child never
> outranks its parent*, *Seeding lives in `run`* (a seed now goes in once the
> agent is idle, by reference), *Control channel*, *Sharing the master
> writer*, *Injection framing* (herdr frames), *The gate verdict has one
> source* (herdr's verdict, plus the adapter's `[screen] dialogs`), the
> socket and lineage parts of `seagents stop` and of *Safety*, and the
> vendored-detection parts throughout. The turn ledger, `wait`'s reasons,
> submit-when-idle, the input-box check, prompts by reference and `read` still
> hold as written, with the mechanism changes recorded in `HANDOFF.md`, which
> is the single source of truth for what is built.

Status: **implemented** (Phases 0–5) on branch `refactor/phase0`, as of
2026-09-29 (HEAD `c3d3ae8`). Written 2026-08-31, revised 2026-09-05,
adversarially revised 2026-09-28, **brought in line with the code 2026-09-29**
(see *Revision log* for what changed from the design and why). This document
now describes what was built and why; where it and the code disagree, the code
and [`HANDOFF.md`](./HANDOFF.md) win. Decisions are recorded with their
rationale; anything marked **verified** was checked on this machine (Claude
Code 2.1.283–284, codex-cli 0.152.0) on the date given. What was designed and
deliberately **not built** is said so, with the reason, under *Phasing*.

Related docs: [`HANDOFF.md`](./HANDOFF.md) (the operational reference: every
module, file format and flag as built, §6–§11 for orchestration) and
[`SESSIONS-SPEC.md`](./SESSIONS-SPEC.md) (the historical-corpus browser, whose
transcript layer this work built and it will read through). The agent-facing
workflow is [`skills/seagents-orchestrate/SKILL.md`](./skills/seagents-orchestrate/SKILL.md).
`seagents <cmd> --help` is the authoritative CLI reference.

Code is referenced by function name, not line number — line numbers rot.

---

## Problem

seagents was a pure **observer**: it watched agents and showed you which one
was blocked. Two things it could not do:

1. **Start** a new agent for a task without you opening a terminal, `cd`-ing, and
   typing the prompt by hand.
2. Let **one agent inspect or drive another** — the "agent A spawns agent B to go
   do X, waits for B, reads what B did, and sends B a follow-up" loop that turns
   a pile of terminals into an orchestrated fleet.

The primary caller of these commands is **an agent, not a human**: a `SKILL.md`
teaches an agent how to fan out work, and it does so by shelling out to these
commands and parsing their JSON. Humans get the same commands, plus the HUD's
`p`/`x` keys. This ordering (CLI first, machine-readable, HUD on top) is
decided.

Because the caller is an agent, every primitive must be **correct under an
agent's usage pattern**: called from a tool with a hard timeout, re-called after
that timeout, racing the 2 s detector tick, run in parallel with siblings. A
primitive that is "fine when a human uses it" (a human looks at the screen
before trusting `wait`) is not fine here.

herdr can do (2) because it *owns every agent's PTY*. seagents deliberately does
not host PTYs for you — but it *is* the parent of every agent launched through
`seagents run`, so this capability is **the point where seagents becomes an
actor, not just an observer**, drawn narrowly on purpose (see *Scope*).

## Why not scrape the terminal (the herdr way)

The obvious way for A to "read B" is what herdr does: scrape B's rendered screen.
We reject it for conversation content:

- A rendered terminal is ANSI color, cursor moves, spinner frames, alt-screen
  redraws, input-box chrome, and a status line. Recovering "what B said" means
  reconstructing semantics that already exist, structured, in the transcript.
- Scrollback is **bounded and lossy** — our own `.raw` is rotated to the last
  ~1 MB. Turn 1 of a long session is simply gone.
- The render is **geometry-sensitive** (wrong cols/rows → garbled; see HANDOFF).

Every coding tool worth driving writes its conversation to disk as structured
records. **The transcript is the source for content; the screen is the source
for liveness and gates.** The one place the screen is read for something other
than state is `send`'s input-box check (§*The input box*), which asks only "is
the box empty, or does it hold exactly what we sent" — never what anyone said.

## Why not the tools' own headless modes

Claude Code has `claude -p` / `--bg` / `attach`; Codex has `codex exec`. These
run the agent *outside* your terminal: you lose the window you can type into and
jump to from the HUD. Right for unattended pipelines, wrong for a fleet you still
supervise by hand. Out of scope, by design — and an agent may not reach them
through `spawn --arg` either (they are in each adapter's `[launch.privileged]`).

---

## Threat model

State it, because every safety rule below is sized against it:

- **Defended against: a confused agent.** An agent that loops, fans out too far,
  addresses the wrong tag, sends into a permission prompt, pastes a follow-up
  into your own main session, or asks for a child with more permissions than it
  has. These are the realistic failures, and the rules below make them
  structurally impossible or loud.
- **Not defended against: a malicious agent.** Any agent with a shell already
  runs as you: it can `kill` your processes, write to your files, or escape its
  process tree (`setsid`, `systemd-run --user`, `i3-msg exec`) and then look like
  a human caller. seagents does not pretend to sandbox that; the tool's own
  permission system and your review are the boundary.

Consequence: guards are designed to be **unforgeable by accident** (derived from
process ancestry, not from environment variables an agent passes through or
edits casually) and are documented as **forgeable on purpose**.

---

## Scope and non-goals

**In scope:** spawn / wait / send / stop / read over agents **in the registry**
(every agent launched through `seagents run`: the `claude = seagents run claude`
alias, `restore`, or `spawn`). Bundled adapters: Claude Code and Codex.

**Who may act on whom** (the *lineage rule*, see *Lineage*):

- A **human caller** (no seagents wrapper among its process ancestors) may act on
  any registry agent, and may answer gates.
- An **agent caller** may `send`/`stop` only its **own descendants**, and may
  `wait`/`read` any registry agent. Reading is harmless; driving your main
  working session from a child is not.

**Explicitly out of scope:**

- **Hand-started agents stay observe-only.** A bare `claude` launched without
  seagents is not in the registry: seagents is not its parent (cannot inject),
  and does not tee it. *seagents drives what it launched, and only that.*
- **No screen-scraping for content.** State from `.raw`, content from the
  transcript.
- **Not a multiplexer.** Agents still run in your own i3 terminals.
- **No cross-machine anything.** Local sockets and local files only.

---

## Multi-tool from the start: agent adapters

seagents detects state for herdr's whole agent roster because detection is
data-driven per agent. Orchestration follows the same rule: **the core is
generic; everything a specific tool needs lives in that tool's adapter.** Claude
Code was the first adapter, not the design; Codex, the second, proved it — it
needed no new core concept, only facts that had been quietly Claude-shaped
turned into adapter data (hook injection, resume argv, permission argv, the
transcript format). The adapter key is the agent id the vendored engine's
`identify_agent()` derives from `argv[0]`, so detection and orchestration always
agree on what a session is.

| capability | generic core (written once) | per-agent adapter |
|---|---|---|
| state | PTY tee, herdr engine | herdr manifests (upstream), `patches.rs` re-weights/additions |
| identity + turns | `__hook`, `.agent` ledger, `turn::Ledger` | `[hooks]`: how hooks reach the command line; `identity`/`turns` |
| spawn | launcher shared with `restore`, tags, lineage, caps | `seed_prompt`; `permission_modes` + `[launch.permission_argv]`; `[launch.argv_mode]`; `[launch.privileged]`; `session_env` |
| send / stop | control socket, shared writer, paste framing, delayed submit, input-box check | `[send]`: submit delay, pre-send nudge, exit sequence, long-prompt policy, input glyph/clear keys |
| read | index, main-line walk, collapse rules, cursor | `[read] transcript`: which parser |
| restore | layout capture, swallow, tag dedup | `[restore]`: resume argv, stale words to drop |

Adapters are TOML, one file per agent, bundled from `src/adapters/` and
replaced **whole** by `~/.config/seagents/agents/<agent>.toml` (an override that
does not parse is an error, not a silent fall-back to the defaults it was
written to change). Every table is `deny_unknown_fields`. Any command no
detection manifest knows gets the generic adapter: no seed, no hooks, no modes,
state only. The schema as built, from `claude.toml` (trimmed):

```toml
[launch]
seed_prompt      = ["--", "{prompt}"]   # appended to the CHILD's argv only; empty = spawn refuses
permission_modes = ["plan", "default", "acceptEdits", "bypassPermissions"]  # least → most privileged
identity         = "hook"               # hook | none  ("watch"/watch_dir are refused: not built, see Phasing)
turns            = "hook"               # hook | screen
seed_by          = "file"               # seed via a 0600 prompt file; argv holds only the pointer line
session_env      = ["CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_MESSAGING_SOCKET", "…"]
config_flags     = []                   # Codex: ["-c", "--config"], for privileged.config_keys

[launch.permission_argv]                # every mode above, and nothing else
plan              = ["--permission-mode", "plan"]
bypassPermissions = ["--permission-mode", "bypassPermissions"]
# …

# [launch.argv_mode]                    # Codex only: read the parent's mode off its argv too
# aliases = { "--sandbox" = "-s", … }; config_keys = { sandbox_mode = "-s", … }; flags = { "--yolo" = "bypassPermissions", … }

[launch.privileged]                     # what an AGENT may not pass via spawn --arg, one reason each
flags    = ["--permission-mode", "--dangerously-skip-permissions", "--settings", "--allowedTools", "--add-dir", "--mcp-config", "-p", "…"]
commands = ["mcp", "plugin", "auth", "…"]
# config_keys = [...]                   # Codex: keys behind config_flags (approval_policy, sandbox_mode, hooks, …)

[hooks]
inject         = "settings"             # settings (Claude --settings JSON) | config (Codex -c hooks={…}) | none
flags          = ["--settings"]         # first is passed; the rest are how a user's own may be spelled
events         = ["SessionStart", "UserPromptSubmit", "Stop"]
hookless_flags = ["--bare"]             # under these the tool runs no hooks: nothing injected

[send]
submit_delay_ms  = 300                  # body, wait, then Enter (herdr's value)
pre_send         = ""                   # e.g. "focus_gained" (ESC[I) for copilot
exit_sequence    = "/exit"              # typed as a --now paste by stop
long_prompt      = "file"               # file | paste — over the limits below → by reference
inline_max_chars = 700
inline_max_lines = 2
input_glyph      = "❯"                  # the input box's first-row marker (Codex: "›")
input_clear      = "\\x15"              # Ctrl+U, pressed until the box is empty

[read]
transcript = "claude"                   # claude | codex — parser for read and the interrupt check

[restore]
resume = ["--resume", "{session}"]      # Codex: ["resume", "{session}"] (a subcommand)
drop   = ["--resume", "-r", "--continue", "-c"]
```

The seed prompt goes after `--` so a prompt beginning with `-` is never parsed as
a flag. Transcript parsers are Rust, one per format (`transcript/claude.rs`,
`transcript/codex.rs`) — the only per-agent code. **Capabilities are derived,
never stored** (`adapter::capabilities`), and `status --json` reports them per
agent so a skill driving a mixed fleet knows what it can do to each child:
`state` (a detector knows it), `wait` (always), `turns` (exact hook ledger),
`spawn` (a seed template), `send` (a detector to say idle; `--now` works on
anything), `stop` (always), `read` (a parser and a way to learn the path). Both
bundled adapters report all seven.

**Why Claude's mode words for Codex.** Codex has no mode flag — `-a` (approval
policy) and `-s` (sandbox) instead — but its hooks report `permission_mode` in
Claude's four words. Naming every bundled adapter's modes in that one
vocabulary lets a parent of either tool bound a child of either tool.

---

## The registry: one module, locked updates

Everything keys off `~/.cache/seagents/<wrapper_pid>.*`. Before Phase 0 `.meta`
had four independent writers across processes, each a non-atomic
read-modify-write: a reader could see torn JSON (a row vanishing for a tick) and
two writers could drop each other's update (a healed con_id silently lost).
Single-writer-per-file is not achievable (window healing legitimately happens
from other processes), so the rule is:

- **Every sidecar is read and written only through `registry.rs`.** No other
  module builds a `<pid>.*` path or parses one. `Dirs` names the two roots (the
  cache and the runtime dir, where sockets live); `Files` is one session's
  paths.
- **Every change is a locked update** (`update_meta` / `update_state` /
  `update_agent`): `flock` on the session's `<pid>.lock`, read, mutate the typed
  record, write a pid-suffixed tmp file, rename over, unlock. Readers take no
  lock. tmp+rename fixes torn reads; only the lock fixes lost updates (the
  concurrency test fails without it: 33 of 400 increments survived).
- Records are **typed** (`serde`), every field but `wrapper_pid` optional, and
  unknown fields carried through untouched: live wrappers outlast a reinstall,
  so a record may come from an older or newer binary.
- **Liveness is one rule** (`procfs::alive(child_pid)`) and two operations:
  `list()` never deletes (the snapshot capture runs it inside other wrappers'
  monitors); `reap()` deletes a dead session's files, called only by
  `status::collect` (so `status` and the HUD).

| file | contents | written by |
|---|---|---|
| `<pid>.meta` | who/where: pids, **user's** argv, cwd, geometry, window ids, `tag`, `parent`, `depth`, `hooks`, `engaged_at` | born once in `TagClaim::create_meta`; window healing from any process |
| `<pid>.raw` (+ `.raw.tmp`) | the tee | wrapper output pump, rotated at 16 MB to ~1 MB |
| `<pid>.state` | screen state, `since`, `seen`, `rule` (while blocked), `screen` (the detector's own verdict when the ledger overrode it) | the monitor, and any reader whose detection disagrees |
| `<pid>.agent` | identity + turn ledger | `__hook` |
| `<pid>.lock` | empty; the per-session `flock` | — |
| `tags.lock` | the cache-wide tag lock | — |
| `<tag>.pending` | a spawn's tag reservation `{tag, parent, spawner}` | `claim_spawn`; consumed by the spawned `run` |
| `<tag>.exited` | tombstone of a reaped session (below) | `reap` |
| `sessions/<id>.idx` | a transcript's offset index | `read`, swept daily |
| `<agent cwd>/.seagents/prompts/<pid>-<stops>-<4hex>.md` | a by-reference `send` body (0600) | the control thread via `registry::write_prompt` |
| `<runtime>/<pid>.sock` | control socket (0600 in a 0700 dir) | wrapper |
| `<runtime>/seed-<tag>` | a spawn's seed prompt (0600), deleted by `run` at startup | `spawn` |

`.agent` stays a separate file even with locking: it churns on every prompt and
turn end, `.meta` is identity that changes almost never, and keeping them apart
means the HUD's hot path never contends with a turn boundary.

**Runtime dir** (`registry::runtime_dir()`, the one place it is decided — the
wrapper binding, every client, `spawn`'s seed file): `$XDG_RUNTIME_DIR/seagents`
unless that is unset, can't be created, or is too long for a socket path
(`sun_path` is 108 bytes with its NUL; room is kept for a 10-digit pid). Then
`/tmp/seagents-<uid>`, used only if `lstat` says it is a real directory owned by
us (a loosened mode is tightened; a symlink or someone else's directory is
refused — it would hand them our sockets and seed prompts — and the refusal
travels in `Files::sock` as an `Err`, so `send` says why there is no socket).
Verified 2026-09-29 with a 119-byte `XDG_RUNTIME_DIR`.

**Socket sweep.** The wrapper removes its socket on exit and on SIGHUP/SIGTERM.
`reap()` removes one only when no live session owns it **and** its pid is dead
or provably not seagents (`procfs::runs_seagents`: neither `/proc/<pid>/exe`
nor argv[0] is named `seagents…`; unreadable keeps it). "Not in this cache"
alone was the first rule, and it was wrong: the runtime dir is shared by every
seagents whatever its `HOME` (a test's scratch HOME, `sudo -E`), so such a run
deleted the user's live agents' sockets. Verified 2026-09-29.

**Tombstones.** `reap` writes `<tag>.exited` (`{tag, wrapper_pid, exited_at,
agent, cwd, stops, last_message, transcript_path, session_id}`) before it
deletes anything, unless a live agent holds the tag. `resolve_exited()` (live,
then a dead unreaped session by that tag, then the tombstone) and
`final_agent()` (the ledger from `.agent` or, after a reap, the tombstone) are
what `wait` and `read` use, so a child that finished and exited between two
calls still answers — even to a waiter that raced the reap. Removed when a new
agent claims the tag and by `reap` after 24 h. `send`, `stop` and `focus` use
`resolve_target()`, live agents only.

### `.meta` — fields added

```jsonc
{
  "wrapper_pid": 1437882, "child_pid": 1437883,
  "cwd": "/home/me/.../repo",
  "argv": ["claude"],       // the command run was given: never the injected hook flag or the seed
  "started": 1783468316, "cols": 212, "rows": 53,
  "con_id": 103471613296720, "window_id": 30728026,
  "tag": "reviewer",        // every agent has one (see Tags)
  "parent": "planner",      // spawner's tag; null without one
  "depth": 1,               // parent's + 1; 0 without a parent
  "hooks": true,            // run handed the agent our hooks
  "engaged_at": 1783468320  // first prompt given (seed past any gate, or a submitted send)
}
```

`argv` never holds what `run` injects — the hook flag or the seed prompt — because
`restore` relaunches from it (`resume_argv`): a seed left there would be
**re-submitted into the resumed conversation after every reboot**, and an
injected `--settings` would stack on each restore. What `spawn` puts on the
command line *before* `run` (the permission argv, `--arg`s) is the user-facing
command and is recorded, so a restored child comes back in the same mode.

`session_id` / `session_id_source` left `.meta` for `.agent` (see *Cutovers*).

### `.agent` — identity and the turn ledger

```jsonc
{
  "transcript_path": "/home/me/.claude-work/projects/…/1f81….jsonl",
  "session_id": "1f81…",
  "source": "startup",              // startup | resume | clear | compact
  "permission_mode": "acceptEdits", // last reported (UserPromptSubmit / Stop)
  "stops": 7,                       // +1 at every Stop; never resets
  "prompted_at": 1783468390120,     // last UserPromptSubmit (Unix ms)
  "stopped_at": 1783468399870,      // last Stop (Unix ms)
  "last_message": "…",              // final assistant text of the last Stop
  "updated": 1783468399870
}
```

Timestamps are **milliseconds**: a prompt in the same second as the last `Stop`
is the common case, and seconds read it as not after it.

**The ledger counts `Stop`s; it does not pair prompts with turns.** Pairing by
`prompt_id` was the design and it is wrong (**verified 2026-09-29**): a message
typed while a turn runs fires `UserPromptSubmit` immediately *reusing the
running turn's `prompt_id`*, and its answer arrives as a **second `Stop` with a
fresh `prompt_id`**; an Esc-interrupted turn fires `UserPromptSubmit` and **no
`Stop` at all**. `Stop` count is the only monotonic, reliable edge. `stops`
never resets — not on `/clear`, not on resume — so a caller's baseline stays
comparable for the wrapper's whole life.

**"Is it in a turn?"** is one function, `turn::Ledger::in_turn()`, used by
`status`, `send` and `wait`. For hook adapters, all three of:

1. `prompted_at > stopped_at`;
2. **no interrupt record** in the transcript since the prompt, read by the
   adapter's parser (`interrupted_since`). Claude writes a `user` record
   `[Request interrupted by user]` (or `… for tool use]`) on Esc; the parser
   looks in the last 256 KiB, newest first, stopping at the first record older
   than the prompt, and parses nothing when the marker isn't in the tail.
   Codex writes `turn_aborted` — even for an Esc before the first token.
3. **not dropped before it began** (the *dropped-prompt rule*). An Esc before
   Claude's first response puts the prompt back in the input box and leaves no
   trace — no interrupt record, no `Stop`, no hook (there is no cancel event),
   and the transcript lines after the prompt are the ones every turn writes at
   its start. With no exact trace, the rule reads the absence of everything a
   live turn does: the detector's own screen verdict (`.state.screen`) is idle,
   **and** the tee has been silent for 10 s since the prompt, **and** the
   transcript holds no assistant record (text, reasoning, tool call) since it
   (`transcript::responded_since`). Thinking and a slow first token show a
   spinner whose timer repaints every second; a streamed answer grows the tee;
   a tool call is an assistant record; a gate is `blocked`. Verified
   2026-09-29: the dropped prompt ends as `interrupted` (rc 5) ~12 s after the
   Esc, while a 25 s foreground command and a streamed 200-word answer still
   end as turns. The monitor re-looks every tick while its agent is `working`,
   since nothing on disk changes at that moment. Codex never meets this rule.

The screen does **not** otherwise end a turn: Claude 2.1.284 streams a
plain-text answer with no spinner and an idle title, so the detector says
`idle` mid-answer, and a turn Esc'd before the detector ever saw it `working`
would never have ended. For hookless (screen) adapters `in_turn` is
"prompted after the last `Stop`, and `.state` not idle since" — but see
*Phasing*: nothing writes a ledger for them, so it is never true.

**The state column** for hook adapters follows (`status::settle`): `blocked`
from the screen wins; else in a turn → `working`; else the screen's verdict. The
fused state is what `.state` records, so the done marker and the "finished"
notification fire when the turn really ends. Any observer (monitor, HUD,
`status`) that sees the working→idle edge marks it unseen (`done`); when only
the monitor's sighting counted, the HUD's 750 ms poll won the race and a
finished turn showed idle.

---

## Identity: learning the transcript

### `hook` — Claude Code and Codex

Both tools run configured commands at lifecycle events with JSON on stdin, and
Codex's hooks turned out to be Claude's: same events, same payload fields.
**Verified 2026-09-28** (Claude) and **2026-09-29** (Codex 0.152.0), trimmed:

```jsonc
// SessionStart
{"session_id":"cd92…","transcript_path":"…/cd92….jsonl","cwd":"…","hook_event_name":"SessionStart","source":"startup"}
// UserPromptSubmit
{"session_id":"…","transcript_path":"…","prompt_id":"68f0…","permission_mode":"plan","hook_event_name":"UserPromptSubmit","prompt":"…"}
// Stop
{"session_id":"…","transcript_path":"…","prompt_id":"68f0…","permission_mode":"bypassPermissions","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"ok", …}
```

Codex's `transcript_path` is its rollout
(`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`), and its
`SessionStart` fires at the first prompt, not at launch.

`run` hands a hook-capable agent **one** extra flag (`hook::inject`) carrying
three hooks — `SessionStart`, `UserPromptSubmit`, `Stop` — each running
`'<abs path of current_exe>' __hook`, placed right after `argv[0]` (anything
after a `--` is the agent's positional text). `__hook` finds its session by
walking its own `/proc` ancestry to the first pid with a live `.meta`
(`registry::enclosing_session`; hook sh → agent → `__lead` → wrapper) and records
the event in `.agent`:

- `SessionStart` → `transcript_path`, `session_id`, `source`.
- `UserPromptSubmit` → `prompted_at`, `permission_mode`.
- `Stop` → `stops += 1`, `stopped_at`, `last_message`, `permission_mode`.
- **Every** event refreshes `transcript_path`/`session_id`, so a lost
  `SessionStart` heals at the next prompt.

Why identity **and** turns from the hook, when herdr uses Claude's hook for
identity only: `wait` needs an exact "the turn I care about has ended" edge, and
the screen cannot give one (see *`wait`*).

**The `__hook` contract** (a hook that misbehaves changes the agent): always
exit 0 (a `Stop` hook exiting 2 *keeps Claude from stopping*), nothing on stdout
(hook stdout can be read as a decision), catch its own panics, accept any
arguments; one ancestry walk, one small read, one rename — no i3, no detection.
Failures go to `~/.cache/seagents/hook.log` only under `SEAGENTS_DEBUG`. The
wrapper forks the agent before its `.meta` exists, so a miss is retried for up
to ½ s.

**Claude: one merged `--settings` (`inject = "settings"`).** Verified
2026-09-28: repeated `--settings` flags **replace** each other (only the last
flag's hooks ran), while hooks from settings *files* and from the flag coexist.
So the user's settings files are untouched (seagents never writes the user's
config), and a `--settings X` the user passed (inline JSON or a path, either
spelling) is loaded, has our entries appended to its `hooks.<Event>` arrays, and
goes back as ONE inline flag in its place. An X that can't be read or isn't a
settings object passes through untouched and the agent runs without hooks, with
a warning. Under `--bare` (`hookless_flags`) nothing is injected.

**Codex: one `-c hooks={…}` override (`inject = "config"`).** Codex keeps `-c`
overrides as their own "session flags" config layer beside the user's. **A hook
runs only once trusted** (`codex exec` silently skips untrusted hooks; the TUI
stops at "Hooks need review"):
`hooks.state."<layer>:<event_snake>:<group>:<handler>".trusted_hash` = `sha256:`
of the canonical JSON of the hook's normalized identity (codex-rs `hook_hash`,
reproduced exactly and pinned by a test against the hash Codex wrote in
`/hooks`). Codex reads trust from the session-flags layer too, so `run` passes
one `-c hooks={…}` inline table carrying our three hooks **and their trust** —
nothing under `~/.codex` is written, and `--dangerously-bypass-hook-trust` is
not used (it would also run the user's unreviewed hooks). A dotted
`-c 'hooks.state."…".trusted_hash=…'` does not work (Codex splits keys on every
`.`), hence the one table; a user's own `-c hooks…` overrides are applied in
order, as Codex would, and merged into it. Codex's `Interrupt` hook exists but
is not injected: the rollout's `turn_aborted` is read through the same
`interrupted_since` seam as Claude's interrupt record. **Verified 2026-09-29.**

**Hook health.** A hook agent whose hooks never fire — a future Codex hashing
trust differently from our copy, a Claude changing what `--settings` means, a
user's `disableAllHooks` — keeps an empty ledger, and `wait` would block to its
timeout with no clue. So `.meta` records that hooks were injected (`hooks`) and
when the agent was first given a prompt (`engaged_at`: the monitor, once a
seeded agent is past any gate; the control thread, on a submitted `send`).
`hook::health`: ≥ 20 s after `engaged_at` with no `session_id` (which every
event carries) → silent. "Given a prompt", not "seen working": Codex's screen
reads `working` while it boots an MCP server before any prompt, and its first
hook fires only at the first prompt (measured). Surfaces as `wait` →
`no_hooks` (rc 6) and `status --json` → `hooks_ok` (`null`: no hooks;
`false`: silent). Verified 2026-09-29 with `claude --settings
'{"disableAllHooks":true}'`.

Caveats: `/clear` (and compact, and resume) fires `SessionStart` again with the
new transcript, so the id is always the conversation the window is writing
*now* — the case a minted `--session-id` would have gotten wrong.

### `watch` and `none`

`none`: `.agent` holds nothing; state + send (and `--now`) + stop only.
`watch` (inotify on a session directory for tools with no hooks) parses in the
schema but is **not built** — see *Phasing*.

---

## Tags and lineage

### Every agent has a tag

A tag is the agent's **one name**: the address for every command, the window
handle `restore` swallows on (`launch::handle_for(tag)` = `seagents-<tag>`), and
the lineage key. Before this, restore minted its own names (`tag_for()`:
session-id prefix or `p<pid>`) in the same handle namespace; cutover: **`run`
assigns the tag, once, for every agent**.

- `run --tag T` (from `spawn --tag`, or `restore` with the captured tag), else
  minted by `run`: `<cwd basename>-<4 hex>`.
- Charset `[a-z0-9][a-z0-9-]{0,31}`, **not all digits** (so a bare target is
  unambiguous against a `child_pid`), because it lands in an X11 property, a
  branch name, and a filename.
- `registry::claim_tag()` checks uniqueness among live agents under an exclusive
  `flock` on `tags.lock` and **keeps it held** through the pty open and fork
  until `TagClaim::create_meta()` — the only way a `.meta` is born — has written
  it. A malformed or taken `--tag`, or a `--parent` that isn't live, exits 2
  before anything is spawned.
- Minting avoids every tag in the saved layout(s), so a new agent can't take a
  placeholder a pending restore will want. A *requested* `run --tag` may be one
  of those — that is how `restore` relaunches under the old name — but
  `spawn --tag` may not.

**Spawn reservations.** `spawn` and the `run` it launches are separate processes
with a terminal between them, so `tags.lock` can't be held from `spawn`'s check
to the child's `.meta`. `claim_spawn()` instead writes `<tag>.pending`
(`{tag, parent, spawner}`) under the lock; other claims treat the tag as taken
and **count it against the caps**, the spawned `run --tag` consumes it in
`create_meta()`, and it lives only as long as the spawner's pid (dead ones are
swept by the next claim).

### Lineage is derived from process ancestry, not the environment

**"Who is calling" = `registry::enclosing_session(pid)`: the first `/proc`
ancestor of the calling process that is a live wrapper.** None → human. An env
var (the design's `SEAGENTS_WRAPPER_PID`) would be inherited by everything an
agent runs and trivially lost or rewritten.

- `spawn` applies it to its own pid → child's `parent`, `depth`.
- The control socket applies it to the **`SO_PEERCRED`** pid of every request,
  so a process writing to the socket directly is judged exactly like the CLI.
  The check lives in the wrapper; the CLI checks nothing itself.
- **"Human" is a positive finding, never an absence.** The peer is pinned with a
  pidfd at accept; if its ancestry cannot be read at any link, or it is gone
  by the time the walk ends, the request is refused ("caller identity
  unknown"). The first build treated an unreadable ancestry as a human: an
  agent could connect, let the connecting process exit, and send from a forked
  child with full rights (`stop --force` on an unrelated agent — reproduced in
  review 2026-09-30, and now an integration step).

Escaping the tree (`setsid` + double fork, `systemd-run`) makes an agent look
human. That requires deliberate effort, which is the line the threat model draws.

**The lineage rule:** an agent caller may `send`/`stop` a target only if walking
the target's `parent` chain (by tag, over live `.meta`s) reaches the caller's
tag — so never itself, its parent, a sibling or a stranger. This alone makes
A→B→A ping-pong impossible, so the design's earlier rate limit was dropped.
Lineage by tag (not pid) survives `restore` and cannot be inherited through pid
reuse.

### Fan-out caps

For **agent callers only** (a human's spawns are uncapped, like opening
terminals by hand), checked in `claim_spawn` under `tags.lock`, counting
reservations:

- depth ≤ 3 (`max_depth`),
- ≤ 4 live children per parent (`max_children`),
- ≤ 12 live agent-spawned agents machine-wide (`max_spawned`; counts agents
  that have a parent).

The bounds are the user's, from `~/.config/seagents/config.toml` `[spawn]`
(compiled defaults otherwise) — **never the environment**: the first build read
`SEAGENTS_MAX_*` from the spawning process, i.e. from the calling agent, which
could lift every cap by setting them (found in review 2026-09-30). The same
one check binds a direct `run --parent P`, which used to bypass the caps.

The refusal names the children in the way, so the skill can `wait` on one
instead of piling on. Defaults decided 2026-09-29, to be tuned from use.

---

## Command surface

```
seagents run [--tag T] [--parent P] [--seed-file F] <cmd> [args]
seagents spawn [--cwd DIR] [--tag T] [--agent claude|codex] [--permission-mode M]
               [--worktree] [--arg A]... (--prompt-file F | [--] PROMPT)
        → {"tag","child_pid","wrapper_pid","cwd","parent","depth","stops":0}
seagents wait <target> [--after N] [--for turn|exit] [--timeout DUR]
        → {"tag","reason","stops","state","rule","last_message","transcript_path"[,"message"]}
seagents send <target> [--now] [--keys] [--no-submit] [--answer-permission]
              [--timeout DUR] (--prompt-file F | [--] TEXT)
        → {"ok":true,"tag","stops","paired"} | {"ok":false,"reason","error"[,"rule"]}
seagents stop <target> [--force]
        → {"ok":true,"tag"}
seagents read (<target> | --transcript PATH) [--json] [--from CURSOR] [--tail N] [--raw]
seagents status [--json]    gains tag, parent, depth, stops, in_turn, transcript_path,
                            session_id, hooks_ok, capabilities
seagents focus <target>     now also takes a tag
```

There is **no `ls`**: it would be a second listing of the same registry beside
`status`. `<target>` is `@tag`, `tag`, or an all-digits `child_pid`, resolved by
**one** resolver shared by every command. The CLI is `clap`-derived (Phase 0),
so per-command `--help` is generated and cannot drift from the parser — which
matters because the primary caller learns the interface from `--help`. Every
flag takes `--flag value` and `--flag=value`; usage errors exit 2.

**JSON shapes are a stable contract.** Adding fields is fine; renaming or
removing is breaking, and the skill states them, so a change to any flag, shape
or exit code changes the skill in the same commit. Stdout carries only JSON;
reasons go to stderr where a command has no JSON error shape.

**Exit codes.**

| command | 0 | 1 | 2 | 3 | 4 | 5 | 6 |
|---|---|---|---|---|---|---|---|
| `wait` | `turn`, `idle` | error (unknown target, N ahead of the ledger) | `timeout` | `blocked` | `exit` | `interrupted` | `no_hooks` |
| `send` | ok | anything else, incl. `unsupported` | `busy_timeout` (nothing sent) | `blocked` | `exit` | `forbidden` | `draft` (nothing sent) |
| `stop` | ok | any other failure | — | — | — | `forbidden` | — |
| `spawn` | ok | any refusal (reason on stderr) | usage | — | — | — | — |

---

## `seagents spawn`

In order, as built (`spawn.rs`); everything that can refuse does so **before a
window opens**:

1. **Check the prompt.** Non-empty, no NUL, and it must fit the adapter's
   `seed_prompt` template as one argv string under `MAX_ARG_STRLEN` (128 KiB).
   An adapter with no template, or a larger prompt, is refused — the design's
   paste-once-idle seed path is not built, as no bundled adapter needs it
   (verified: a 50-line, ~5000-char positional seed arrives inline). An `--arg
   --` is refused (the agent's arguments end where the seed begins).
2. **Resolve cwd** (`--cwd`, else the caller's; canonicalized).
3. **Resolve the caller** (ancestry), and for an agent its current mode: its
   hooks' `permission_mode`, bounded by its own argv where the adapter says the
   hooks report less (`Launch::bound_mode`, below).
4. **Resolve the permission argv** (below) and, for an agent caller, **check
   `--arg`s** against the adapter's `[launch.privileged]`.
5. **Claim the tag and check caps** (`claim_spawn`, under `tags.lock`, writing
   `<tag>.pending`).
6. **`--worktree`**: `git worktree add <repo>/.seagents/worktrees/<tag> -b
   seagents/<tag>`, refusing an existing path or branch (no silent reuse of a
   stale worktree), with `.seagents/` added to the repo's local `info/exclude`
   (`registry::exclude_seagents`, the one helper; never a tracked file).
   **Inside the repo, not beside it**, because Claude's folder-trust dialog
   appears in any never-trusted directory under every mode, `bypassPermissions`
   included, *before* `SessionStart`, while a subdirectory of a trusted folder
   inherits the trust (both verified 2026-09-29). Opt-in: a child usually wants
   the parent's uncommitted tree, and a worktree has none of it and a cold
   build directory — the skill makes the seed prompt say so.
7. **Write the seed** to `<runtime dir>/seed-<tag>` (0600).
8. **Launch** through `launch::spawn_terminal(&Run { keep_shell: false, … })` —
   `restore`'s launcher, the one per-terminal recipe table — in the terminal
   class of the caller's own window (the focused one for a human;
   `SEAGENTS_TERMINAL` overrides). The inner command is
   `'<abs exe>' run --tag <tag> [--parent <p>] --seed-file <F> -- <agent> <permission argv> <--args>`.
   `keep_shell: false` is the one difference from `restore`: **a spawned window
   closes with its agent** (a fan-out would otherwise leave N dead shells);
   a restored one drops to a shell. The launcher also strips every bundled
   adapter's `session_env` from the terminal's environment *and* `unset`s them
   at the head of its `sh -c` line (a daemon-hosted window gets the daemon's
   environment): a Claude child inheriting its spawner's `CLAUDECODE` /
   `CLAUDE_CODE_*` never wrote its transcript outside tmux (bisected in Phase
   2), and the messaging socket/token are that session's alone. Never
   `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or other user config.
9. **Return** once the child's `.meta` exists (poll every 100 ms, ≤ 15 s;
   normally ~0.1 s). `run` registers **before** it looks for its window: the
   handle poll can take 5 s (all 5 with no i3), which used to outlast `spawn`'s
   wait and report failure for a running agent; the window ids land in `.meta`
   a moment later. `spawn` does not wait for the agent to start: a
   never-trusted `--cwd` sits at the trust dialog before any hook fires, which
   the detector calls `blocked` (`live_blocked_form`, verified) and `wait`
   reports — a human answers it.

### Seeding lives in `run`

`run --seed-file F` reads F, **deletes it**, and appends the adapter's
`seed_prompt` (`-- <text>`, submitted as the first turn by both Claude and
Codex, verified) to the *child's* argv only — never to `.meta`'s. Under
`seed_by = "file"` (both bundled adapters) `<text>` is not the prompt: the
prompt goes to the session's prompt file (`<cwd>/.seagents/prompts/<pid>-0-<hex>.md`,
0600, deleted with the session — the long-`send` mechanism) and `<text>` is
the one line pointing at it. `/proc/<agent>/cmdline` is readable by every
local user, so the prompt must not be there (the first build put it there;
found in review 2026-09-30). Verified live: Claude in default mode and Codex in
its read-only sandbox read the file without a prompt and carry it out.

### Permission mode: a child never outranks its parent

**On this machine the tool's own default is `bypassPermissions`** (`defaultMode`
in both Claude config dirs, verified 2026-09-28) — so "don't pass a flag" means
*bypass*. The rule is therefore stated in explicit modes, ranked by the
adapter's `permission_modes` (least privileged first):

- **Agent caller:** always an explicit mode. Default = the parent's current
  mode; an explicit mode **above** it is refused; parent mode unknown (no hook
  yet, `--bare`) → the least privileged. Never the tool default.
- **Human caller:** passed only if given; otherwise the tool's configured
  default applies, the same as typing `claude` (decided 2026-09-29). The
  escalation the rule exists for is an *agent* granting a child more than it
  has.

The mode becomes argv through `[launch.permission_argv]`: Claude
`--permission-mode <m>`; Codex, which has only `-a` (0.152: `on-request` |
`never`) and `-s` (`read-only` | `workspace-write` | `danger-full-access`),
maps each mode to the nearest pair that is **not more privileged**: `plan` and
`default` → `-a on-request -s read-only` (every write is an approval request —
verified: the child stopped at Codex's run-this-command gate, `wait` rc 3),
`acceptEdits` → `-a on-request -s workspace-write`, `bypassPermissions` →
`-a never -s danger-full-access`. Measured for every pair (2026-09-29): Codex's
hooks report `permission_mode` from `-a` alone (`on-request` → `default`,
`never` → `bypassPermissions`, whatever `-s`), so nothing short of bypass may
use `-a never` — its child would read as bypass to its own children.

**`[launch.argv_mode]` (Codex).** Since Codex's hooks report the approval policy
alone, a Codex a human started as `-a never -s read-only` reports
`bypassPermissions` but can write nothing. `Launch::bound_mode` also reads the
parent's `.meta` argv: `permission_argv` in reverse (a flag value ranks as the
most privileged mode that uses it: `-s read-only` → `default`, `-s
workspace-write` → `acceptEdits`, `-a never` → bypass; an unknown value → the
least), plus the long spellings (`aliases`), `-c approval_policy=` /
`sandbox_mode=` (`config_keys`), and whole-mode flags (`--yolo`,
`--dangerously-bypass-approvals-and-sandbox`, `--full-auto`). The parent's mode
is the **lesser** of that and its hooks' word. A mode changed mid-session
(`/permissions`) or set by a `-p` profile is not on the command line; the lesser
still holds. Claude has no `argv_mode`: its hooks report the mode whole and
current (Shift+Tab changes it after launch, so argv would be stale).

**`[launch.privileged]` — an agent's `--arg` may not set permissions.** The cap
would mean nothing if `--arg --dangerously-skip-permissions` (or `--settings
'{"permissions":…}'`, or Codex's `-c approval_policy=never`) got past it. Each
adapter lists, as data with a one-line reason per entry: `flags` refused in any
spelling (`--flag`, `--flag=v`, a one-letter flag glued to its value or inside
a cluster like `-cp`); `commands` — subcommands that change the user's config
or act on other sessions (`claude mcp`, `codex exec`, …) — as bare words; and
`config_keys` behind the adapter's `config_flags` (Codex's `-c` keys and
anything under them: `approval_policy`, `sandbox_mode`, `hooks`, `notify`,
`mcp_servers`, `projects`, …). Matching errs toward refusing (a refused word is
refused even as another flag's value). Refused before anything is claimed,
naming the argument and pointing at `--permission-mode`. A human's `--arg`s are
not checked: they could type the same command into a terminal.

---

## `seagents wait`

### The race a screen-based wait has

"Poll `.state` until idle or blocked" returns **immediately** in the two cases
that matter most — right after `send` (the detector ticks every 2 s and hasn't
seen it start) and right after `spawn` (the agent paints an idle input box
before it takes the seed) — and a turn shorter than one tick is never observed
as `working`, so a "working→idle edge" rule hangs.

### Turn-based wait

`wait` blocks on the **turn ledger**, polling `.agent`, `.state` and `/proc`
every 250 ms. The caller holds a baseline N — 0 from `spawn`, `stops` from
`send`. `wait::decide()` is the whole reason logic, a pure function over one
look, tested row by row; order matters:

| reason | exit | when |
|---|---|---|
| `turn` | 0 | `stops > N`: the turn after N ended; `last_message` is its final text. Wins even if the agent has since exited or hit a gate |
| `exit` | 4 | `/proc/<child_pid>` is gone (not `.meta`, which only goes when someone reaps) — also for a reaped agent, from its tombstone, for a day |
| `blocked` | 3 | `.state` is `blocked` (a permission prompt, the trust dialog); `rule` names the gate |
| `no_hooks` | 6 | hooks injected, prompt given ≥ 20 s ago, no hook ever reported; `message` names the adapter and `hook.log` |
| `idle` | 0 | no `--after`, and none of the above: nothing to wait for, the last result at once |
| `interrupted` | 5 | a turn opened since the last `Stop` is no longer running (Esc, or dropped before its first response): no final message. Its own code so `wait && next` does not treat an abandoned turn as finished |
| `timeout` | 2 | `--timeout` (default 5m; `30s`/`5m`/`1h`/seconds) passed |

- **Without `--after`**: a running turn is waited on (N = current `stops`);
  with no turn running, the agent's condition is the answer at once by the
  same table — `blocked` at a gate (the trust dialog comes before any prompt,
  so no turn is open there; this used to return `idle`, exit 0, and a caller
  chaining on success went on as if the child were ready), `exit` when gone,
  else `idle`.
- **N ahead of the ledger** is an error (1), not a wait that can never end: a
  restored agent's ledger restarts at 0, so a restored parent waits without
  `--after` first.
- `--for exit` waits for exit only.
- The reported `state` follows the ledger for hook agents (`.state` lags a
  turn's end by up to one monitor tick); `null` for an exited agent.

`last_message` comes straight from the `Stop` hook, so the core
spawn → wait → result loop works **without any transcript parser** — which is
why `read` could wait until Phase 3.

**Timeout is the normal case, not the error case.** An agent's shell tool kills
long commands, so re-calling `wait --after N` with the same N is always correct
— nothing is consumed, the baseline is in the caller's hands. The skill teaches
that loop with `--timeout` below the shell tool's limit.

`wait` only reads: it never acknowledges `done` (a human still sees 🔵 after an
agent has waited on it) and never writes `.state`. The design's `--for done` was
dropped: `done` is cleared by a human focusing the window, so a waiter on it
could hang forever.

### The gate verdict has one source

`.state`, written by the wrapper's monitor (the one process that sees every byte
the agent prints), is what `wait`, `send`'s gate and `stop` read. Its 2 s tick
would leave `blocked` standing after a gate is answered, so input into a gated
agent hurries it (`wrapper/monitor.rs::Wake`): the control thread and the input
pump nudge the monitor, which then looks every 100 ms for a second (the agent
repaints once it has read the key, not when it is written), and `send
--answer-permission` replies only once the monitor has seen the gate go
(≤ 1 s). A `wait` issued the instant that returns gets the turn, not the
answered gate (tested). Having `wait`/`send` render `.raw` themselves was
rejected: a detector per waiting process (a 1 MB vt100 render per poll while a
turn streams) that would still race the repaint.

---

## `seagents send` — drive B

The only capability that needs the PTY master. The wrapper owns it; `send`
reaches it over a per-session unix socket.

### Control channel

- `<runtime dir>/<wrapper_pid>.sock` (see *Registry*), bound once the `.meta`
  exists; a control thread beside the output/input/resize/monitor threads
  (`wrapper/control.rs`).
- One JSON request line in, one reply line out (`send.rs` owns the wire format):
  `{"op":"send","text","mode":"paste|keys","submit","now","answer_permission","timeout_ms"}`,
  `{"op":"stop","force"}`. Any client speaking it is treated like the CLI; the
  HUD's `p`/`x` use the same client (`send::ask`).
- **Every request is authorized in the wrapper** from `SO_PEERCRED` → ancestry
  → caller tag → lineage rule; the peer's uid must be ours. Refusal:
  `forbidden`, rc 5.
- A stale socket (`ECONNREFUSED`) is a clear error; see *Socket sweep*.
- `Control::finish()` lets in-flight requests reply (≤ 2 s) before the socket
  is removed at exit.

### Default: submit when idle

Queue-while-working (herdr's default) suits a human who reads the screen; it
breaks **turn pairing** for an agent — a prompt queued behind a running turn is
folded into it or answered after it, and "the next ended turn" answers neither.

So a `send` is **held** (up to `--timeout`, 5 min) until the agent is out of its
turn (`in_turn`, the one definition) **and** `.state` reads idle; then it
submits and replies with the ledger's `stops` at that instant, `paired: true`:
`wait --after <stops>` returns the turn it started. Measured 2026-09-29: a
second `send` issued while the first's turn ran was held ~4 s and each was
answered by its own turn.

- **In flight until the hook confirms.** Submissions are serialized in the
  wrapper, and one counts as in flight until the agent's `UserPromptSubmit`
  hook fires for it (or 5 s pass), so two back-to-back sends can't both see
  "idle" and fold into one turn. **The reply itself waits** for that hook
  (≤ 5 s): otherwise, after an Esc'd turn, the ledger still read "prompted, no
  longer running" and a `wait --after <stops>` issued the instant `send`
  returned said `interrupted` for the *old* turn (found live on Codex).
- `--now` writes at once (`paired: false`: a prompt sent mid-turn is answered
  by its own later `Stop`, verified, but `wait --after N` returns the running
  turn's).
- **Blocked is refused at once** — `blocked`, rc 3, with the gate's `rule` —
  even with `--now`, since whatever is typed into a permission menu answers it.
- An agent no detector knows (a plain shell) can't be seen idle: only `--now`
  sends to it (`unsupported`, rc 1, otherwise).
- The target exiting while held: `exit`, rc 4. Still busy at `--timeout`:
  `busy_timeout`, rc 2, nothing sent.

### The input box

A paste lands after whatever the agent's input box already holds: a prompt
Claude put back after a turn dropped before its first response, or a human's
half-typed draft (verified: the two arrived as one prompt, `…the seaReply with
exactly: …`). So before any paste (not `--keys`, not into a gate) the wrapper
looks (`control::make_room`): `detect::input_box` renders the tee at the live
size and takes the last row starting with the adapter's `input_glyph` (Claude
`❯`, Codex `›`) and the rows under it, up to a blank row or a rule. The empty
box's placeholder (`Try "…"`, `Ask Codex to do anything`) is drawn dim, which
vt100 0.15 does not record, so the **cursor** tells it apart: both tools park it
at the text's start in an empty box (measured), and a box the cursor is not in
counts as "nothing to go by" (send as before) — a placeholder never reads as a
draft.

- Text that is, whitespace aside, **the last prompt this wrapper submitted** (a
  `send`'s paste, or the seed) is ours: the adapter's `input_clear` (Ctrl+U for
  both: to the line's start, then the newline before it — never Ctrl+C, which
  on an empty box starts exiting) is pressed one at a time, re-looking after
  each repaint, until the box reads empty.
- **Anything else is a draft: refused**, `draft`, rc 6, `--now` or not — the
  human is typing. The reply gives its length, never its words.

Verified 2026-09-29 on Claude 2.1.284: a dropped one-line, two-line and
by-reference prompt were each cleared and the next send arrived alone; a typed
draft was refused (also with `--now`). Codex 0.152.0 does not put an Esc'd
prompt back, but its composer takes a draft, refused the same way.

### Sharing the master writer

`wrapper/inject.rs::SharedWriter` is the PTY master's input side, shared by the
stdin pump (locking per chunk) and the control thread (locking for the whole
**body**), so a keystroke burst cannot interleave into an injected prompt. The
submit is a second, delayed write, so a submission is body → release → delay →
re-lock → Enter. Human keystrokes still flow between the two — the human stays
in charge of their own window — accepted; the transcript shows what arrived.

### Injection framing — from herdr

herdr @ `4b5e9bd` (`src/app/api/agents.rs`, `src/app/api_helpers.rs`,
`src/pty/actor/unix.rs`), in `inject::deliver`:

1. **Bracketed paste** (`ESC[200~…ESC[201~`, the end marker stripped from the
   body) when the child enabled DECSET 2004 — tracked by `ModeScanner` watching
   (never altering) the output stream — raw otherwise. No typed path.
2. **Enter is a separate write after `submit_delay_ms`** (300 ms) so the TUI
   consumes the paste first.
3. **Enter is encoded** for the child's keyboard protocol: `\r`, or `CSI 13 u`
   when it has pushed kitty keyboard flags that report every key (flag 8; stacks
   tracked per screen). modifyOtherKeys never changes an unmodified Enter, so it
   isn't tracked.
4. **Per-adapter `pre_send`** (`focus_gained` → `ESC[I`) first.

`--keys` writes bytes verbatim after decoding `\e \r \n \t \0 \\ \xNN` (in the
wrapper, so a raw client gets the same syntax); `--no-submit` skips the Enter.
Verified on Claude 2.1.284: a multi-line body with both quote kinds, backticks,
`$` and backslashes arrives exactly. Prompt text is never logged.

### Long prompts go by reference (measured 2026-09-29)

Claude files a large paste as a `<pasted_content id=…>` block, which current
models treat as **data that may carry instructions the user did not write** —
haiku declined to act on one. Measured (2.1.284, bracketed paste + 300 ms +
Enter): one line of 700 chars and 2 short lines land inline; one line of 900
chars, or 7 short lines, are wrapped. A raw unbracketed burst is classified the
same way, so no framing avoids it.

So past the adapter's limits (`long_prompt = "file"`, `inline_max_chars = 700`,
`inline_max_lines = 2`) the wrapper writes the body to
`<agent cwd>/.seagents/prompts/<wrapper_pid>-<stops>-<4 hex>.md` (0600,
`registry::write_prompt`; `.seagents/` into `info/exclude` first) and pastes one
line: *Read the file .seagents/prompts/<name>.md (relative to your working
directory) and carry out the instructions in it. They were sent to you through
seagents by <@caller | the user>.* The instruction is the user's own message; the
file is what it points at. The agent's **cwd**, because a Read there needs no
permission in any mode (verified in `default`, where the runtime dir would stop
at a prompt). The files are the session's: deleted by the wrapper when its agent
exits and by `reap` for one that died harder (only that wrapper's pid prefix).
Codex takes every paste as the user's words (4,392 chars / 31 lines, acted on),
so its adapter is `long_prompt = "paste"`. Seeds need none of this.

### Permission gates are a human's to answer

A gate is a blocked state, so a plain `send` never reaches one.
`--answer-permission` is the only way past the refusal, **refused for agent
callers** — only a human can answer a child's gate. Usually with `--keys`
(`--keys 1 --no-submit` picks "Yes" on Claude's menu, verified). An agent can
defeat this by escaping its tree; per the threat model, that is deliberate
misuse. With `bypassPermissions` as this user's default, gates are rare; the
permission-mode rule matters more.

### `seagents stop`

- Bypasses the blocked refusal — a stuck agent is the one you most want to
  stop. The lineage rule applies.
- Types the adapter's `exit_sequence` (`/exit`, ~1 s for Claude, instant for
  Codex) as a `--now` paste and gives it 5 s; then SIGTERMs the PTY's
  foreground process group (`tcgetpgrp` on the master) and continues it; then
  SIGKILLs it and the anchor, 5 s apart; replies once the agent is gone.
- **Skips the exit sequence** with `--force`; for a **blocked** agent (`/exit`
  + Enter typed into a permission menu would pick an answer — verified: the
  gated command never ran; on Codex's trust dialog it *trusted the folder*
  before `patches.rs` taught the detector that dialog); and when the input box
  holds a **draft** (the sequence would be submitted after it as a prompt). A
  prompt of ours put back in the box is cleared first, as for `send`.
- A spawned agent's window closes with it; a hand-started/restored one drops
  back to its shell. Its prompt files and socket go with it.

---

## `seagents read` — inspect B's conversation

Source: the path the hooks reported in `.agent` (or the tombstone, once
reaped), or any file with `--transcript PATH`, its format told from its first
lines (a Codex rollout wraps every line in `{timestamp, type, payload}`, a
Claude transcript never does). The adapter's `[read] transcript` picks the
parser. Everything is parsed into the **common record model**
(`transcript/model.rs`):

```
user        { text }                         assistant { text }     thinking { text }
tool_use    { id, name, input[, subagent] }  tool_result { tool_use_id, text, bytes[, is_error] }
```

each with `uuid`, `parent_uuid`, `timestamp`. One JSONL line can hold several
blocks and yields several records sharing its uuid; the tree, the main line and
the cursor are made of lines. `transcript/` is the shared layer SESSIONS-SPEC's
browser will read through (index, main line, renderer).

- **Main line, not raw log**: root → newest **tip**, where a tip is the newest
  *conversation* node (user/assistant) with no conversation below it — not the
  newest leaf of any kind. `system`, `attachment` and `progress` lines are tree
  nodes too, many are side leaves, and 94 measured here were newer than the
  sibling the conversation went on through: ending on one would make a cursor
  read as a rewind a moment later.
- **Compaction bridging**: a compaction boundary takes its `logicalParentUuid`
  as parent; when that line was never written (5 of 15 here) the walk bridges
  to the newest tip before the boundary, so the line still starts at the real
  beginning.
- **Codex rollouts** are logs, not trees: nodes are lines (uuid = byte offset
  + 1), chained; `thread_rolled_back {num_turns}` hangs off the node before the
  turns it drops, making them a side branch the main line skips. A "user"
  message is the user's only when its `content_item_kinds` say `user.*` (older
  rollouts: not one of Codex's context tags).
- **Collapse rules** (`render.rs`, SESSIONS-SPEC's): user text behind `│`,
  assistant text in full, `⋯ thinking (N words)`, `● Tool(short arg)`,
  `  ⎿ first line (N bytes)`; `--raw` shows all. A real subagent is one line,
  `▸ subagent: <type> — <description>` (from its `.meta.json` sidecar, else the
  call's input); `agent-a<word>-…` helper transcripts are never subagents;
  `isMeta` user lines are not shown. A user line that only points at a
  by-reference prompt gets the file's words under it while the file exists
  (text mode).
- **Cursor**: every `read` ends with
  `{"cursor":{"transcript_path","offset","leaf","token"}}` (text mode: a last
  line `-- cursor: <offset>:<leaf>:<path>`; `--from` takes that line, the
  token, or the JSON). `leaf` is the main line's tip, because a byte offset alone
  would present a post-rewind branch as a continuation. On `--from`: if the
  main line still runs through `leaf`, only what follows; if not,
  `{"rewound":{"fork"}}` and reading resumes after the fork; a different path
  (`/clear`) or a file shorter than `offset` → `{"reset":{"transcript_path"}}`
  and read from the start.
- **The 1 s post-`Stop` wait.** Claude writes its transcript *behind* its hooks
  (measured 2.1.284: a read the moment `wait` returned saw the prompt but not
  the answer, ~60 ms late). A target read after a finished turn waits (≤ 1 s)
  until a line stamped at or after the `Stop` is there — the turn's
  `stop_hook_summary`.
- **Offset index** (`index.rs`): one 53-byte entry per tree node (offset,
  length, kind, timestamp, uuid, parent), cached as `sessions/<id>.idx`
  (a subagent transcript: a hash of its path), extended from the last indexed
  byte on every open (only complete lines), rebuilt when the file shrank or its
  first 4 KiB hash differently. Format **v2** records the transcript's
  canonical path; a v1 index is swept and rebuilt. Measured on a 53 MB
  transcript: `--tail 20` 65 ms cold, 6 ms warm; the whole main line as NDJSON
  90 ms. **Daily sweep**: `index::cache_path()` sweeps at most once a day (the
  mtime of `sessions/.swept`, renewed first so concurrent readers don't all
  do it): an index whose transcript is gone, or unread for 30 days (a read
  touches its mtime), goes, as do day-old `*.tmp` leftovers. Verified
  2026-09-29.

`read` is the full record; `wait`'s `last_message` is the summary. Exit 1 with
the reason on stderr when there is nothing to read.

---

## HUD (Phase 5)

The HUD is a front-end on the same primitives an agent drives:

- **Lineage tree**: every spawned agent directly under its parent, its name
  behind `└ ` (two more columns per generation), the marker in the name column
  so dot/state/time columns stay aligned. The sort decides one level at a time,
  but a family is placed where its **highest-sorted member** would be — under
  urgency a child blocked on a prompt lifts its whole family to the top. An
  orphan is a top-level row; a parent cycle (only a hand-edited `.meta`) costs
  no row.
- **Tags as names**: `@tag` when somebody chose it, the directory when `run`
  minted it (`registry::minted_from`), since a spawned fleet usually shares one
  directory and would read as N identical rows.
- **`p`** sends the selected agent a prompt (a one-line footer input; Enter
  sends, Esc cancels; bracketed paste on, so a pasted multi-line prompt arrives
  whole). **`x`** stops it after a y/n. Both go through `send::ask` on a
  background thread (a send is held until idle, a stop takes its grace
  periods); the outcome lands in the footer (`sent to @w · stops 3`, `stopped
  @w`, or the wrapper's refusal). The target is fixed when the input opens,
  since rows move under the urgency sort.
- **No answer-the-gate key**, deliberately: what `1` picks differs per gate,
  and approving one unseen from a list row is exactly what the blocked refusal
  exists to prevent. `p` on a blocked row says so at once; Enter jumps to the
  window, where the gate is answered.

The HUD is a human caller, so the lineage rule never refuses it; the blocked
refusal still applies. Verified live 2026-09-29 (tmux, scratch HOME).

---

## Relationship to `restore`

- **One launcher** (`launch::spawn_terminal`), one window-identity path
  (`run --tag` → `handle_for(tag)`), one terminal-recipe table, one
  `session_env` strip.
- **`layout.json` records `tag`, `parent`, `depth`** per agent; `restore` passes
  `--tag` (and `--parent`) to `run`. **Tag-based dedup**: a captured agent whose
  tag is live is already on screen and is skipped — tag identity alone makes
  restore idempotent (the session-id match and launch-signature counting were
  deleted). **Lineage restore**: a child gets `--parent` when its parent is
  live or relaunched earlier in the same run (each workspace launches parents
  first and waits up to 10 s for a parent's `.meta`); a parent that is gone, or
  restores on a later workspace, drops the lineage rather than failing.
- **Resume per adapter** (`resume_argv`, `[restore]`): the stale selection words
  are dropped (Claude `--resume`/`-r`/`--continue`/`-c`; Codex
  `resume`/`fork`/`--last`, each with its id) and the resume argv inserted right
  after the command (Claude `--resume <id>`, Codex's `resume <id>` subcommand) —
  so before any `--`, after which it would be read as prompt text. An adapter
  with no `resume` restores fresh. The id comes from `.agent`, kept current by
  `SessionStart` across `/clear`.
- **`argv` excludes seed and injected hooks** (above), so relaunching stays
  exact.
- **Boot set-aside.** Capture is continuous (every monitor, HUD poll and
  `status`, throttled to one write per 30 s), so after a reboot the first
  capture would overwrite the old boot's layout with an empty one. The write
  (`write_layout`, under `layout.lock`) compares the file's `captured` with this
  boot's `btime`: a previous boot's non-empty layout is moved to
  **`layout.prev-boot.json`** first. `restore` reads that when present, else
  `layout.json`; a full restore from it renames it to `layout.restored.json`
  (`--dry-run`/`--workspace` leave it pending). Its tags stay reserved against
  minting until then.
- A restored agent's ledger starts again at `stops: 0`; a restored parent must
  re-`wait` without `--after` (the skill says so).

---

## Phase 0 — groundwork refactor (done)

The features landed on a codebase whose seams were in the wrong places: the
concerns orchestration needs — the registry, `/proc` ancestry, i3 access, the
terminal launcher, the wrapper's threads — were each implemented two to four
times, and `main.rs` was both a hand-rolled argv parser and the whole PTY
wrapper. Phase 0 reshaped first, with no feature change except the lock +
atomic write (a fix), in five commits: `i3.rs`/`procfs.rs`/`clock.rs` (leaf
modules; where copies differed the more careful one won) → `registry.rs` (typed
records, locked update; four parsers and three writers deleted) → `launch.rs`
out of `snapshot.rs` (`focus.rs` became `window.rs`) → `wrapper/` out of
`main.rs` → `clap`.

Module map as built (all phases):

```
src/
  main.rs          clap-derived CLI + dispatch only
  registry.rs      THE cache: Dirs/Files, typed Meta/StateRecord/AgentRecord,
                   list/reap/update_*, tags + reservations + tombstones,
                   runtime_dir, resolve_target/resolve_exited, prompt files
  procfs.rs  clock.rs  i3.rs
  window.rs        window identity + focus/preview
  launch.rs        terminal recipes, handle_for, spawn_terminal (restore + spawn)
  adapter.rs + adapters/*.toml     per-agent facts
  hook.rs          inject() (the one merged hook flag), __hook, health()
  turn.rs          Ledger::in_turn — the one definition of mid-turn
  transcript/      model, claude.rs, codex.rs, index.rs, mainline.rs, render.rs
  spawn.rs  wait.rs  send.rs (client + wire format)  read.rs
  wrapper/         mod.rs, pumps.rs, monitor.rs (+ Wake), lead.rs,
                   control.rs (socket, authorize, gate, make_room, stop),
                   inject.rs (SharedWriter, ModeScanner, deliver, decode_keys)
  status.rs  snapshot.rs  hud.rs  detect.rs (+ input_box)  vendor/
```

Decisions kept from the design: `clap` because the primary caller learns the
interface from `--help` (one deliberate interface change:
`debug-screen <FILE> [COLS] [ROWS] [--agent A] [--json]`, no more guessing);
`#[serde(default)]` everywhere plus carried unknown fields, since live wrappers
outlast a reinstall.

---

## Cutovers

What was deleted, so there is one source of truth per concern at every commit:

- **`resolve_session_ids()` and helpers** (`/proc` scratch-fd scan, window-title
  ↔ `aiTitle` matching, `claude_project_dir()`, `session_id_source`,
  `snapshot --refresh`) → the `SessionStart` hook. This also fixed a live bug:
  `claude_project_dir()` hardcoded `~/.claude/projects`, wrong under
  `CLAUDE_CONFIG_DIR=~/.claude-work`. One-time cost: wrappers from a pre-hook
  binary restore without `--resume` until restarted (HANDOFF).
- **`tag_for()`**, the session-id match and `launch_signature` in snapshot → the
  `.meta` tag. One-time cost for pre-tag wrappers (HANDOFF).
- **`run --window <handle>`** → `run --tag`; the handle is `handle_for(tag)`.
- **`registry::in_turn`'s screen rule** for hook agents → `turn::Ledger`.
- **Hard-coded Claude `resume_argv`** (which also dropped Codex's `-c` as a stale
  `--continue`) → `[restore]`.
- **Adapter `settings_flag`, `[launch] hookless_flags`, `permission_flag`** →
  `[hooks]` and `[launch.permission_argv]`.
- **`mark_unseen`** (only the monitor could mark a turn done) → any observer.
- **The design's `ls`, `--role` launch path, `SEAGENTS_WRAPPER_PID`, report
  files** (`~/.cache/seagents/reports/<tag>.md`, which keyed by a reusable tag
  would have served a stale report) → never built.

---

## Safety (summary)

| risk | guard |
|---|---|
| child drives your main session | lineage rule, enforced in the wrapper from `SO_PEERCRED` ancestry |
| A↔B ping-pong | lineage rule (children cannot drive parents) |
| runaway fan-out | depth, breadth and global caps under one lock, reservations counted |
| child escalates privilege | explicit mode never above the parent's; Codex parent bounded by its argv too; agent `--arg`s checked against `[launch.privileged]` |
| agent answers a permission gate | blocked refusal (even `--now`); `--answer-permission` human-only; `stop` skips `/exit` at a gate; no HUD gate key |
| paste appended to a human's draft | input-box check: clear only our own prompt, refuse a draft |
| prompt leaks to disk or other users | seed file 0600 in the runtime dir, deleted by `run` at startup; the seed itself goes by file, never in the agent's argv (`/proc/*/cmdline`) or `.meta`; prompt files 0600, deleted with the session; the cache and state dirs 0700 with 0600 files (`.raw`, `.agent`, tombstones hold conversation text); prompt text never logged |
| caller forges its identity | unreadable/vanished peer ancestry refused, not taken for a human; caps from the user's config, not the caller's env |
| hook breaks the agent | `__hook` always exits 0, silent stdout |
| hooks silently absent | `no_hooks` / `hooks_ok` |
| user's settings clobbered | one merged `--settings` / `-c hooks`; never write user config (`~/.claude`, `~/.codex`) |
| another user's `/tmp` dir | fallback runtime dir used only if a real dir owned by us |
| a stray seagents deletes live sockets | sweep only when the pid is dead or not seagents |

`.raw` holds screen content and `.agent` holds `last_message`; both live in the
user's cache dir, as the tee always has. Nothing new crosses a user boundary.

---

## Relationship to the skill

`skills/seagents-orchestrate/SKILL.md` owns the workflow; this spec owns the
primitives. It states flags, JSON shapes and exit codes from `--help` as built,
and was run end to end against a live haiku agent. Conventions shared:

- When to spawn (independent, substantial, ideally parallel) versus doing it
  yourself; a child starts with none of your context.
- The seed prompt tells the child its tag, its parent's tag, what done means,
  and that **its final message is its report**; with `--worktree`, that it has
  none of the parent's uncommitted changes and a cold build directory.
- The loop: `spawn` → `wait --after 0` (re-call on timeout, same N) → act on
  `last_message` → `send` → `wait --after <stops from send>` → `read` when the
  summary is not enough → `stop`.
- A table of what to do per `wait` reason: on `blocked`, tell the human which
  child and which gate — never answer it; on `no_hooks`, tell the human.
- `send` rc 6 (`draft`): the human is typing; tell them or retry later.
- Never pass `--answer-permission` or `--now` without a stated reason.

---

## Phasing

| phase | scope | state |
|---|---|---|
| 0 | Groundwork refactor: `registry`, `procfs`, `i3`, `clock`, `launch`, `wrapper/`, clap | **done** |
| 1 | Tags for every agent; `run --tag/--parent/--seed-file`; `.agent` + `__hook` + merged `--settings`; `spawn` via `spawn_terminal`; lineage + caps; `wait` on the ledger; `status` fields; `focus <tag>`; layout carries tag/parent/depth | **done** |
| 2 | `send` / `stop`: socket, `SO_PEERCRED` authorization, shared writer, paste framing, submit-when-idle, blocked refusal, long prompts by reference, session-env strip | **done** |
| 3 | `read`: record model, Claude parser, index, main line, cursor with `rewound`/`reset`, collapsed text | **done** |
| 4 | Codex adapter: hooks with trust hashes via one `-c hooks={…}`, rollout parser, `[restore]`, `-a`/`-s` permission argv + `argv_mode`, trust-dialog detector patch | **done** (codex-cli 0.152.0) |
| 5 | HUD: lineage tree, tags as names, `p`/`x` | **done** |
| — | Polish (after Phase 5): tombstones, plain wait on a gate → `blocked`, `no_hooks`, dropped-prompt rule, input-box check, register-before-window, `/tmp` runtime fallback, socket sweep rule, privileged `--arg`, Codex `argv_mode`, index sweep, skill | **done** |
| 6 | `watch` identity; screen-turn ledger | **not built** |

**Not built, and why.** The design's hookless path — `watch` identity (inotify
on a session directory, first file created after launch) and the screen-turn
ledger (the wrapper monitor stamping `prompted_at` on a control-thread submit
and `stops += 1` at the next idle-with-output) — exists only as schema:
`turns = "screen"` parses and `in_turn` has a screen branch (`identity =
"watch"`/`watch_dir` were removed from the schema in review — an option that
parses and does nothing is worse than an error), but **nothing learns a watched path and nothing
writes a ledger for a hookless agent**. No bundled adapter needs either: both
Claude Code and Codex have hooks, and the Codex design this was meant for
(`notify` + `watch`/screen) was superseded when Codex 0.152 turned out to have
Claude's hooks. So a hookless agent today gets state, `send --now` (or `send`
when a detector knows it), `stop`, and `wait` for `blocked`/`exit` only — its
`stops` never moves and it reports no `turns` capability. Likewise the
**paste-once-idle seed** for an adapter with no `seed_prompt` or a > 128 KiB
prompt is refused rather than built; when needed it pastes through
`inject::deliver`, as `send` does. Build them when a tool that needs them is
adopted, not before.

---

## Verified

Verified 2026-09-28 (Claude Code 2.1.283):

- `SessionStart` / `UserPromptSubmit` / `Stop` payloads as shown; `Stop` carries
  `last_assistant_message`; `UserPromptSubmit` carries `prompt` and
  `permission_mode`.
- Repeated `--settings` flags replace each other; settings-file hooks and
  `--settings` hooks coexist.
- Transcripts live under `$CLAUDE_CONFIG_DIR/projects`, not `~/.claude/projects`.
- This user's `defaultMode` is `bypassPermissions`.

Verified 2026-09-29 (Claude Code 2.1.283–284, live in tmux and the nested
desktop):

- The positional seed after `--` is submitted in interactive mode, inline at any
  size tested (50 lines, ~5000 chars).
- Bracketed paste + 300 ms + Enter submits the full body; ≤ 700 chars / ≤ 2
  lines inline, 900+ chars or 7+ lines wrapped as `<pasted_content>`.
- An Esc-interrupted turn fires `UserPromptSubmit` but no `Stop`, and writes an
  interrupt record; an Esc before the first response writes nothing and puts
  the prompt back in the input box.
- A message sent mid-turn reuses the running `prompt_id` and is answered by a
  later `Stop` with a fresh one.
- The folder-trust dialog appears in never-trusted dirs under every mode incl.
  bypass, before `SessionStart`; the detector calls it `blocked`
  (`live_blocked_form`); subdirectories of a trusted dir inherit trust.
- Claude writes its transcript ~60 ms behind the `Stop` hook.
- Inherited `CLAUDECODE`/`CLAUDE_CODE_*` stop a Claude outside tmux writing its
  transcript.
- A Read inside the agent's cwd needs no permission in `default`.

Verified 2026-09-29 (codex-cli 0.152.0, scratch `CODEX_HOME`):

- Hooks: Claude's events and payloads; `SessionStart` at the first prompt;
  trust via `hooks.state…trusted_hash` honoured from a `-c` override; our hash
  equals the one Codex writes.
- Seed `codex -- <prompt>`; `/exit`; a 4,392-char paste submitted whole.
- Esc fires `Interrupt`, no `Stop`, and writes `turn_aborted`, even before the
  first token; no prompt is put back.
- The hooks' `permission_mode` follows `-a` alone, for every `-a`/`-s` pair;
  `-a on-request -s read-only` gates every write.
- `codex resume <id>` takes every TUI option; a `-c` before it applies.
- The trust dialog drawn over an update notice is missed upstream (patched).

## Open questions and known gaps

No open design questions. Known gaps, accepted:

- A one-line draft whose cursor was moved to its start reads as an empty box
  (the placeholder rule), so `send` would paste after it.
- The dropped-prompt rule takes ~10 s to call a dropped turn `interrupted`.
- A Codex parent's mode set by a `-p` profile or changed via `/permissions` is
  bounded only by its hooks' word (the lesser still holds).
- Hookless agents have no turn ledger (*Phasing*).

## Relationship to existing docs

- **HANDOFF.md** is the operational reference and carries all of this as built
  (§2 data model, §6 hooks and ledger, §7–§11 spawn/wait/send/read/Codex).
- **SESSIONS-SPEC.md** owns the transcript renderer's rules; this work built the
  shared `transcript/` layer (record model, parsers, index, main line, render)
  the browser should read through. SESSIONS-SPEC still names
  `~/.claude/projects` and should use `$CLAUDE_CONFIG_DIR` (and learn Codex
  rollouts) when it is built.

---

## Revision log

**2026-09-29 — as built.** The spec now describes the code at `c3d3ae8`. What
changed from the 2026-09-28 design, and why:

- **Codex is a hook adapter, not `notify` + `watch`/screen.** Codex 0.152 has
  Claude's hooks; the only new problem was trust, solved by carrying our hooks'
  trust hashes in the same `-c hooks={…}`. Adapter schema grew `[hooks]`
  (`inject = settings|config`), `[restore]`, `[read] transcript`,
  `[launch.permission_argv]` (Codex has no mode flag), `[launch.argv_mode]`
  (its hooks report the approval policy alone), `[launch.privileged]` (an
  agent's `--arg` could bypass the mode cap), `session_env` (inherited session
  variables broke the child's transcript), and `[send]` long-prompt / input-box
  keys. `watch` and the screen ledger were not built: nothing needs them.
- **The ledger's in-turn rule** is transcript-based, not screen-based (the
  screen says idle mid-answer), plus the dropped-prompt rule (formerly a
  "known gap"); ms timestamps.
- **`wait`** gained `no_hooks` (rc 6), answers a plain wait on a gate as
  `blocked`, and answers for exited agents via tombstones.
- **`send`** replies only once the prompt hook confirms (else an immediate
  `wait` saw the previous turn), sends long bodies by reference (Claude treats
  big pastes as untrusted data), checks the input box (a dropped prompt or a
  draft got glued to the paste), and wakes the monitor on answered gates.
- **`spawn`** registers before window search and waits 15 s (a headless spawn
  reported failure for a running agent); caps count `.pending` reservations;
  privileged `--arg`s refused for agents.
- **Registry** gained the `/tmp/seagents-<uid>` fallback (long runtime dirs
  bound no socket) and a socket sweep that no longer deletes other `HOME`s'
  live sockets; `.state` gained `rule` and `screen`; any observer marks `done`.
- **`read`**: the tip is the newest conversation node, not any leaf; compaction
  bridging; a ≤ 1 s wait for Claude's late transcript write; cursor `token`;
  format sniffing for `--transcript`; index v2 with a daily sweep.
- **Snapshot**: a previous boot's layout is set aside, not overwritten.
- **HUD**: lineage tree, tags, `p`/`x`; no gate key by design.

**2026-09-28 — adversarial revision.** Every change came from attacking the
previous draft against the code and against live hook payloads.

- **Bug — `wait` returned before the turn it was waiting for** (detector lag,
  idle-before-seed). → turn ledger from `UserPromptSubmit`/`Stop` hooks,
  `wait --after N`, `send`/`spawn` return the baseline.
- **Bug — seed prompt re-submitted on every restore** (it lived in `.meta` argv).
  → `run --seed-file`; recorded argv is the user's.
- **Bug — `.meta` had four unlocked, non-atomic writers across processes.** →
  Phase 0 `registry`: typed records, `flock` + tmp/rename; `.agent` split off
  for churn.
- **Structure — the concerns orchestration needs were duplicated 2–4×.** →
  Phase 0 refactor, clap CLI.
- **Bug — `--settings` injection would silently drop the user's `--settings`**
  (last flag wins). → single merged flag.
- **Bug — "tool's own default" permission mode is bypass on this machine.** →
  explicit mode, never above the parent's.
- **Bug — report files keyed by reusable tag served stale reports.** → dropped;
  `last_message` from `Stop`.
- **Bug — queue-while-working breaks turn pairing for agent callers.** →
  submit-when-idle default, `--now` opt-in.
- **Bug — byte-offset cursor mis-presents post-rewind records as continuation.**
  → cursor carries the leaf uuid; `rewound`/`reset` records.
- **Gap — any agent could drive any registry agent.** → lineage rule, enforced
  server-side via `SO_PEERCRED` ancestry; rate limit dropped as redundant.
- **Gap — `--answer-permission` guarded only by the skill asking nicely.** →
  refused for agent callers.
- **Gap — depth cap didn't bound breadth.** → breadth + global caps.
- **Gap — `stop` would have hit the blocked refusal.** → bypasses it.
- **Gap — `__hook` could break the agent.** → explicit contract.
- **Accretion — the draft duplicated `restore`'s launcher, `status` (as `ls`),
  `resolve_session_ids` and restore's tag namespace.** → one of each; *Cutovers*.
- **Reorder — `last_message` makes the core loop parser-free** → `read` to
  Phase 3, `send`/`stop` to Phase 2.
