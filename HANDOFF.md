# seagents — Handoff

Context document for picking this project up cold (e.g. a fresh Claude session
on another machine). Read this first; it replaces the in-session memory that
does not travel between machines.

---

## TL;DR — what seagents is

**seagents = "see agents".** A sidecar/observer for coding agents (Claude Code
today) that run in your *own* i3wm-managed terminal windows. It is **not** a
terminal multiplexer.

Contrast with [herdr](https://github.com/ogulcancelik/herdr) (a herd/agent
multiplexer we studied): herdr *owns* the PTYs — agents run inside herdr's panes.
seagents does the opposite: you keep running `claude` in your normal Terminator/i3
windows, and seagents watches them from the side. It shows a floating HUD of every
agent's status (blocked / done / working / idle) and, on click, focuses that agent's real
i3 window.

The core value: **at a glance, see which agent is blocked and waiting on you, and
jump straight to it.**

---

## Current status

All four milestones are **code-complete and on `main`**:

| capability | command | state |
|---|---|---|
| transparent launcher (pty-tee) | `seagents run <cmd>` | done, verified feels native |
| status detection | `seagents status` | done; blocked/working/idle verified live |
| focus the agent's i3 window | `seagents focus <child_pid>` | done; con_id fix **committed but not yet live-verified** |
| floating HUD | `seagents hud` | done; works, j/k preview + click-to-focus |
| reboot survival | `seagents snapshot` / `restore` | done; round-trip verified in a nested X (`tools/test-restore.sh`) |

Focus identity is exact-handle only (no title matching since 2026-07-14), so
the old two-idle-claudes ambiguity concern is structurally gone; con_id focus
and healing have been verified live.

---

## Getting set up on a new machine

`target/` and the `herdr` reference clone are gitignored, so after cloning you
build fresh:

```bash
git clone git@github.com:AnakTeka/seagents.git
cd seagents
cargo build --release
install -Dm755 target/release/seagents ~/.local/bin/seagents   # ensure ~/.local/bin is on PATH
```

Runtime deps on the machine: **i3wm**, **i3-msg** (ships with i3), a terminal
emulator. Everything else is vendored via Cargo. **`xdotool`** is optional but
worth having: it is what lets a one-process-per-window terminal identify an
agent's window exactly (§4); without it seagents falls back to the correlation
paths.

Auto-track every agent by aliasing `claude` (zsh):

```zsh
alias claude='seagents run claude'
```

Add the HUD keybind + float rule to your i3 config (`~/.config/i3/config`).
`$mod` is Super on the dev machine; `$mod+o` was free there — pick any free key:

```conf
# seagents — floating agent HUD
bindsym $mod+o exec --no-startup-id terminator --role=seagents-hud -e "seagents hud"
for_window [window_role="seagents-hud"] floating enable, sticky enable, border pixel 1, resize set 480 340, move position center
```

Under alacritty (which has no window roles) the same two lines are:

```conf
bindsym $mod+o exec --no-startup-id alacritty --class seagents-hud -e seagents hud
for_window [instance="^seagents-hud$"] floating enable, sticky enable, border pixel 1, resize set 480 340, move position center
```

A single `--class <name>` sets **both** halves of WM_CLASS (verified:
`--class sgprobe` → `class=sgprobe instance=sgprobe`), which is why no comma is
needed here — and that matters, because **a comma in an i3 `exec` line is i3's
own command separator**. `--class Alacritty,seagents-hud` unquoted makes i3 run
`exec ... alacritty --class Alacritty` (no `-e`, so you get a shell in a
HUD-shaped window) and then fail to parse `seagents-hud -e seagents hud`, which
raises the "configured command could not be run successfully" nagbar. If you do
want to keep `Alacritty` as the general class so Alacritty-wide rules still
apply, quote the whole argument:

```conf
bindsym $mod+o exec --no-startup-id "alacritty --class Alacritty,seagents-hud -e seagents hud"
```

Then reload i3 (`$mod+Shift+c` or `i3-msg reload`). Any other terminal works
too — keep a matchable `window_role` **or** WM_CLASS instance of
`seagents-hud`, which is what `focused_terminal_ids()` checks so the HUD never
records itself as an agent's window.

**Terminal choice matters more than it looks.** Terminator runs every window
from one process, which is why window identity needs correlation at all (see
§4). A terminal that gives each window its own process — alacritty, or kitty
launched as fresh instances — lets `seagents run` identify its own window
exactly, from `/proc` ancestry, with no focus race and no unidentifiable
windows.

---

## Commands

```
seagents [hud]              floating agent HUD (also the default with no args)
seagents run [--tag T] [--parent P] [--seed-file F] <cmd> [args]
                           launch an agent, teeing its output (this is how
                           tracking happens); every agent gets a tag (below)
seagents spawn [--cwd DIR] [--tag T] [--agent claude] [--permission-mode M]
               [--worktree] [--arg A]... (--prompt-file F | [--] PROMPT)
                           open a new terminal running an agent on a prompt;
                           prints {"tag","child_pid","wrapper_pid","cwd",
                           "parent","depth","stops"} (§7)
seagents wait <target> [--after N] [--for turn|exit] [--timeout DUR]
                           block until the turn after N ends, the agent is
                           blocked, interrupted or gone; prints the outcome
                           JSON incl. the turn's last_message (§8)
seagents send <target> [--now] [--keys] [--no-submit] [--answer-permission]
              [--timeout DUR] (--prompt-file F | TEXT)
                           submit TEXT as a new turn once the agent is idle;
                           prints {"ok","tag","stops","paired"} — `stops` is
                           the baseline for `wait --after` (§9)
seagents stop <target> [--force]
                           make the agent exit: its exit command, then
                           SIGTERM, then SIGKILL (§9)
seagents read (<target> | --transcript PATH) [--json] [--from CURSOR]
              [--tail N] [--raw]
                           the conversation's main line from the agent's
                           transcript, collapsed text or NDJSON records,
                           ending with a cursor to continue from (§10)
seagents status [--json] [--sort=urgency|dir|recent]
                           one-shot table of every tracked agent's state
                           (--json: machine-readable, for eww/widget frontends,
                            incl. each agent's conversation `title`, `tag`,
                            `parent`, `depth`, and from its hooks `stops`,
                            `in_turn`, `transcript_path`, `session_id`, and
                            `capabilities` derived from its adapter;
                            sorts: urgency = triage, longest-waiting first;
                            dir = grouped by cwd; recent = latest change first)
seagents focus <target>    focus that agent's i3 window (target: `tag`,
                           `@tag`, or the `pid` status --json reports)
seagents snapshot [--print]
                           capture the agent layout now (it is also captured
                           continuously)
seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]
                           rebuild that layout after a reboot, each window
                           resuming its own conversation
seagents update-manifests  fetch fresh detection rules from herdr.dev now
seagents debug-screen <FILE> [COLS] [ROWS] [--agent A] [--json]
                           dev tool: render a .raw + the engine's explain verdict
```

`seagents --help` and `seagents <command> --help` are generated from the clap
definition in `main.rs`, so they are the authoritative reference. Every flag
takes both `--flag value` and `--flag=value`. `run` passes everything after
the agent's first word through untouched (`seagents run claude --model opus
--help` hands `--help` to claude); `--tag`/`--parent` are only `run`'s own
when they come before the command. Usage errors exit 2. `debug-screen` used to guess which
argument was the file and which were the geometry (any number was a
dimension); since the move to clap it is strictly file, then cols, then rows.

HUD keys: `j`/`k` or arrows = navigate + **preview** (reveal the agent window,
keep focus on HUD); `g`/`G` = first/last row (vim-style, also previews);
`Enter`/left-click = **jump** (commit focus to the agent);
`s` = cycle sort (urgency → dir → recent; remembered across launches);
`t` = cycle the name columns — **both** (directory + title, the default) →
**title** only → **dir** only (remembered across launches);
`?` = the full key list, as an overlay (the footer hint is one line and a
floating window clips it); `r` = refresh now; `q`/`Esc` = quit.

While the `?` overlay is up it owns the keyboard: any key but `q` dismisses it
rather than acting, so a keystroke aimed at the overlay can't navigate the list
underneath. It draws over the agent list (no room for both in a floating
window), and if the window is too short for all nine bindings the bottom border
says `▼ n more` — otherwise `q / esc` would silently fall off the end.

---

## Architecture / how it works

Source layout under `src/`:

- **`src/main.rs`** — the command line only: the clap-derived `Cli`/`Command`
  and `dispatch()` to the module that owns each command, plus parse tests.
- **`src/wrapper/`** — `seagents run`, the pty-tee wrapper (§1):
  - `mod.rs` — setup and wiring: `run()` (window identity, pty + child,
    `.meta`, raw mode, thread start, the job-control wait loop),
    `identify_window()`, `terminal_size()`, `write_meta()`.
  - `pumps.rs` — the output pump (stdout + tee, with `.raw` rotation), the
    input pump (with window-identity healing, `input_capture_eligible()`), and
    the SIGWINCH resize pump.
  - `monitor.rs` — the self-monitor thread: detection tick, `.state`
    transitions, `notify_transition()`, the done-ack on focus, the snapshot
    offer; `Wake`, how input into a gated agent hurries it.
  - `lead.rs` — `lead()`, the hidden `__lead` job-control anchor.
  - `control.rs` — the control socket (§9): bind, `SO_PEERCRED`
    authorization (`authorize()`, the lineage rule), submit-when-idle
    (`gate()`), `stop`'s escalation; `Control::finish()` at exit.
  - `inject.rs` — the one implementation of writing into the agent
    (§9): `SharedWriter` (the PTY master's input side, shared by the input
    pump and the control thread), `ModeScanner`/`PtyModes` (bracketed paste
    and kitty keyboard flags, watched in the output stream), `deliver()`
    (pre-send nudge, bracketed paste, delayed Enter encoded for the
    keyboard protocol), `decode_keys()`.
- **`src/send.rs`** — `seagents send` / `stop`, the socket's client, and its
  wire format (`Request`, `Reply`, `Refusal` → exit codes).
- **`src/hook.rs`** — the agent's lifecycle hooks (§6): `inject_settings()`
  (the one merged `--settings` `run` hands a hook-capable agent) and `run()`,
  the hidden `__hook` command that records each event in `.agent`.
- **`src/registry.rs`** — the one owner of the session cache (§2): `cache_dir()`,
  sidecar paths (`Files`), the typed `Meta` / `StateRecord` / `AgentRecord`,
  `Dirs` (the cache and the runtime dir — sockets live in the latter),
  `list()` (live, never deletes) / `reap()` (deletes the dead, and sweeps
  control sockets no live session owns) /
  `by_child_pid()` / `enclosing_session(pid)` (the session a process runs
  inside, by `/proc` ancestry — "who is calling"), the locked `update_meta()` / `update_state()` / `update_agent()`;
  tags (`valid_tag()`,
  `claim_tag()` → `TagClaim::create_meta()`, the only way a `.meta` is born;
  `claim_spawn()` with its `Caps`; `backfill_tags()`), `runtime_dir()`, and
  `resolve_target()`, the one resolver every command
  that addresses an agent uses; a session's prompt files
  (`write_prompt()` / `remove_prompts()`) and `exclude_seagents()`, the one
  way `.seagents/` gets into a repo's `info/exclude`. No other module builds a `<pid>.*` path or
  parses one.
- **`src/turn.rs`** — `Ledger::in_turn()`, the one definition of "this agent
  is mid-turn" (§6), per adapter: the hook ledger plus the transcript's
  interrupt record, or the screen for hookless agents.
- **`src/transcript/`** — the agent's own conversation file (§10), the
  shared layer `read` and the future sessions browser read through:
  `model.rs` (the common record model), `claude.rs` (the one Claude JSONL
  parser: a line's cheap `head()` for the index, its `records()`, the
  subagent sidecars, and `interrupted_since()` for the turn ledger),
  `index.rs` (the lazy, incrementally extended offset index cached under
  `sessions/`), `mainline.rs` (the tree, the main line, `resume()` for a
  cursor), `render.rs` (the collapse rules); `Transcript` in `mod.rs` ties
  them together. `Format` names a parser; the adapter's `[read] transcript`
  picks it.
- **`src/read.rs`** — `seagents read`: the source (a target's reported
  transcript, or `--transcript`), the cursor and its token, continuing
  after one (`rewound`/`reset`), text and NDJSON output.
- **`src/status.rs`** — `State` enum, `Session`, `collect()` (reaps, then
  detects), `print()`, `read_session()` (detection), `settle()` (screen fused
  with the turn ledger), `observe_state()` (the `.state` transition rules), OSC-title parsing (`all_titles`,
  `current_title`, `strip_title_glyph`, `conversation_title`), `read_tail()`.
- **`src/detect.rs`** — `render()` (vt100) plus regression tests that pin the
  vendored engine's verdicts on live-capture scenarios; `debug_screen()`; the
  manifest-update helpers (`update_manifests_now()`, the HUD's daily
  `spawn_manifest_update()`, `manifest_health_warning()`).
- **`src/vendor/herdr_detect/`** — **herdr's detection engine, vendored**
  (see its `VENDORED.md`): per-agent TOML rule manifests evaluated by a
  region/priority engine, self-updating from herdr.dev. This replaced our
  hand-written `is_blocked()` rules entirely. Where herdr's answer to a rule
  differs from what seagents needs, `manifest/patches.rs` re-weights that one
  rule by id as the manifest loads, so the rest keeps updating upstream.
- **`src/window.rs`** — window identity and focus (§4): `focus()` (the
  `focus` command), `focus_window()` (exact handles: con_id, then X window id,
  with healing), `preview_window()`, `record_window()` (identity capture),
  `focused_con_id()`/`focused_terminal_ids()`, `window_ids_from_env()`,
  `window_ids_by_pid()`, `window_ids_for_handle()`, `terminal_class()` (which
  terminal `spawn` opens its window in).
- **`src/launch.rs`** — opening a terminal window seagents can recognize
  afterwards: `handle_for()` (the minted `seagents-<tag>` handle),
  `WindowMatch` (which X11 property carries it), the per-terminal recipes
  (`terminal()`, `recipe_for_class()`, `matcher_for()`), `spawn_terminal()`,
  `shell_quote()`. `spawn_terminal(&Run)` runs `<abs path of this binary> run
  --tag <tag> [--parent <p>] [--seed-file <f>] -- <argv>`, so the new
  window's PATH doesn't matter; `Run::keep_shell` is the one difference
  between its two callers — `restore` leaves the window at a shell when the
  agent exits (`; exec "$SHELL"`), `spawn` closes it with the agent. The
  window starts as a user's shell would: the calling agent's own session
  variables (every bundled adapter's `[launch] session_env` — Claude's
  `CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, messaging socket and token, … but
  never `CLAUDE_CONFIG_DIR` or other user config) are removed from the
  terminal's environment *and* `unset` at the head of its `sh -c` line, since
  a daemon-hosted window (`alacritty msg`) gets the daemon's environment, not
  ours. Inherited, they made a Claude started outside tmux never write its
  transcript (bisected in Phase 2), and the socket/token are that session's
  alone. seagents sets no per-agent variables of its own.
- **`src/wait.rs`** — `seagents wait` (§8): `decide()`, the whole reason
  logic as a pure function over one look at the ledger, `.state` and
  liveness; `parse_duration()`.
- **`src/spawn.rs`** — `seagents spawn` (§7): prompt checks, permission-mode
  resolution (`permission_args()`), caps from the environment, the seed file,
  `--worktree`, the launch, and the wait for the child's `.meta`.
- **`src/adapter.rs`** + **`src/adapters/*.toml`** — per-agent orchestration
  facts as data (ORCHESTRATION-SPEC "agent adapters"): how a seed prompt is
  passed, the settings/permission flags and mode order, where identity and
  turns come from, submit delay / pre-send / exit sequence. Keyed by the
  detection engine's agent id for `argv[0]`; `~/.config/seagents/agents/<agent>.toml`
  replaces the bundled file whole (an unparseable override is an error, not a
  silent default); unknown agents get the generic state-only adapter. Bundled:
  `claude`, and a conservative `codex` (no hooks, screen turns) until its
  `notify` hook is verified. `run` reads it to decide on hook injection
  (`settings_flag`, `hookless_flags`, `identity`/`turns`) and to turn a
  seed into argv (`Launch::seed_args()`); `spawn` reads the permission flag
  and mode order; `capabilities()` is what `status --json` reports per agent
  (`state`, `wait`, `turns`, `spawn`, `send`, `stop`, `read` as applicable); `send`
  and `stop` read `[send]` (submit delay, pre-send nudge, exit sequence, and
  when a body goes by reference: `long_prompt`, `inline_max_chars`,
  `inline_max_lines`); `[read] transcript` names the transcript format
  (`claude`) that `read` and the turn ledger's interrupt check parse —
  `read` is a capability where there is one and a way to learn the path.
- **`src/i3.rs`** — the only `i3-msg` caller: `tree()`, `windows()`,
  `focused_node()`, `focus()`, `msg()`, `active_outputs()`, `quote()`.
- **`src/procfs.rs`** — `/proc` access: `alive()` (the liveness rule),
  `parent_pid()`, `ancestors()`, `live_pty_size()`.
- **`src/clock.rs`** — `unix_now()` (seconds: `.meta`, `.state`, the
  snapshot) and `unix_now_ms()` (the `.agent` turn ledger).
- **`src/hud.rs`** — the ratatui HUD, event loop, preview-on-navigation.
- **`src/snapshot.rs`** — the reboot snapshot, layout capture/restore only:
  `capture()` (conversation ids read from each session's `.agent`), layout
  emission with i3 swallow criteria, `restore_cmd()` (which launches through
  `launch`).

### 1. `seagents run` — the transparent pty-tee launcher

Runs `<cmd>` on an *inner* PTY and relays stdin/output byte-for-byte between your
real terminal and the child, while **tee**-ing a copy of the output stream to a
file. "pty-teeing" = a T-junction in the terminal stream (like `tee`, named after
the plumbing fitting): the output flows through to your terminal unchanged *and* a
copy branches off for status detection. Transparency comes from **relaying, not
interpreting** — nothing in the hot path changes the stream, so colors, mouse,
alt-screen, resize (SIGWINCH) all pass through. (The output pump does *look*
at it, byte by byte, for the handful of mode switches `send` needs —
`wrapper/inject.rs`, §9 — and forwards every byte untouched.) Uses
`portable-pty` + `crossterm`.

We chose pty-tee over reading the terminal's screen because **you cannot read the
PTY of a process you didn't spawn** (a kernel isolation, not a missing feature).
The alternative was kitty's `@ get-text` introspection, rejected because the dev
machine has no kitty (`TERM=xterm-256color`, Terminator).

**Desktop notifications on transitions.** The wrapper's monitor thread fires
`notify-send` when its agent finishes (working → idle, normal urgency) or
starts waiting on input (→ blocked, critical urgency) — suppressed while the
agent's own i3 window is focused (you're already looking at it; tabs sharing
one window/con_id are all treated as focused). `SEAGENTS_NO_NOTIFY=1`
disables. herdr does this with OSC 9/99 terminal-native notifications, which
Terminator/VTE doesn't support — hence notify-send.

**Job control (Ctrl+Z) is forwarded, not passed through.** The agent does not run
directly as the inner PTY's session leader: a lone process group in its own
session is *orphaned*, and the kernel silently discards the SIGTSTP an agent
raises on Ctrl+Z (this is why suspend "did nothing" before 2026-07-08). Instead
`run` spawns a hidden anchor (`seagents __lead <cmd>`) as session leader, which
forks the agent into its own foreground process group — de-orphaned, so Ctrl+Z
stops it normally. The anchor mirrors the stop upward with `raise(SIGSTOP)`
(not discardable); the wrapper waits with `WUNTRACED`, has its output pump
relay whatever the agent printed on its way down (stopping itself freezes the
pump too — until 2026-09 that race left Claude's "run `fg`" line, or any last
output, unshown until `fg`, and made `tools/test-jobctl.py` flake), restores the
outer terminal, and suspends itself, so your shell shows the job as `Stopped`. `fg`
unwinds the chain: wrapper re-raws + resizes, continues the anchor, anchor
continues the agent's group and nudges a repaint with SIGWINCH.

### 2. Data model — `~/.cache/seagents/`

Per session, keyed by the wrapper's pid:

- **`<wrapper_pid>.raw`** — the teed output byte stream (for detection);
  rotated down to its last ~1 MB once it passes 16 MB (via a
  `<wrapper_pid>.raw.tmp` renamed over it).
- **`<wrapper_pid>.state`** — last observed state transition
  (`{"state": "idle", "since": <unix>, "seen": bool}`, plus `"rule"` — the
  matched detection rule's id, e.g. `live_blocked_form` — while `blocked`,
  which names the gate for `wait`); powers the
  "idle for / waiting for" column and the **done** state: the wrapper marks a
  working→idle completion `seen: false` (shown as 🔵 done, ranked just under
  blocked) until acknowledged — by a HUD preview/jump of that agent, or by the
  agent's window gaining focus — which flips it to plain idle without touching
  the clock. Reader-written transitions are always `seen: true`, so sessions
  from pre-monitor wrappers never show a stuck done.
  **Maintained by the wrapper itself** (a monitor thread re-detects
  its own agent every 2s — every 100 ms for a second after input reaches it
  at a gate, §8 — skipping the pass while the tee is unchanged), so
  durations stay truthful with no HUD open. HUD/status polls also write it
  whenever their own detection disagrees — the writers race benignly on
  transitions (both stamp since≈now; each read-decide-write is one locked
  update, and a transient Unknown never overwrites a known state).
  Transition-based on purpose: output age can't work because a
  focused Claude repaints on the terminal's focus-in report, which would
  reset the clock every time the HUD previews a window.
- **`<wrapper_pid>.meta`** — JSON identity:
  ```json
  {
    "wrapper_pid": 1437882,
    "child_pid": 1437883,          // the __lead anchor (agent's parent, same lifetime, same tty)
    "cwd": "/home/me/.../homelab", // launch cwd = grouping key (like herdr)
    "argv": ["claude"],
    "started": 1783468316,
    "cols": 212, "rows": 53,        // launch geometry (fallback for rendering)
    "con_id": 103471613296720,      // i3 window recorded at launch (focus handle)
    "window_id": 30728026,          // X11 id (survives an i3 restart)
    "tag": "homelab-3f9a",          // the agent's name (see Tags, below)
    "parent": null,                 // spawner's tag; depth = its depth + 1
    "depth": 0
  }
  ```
  `argv` is the user's command line — never what `run` injected (§6) —
  because `restore` relaunches from it. Parsed as a typed record with
  every field but `wrapper_pid` optional, and fields this binary doesn't know
  carried through untouched — live wrappers outlast a reinstall, so a `.meta`
  may come from an older or newer seagents than the one reading it.
- **`<wrapper_pid>.agent`** — what the agent reports about itself, written by
  `__hook` from its own lifecycle events (§6); absent for agents without
  hooks:
  ```json
  {
    "transcript_path": "/home/me/.claude-work/projects/…/1f81….jsonl",
    "session_id": "1f81…",          // the conversation, for restore's --resume
    "source": "startup",            // startup | resume | clear | compact
    "permission_mode": "bypassPermissions",
    "stops": 7,                     // +1 at every Stop; never resets
    "prompted_at": 1783468390120,   // last UserPromptSubmit (Unix ms)
    "stopped_at": 1783468399870,    // last Stop (Unix ms)
    "last_message": "…",            // the last turn's final assistant text
    "updated": 1783468399870
  }
  ```
  Its own file because it churns on every prompt and turn end while `.meta`
  almost never changes.
- **`<wrapper_pid>.lock`** — empty; `flock`ed around every change to `.meta`,
  `.state` or `.agent`.
- **`tags.lock`** — one for the whole cache; see Tags.
- **`<agent cwd>/.seagents/prompts/<wrapper_pid>-<stops>-<4 hex>.md`** — a
  `send` body too long to paste as the user's words (§9), 0600. Outside the
  cache, but the session's: named and deleted by `registry`.
- **`sessions/<session id>.idx`** — a transcript's offset index (§10),
  about a conversation rather than a live agent, so never reaped; a
  subagent's transcript (not named by a uuid) is keyed by a hash of its
  path. `registry::sessions_dir()`.
- **`$XDG_RUNTIME_DIR/seagents/<wrapper_pid>.sock`** — the wrapper's control
  socket (§9; 0600 in a 0700 dir, the cache dir when there is no runtime
  dir). Bound once the `.meta` exists; removed by the wrapper on exit and on
  SIGHUP/SIGTERM, and by `reap()` when no live session owns it (a SIGKILLed
  wrapper). A `sun_path` holds 108 bytes, so a very long `XDG_RUNTIME_DIR`
  means no socket (and no `send`); the agent still runs.

**Tags.** Every agent has one: its name for every command (`focus reviewer`),
the window handle `restore` swallows on (`seagents-<tag>`), and the lineage
key. `run --tag T` asks for one; otherwise `run` mints `<cwd basename>-<4 hex>`.
Charset `[a-z0-9][a-z0-9-]{0,31}`, never all digits (so a bare target can't be
mistaken for a pid). `registry::claim_tag()` checks uniqueness among live
agents under an exclusive `flock` on `tags.lock` and keeps it held — through
the pty open and the fork — until `TagClaim::create_meta()` has written the
`.meta`, so two concurrent `run`s cannot both get a name. A malformed or taken
`--tag`, or a `--parent` that isn't a live agent, exits 2 before anything is
spawned. Minting also avoids every tag recorded in `layout.json`, so a new
agent can't take a placeholder a pending restore will want back; a *requested*
tag may be one of those — that is how `restore` relaunches an agent under its
old name. `parent`/`depth` are lineage (0 without a parent); `spawn` derives
them from its caller and caps them (§7).

**Spawn reservations — `<tag>.pending`.** `spawn` and the `run` it launches are
different processes with a terminal between them, so the tags lock can't be
held from `spawn`'s check to the child's `.meta`. Instead `claim_spawn()`, under
the lock, writes `<tag>.pending` (`{tag, parent, spawner}`): other claims treat
the tag as taken (and count it against the caps), the spawned `run --tag`
may claim it and deletes it in `create_meta()`, and it lives only as long as
the spawner's pid (dead ones are swept by the next claim).

**Every sidecar goes through `registry.rs`**, because each has writers in
several processes (`.meta`: the wrapper at launch, window healing from its
input pump *and* from `focus`/the HUD, tag backfill from whichever process runs
a snapshot capture; `.agent`: each `__hook` the agent runs). Each change is `flock` on `.lock` → read →
mutate the typed record → write a pid-suffixed tmp file → rename over → unlock;
readers take no lock. tmp+rename is what stops a reader seeing torn JSON (a row
vanishing for a tick); only the lock stops two writers dropping each other's
update (two window heals landing at once, one silently lost).

Sessions self-reap: `status::collect()` (so `status` and the HUD) calls
`registry::reap()`, which deletes every file of a session whose child process is
gone (`/proc/<child_pid>` missing). So the HUD/status only ever show live agents;
there is no `ended` state. `registry::list()` applies the same liveness rule but
never deletes — the snapshot capture uses it, from inside wrappers' monitor
threads, which have no business deleting other sessions.

### 3. `seagents status` — detection

Classification is **herdr's manifest engine, vendored** at
`src/vendor/herdr_detect/` (from herdr v0.7.3; AGPL — see License below). Per
agent, a TOML manifest of rules is evaluated against *regions* of the screen
(`prompt_box_body`, `after_last_horizontal_rule`, `osc_title`, …) with
priorities — e.g. a `❯` in the prompt box is a *950-priority idle* signal that
outranks loose blocked heuristics, which is what kills the "chat text says
'would you like to…' → blocked" class of false positives our old hand rules had.

- Inputs we feed it: the **vt100-rendered screen** from the raw tail, plus the
  last non-empty **OSC title** (`current_title`). **Rendering is
  geometry-sensitive** — wrong cols/rows → garbled screen → detection silently
  fails. We read the child's **live** terminal size from
  `/proc/<child_pid>/fd/0` via `TIOCGWINSZ` (falling back to meta, then 80×24).
- The agent is identified from the recorded argv basename
  (`identify_agent("claude")`), so any of herdr's ~18 supported agents (codex,
  gemini, cursor, copilot, …) is detected if launched via `seagents run`.
- **Rules self-update**: manifests are fetched from
  `https://herdr.dev/agent-detection/index.toml` (shelling to `curl`), cached
  under `~/.local/state/seagents/agent-detection/`, version-gated (never
  downgrade, engine-version-checked). The HUD refreshes them in the background
  at most daily; `seagents update-manifests` forces it. Local overrides:
  `~/.config/seagents/agent-detection/<agent>.toml`.

### 4. `seagents focus` — jump to the agent's i3 window

**Exact handles only — no fuzzy matching.** X11 can't be asked "which window
hosts this pty" (Terminator shares one process across windows, so
`_NET_WM_PID` is useless); the only honest strategy is correlation at moments
we *know* the user is interacting with the agent:

- **At launch**: the focused terminal's `con_id` + **X window id** go into
  the meta (`seagents run` was just typed there).
- **On every input burst** (throttled to 1/min): typing proves the agent's
  terminal is focused, so the input pump re-captures both ids — healing
  stale handles within one keystroke of real use.

**Most terminals make all that correlation unnecessary**, and two paths ahead of
it try to say so:

1. **`window_ids_from_env()`** — the terminal simply tells its child which
   window it is: `ALACRITTY_WINDOW_ID`, or the old xterm `WINDOWID`. Measured on
   alacritty, the value equals i3's window id for every window. This is the best
   source available — instant, no subprocess, no focus, no handle — and it stays
   exact when one process owns many windows, so it works under
   `alacritty --daemon`. The id is validated against the i3 tree, so a value
   inherited somewhere it no longer applies is declined rather than believed.
   Terminator sets neither variable.
2. **`window_ids_by_pid()`** — walks this process's `/proc` ancestry and asks X
   (via `xdotool search --pid`) which window each ancestor owns; `seagents run`
   is a descendant of the terminal owning its window. Exact on a
   one-process-per-window terminal. Terminator shares one process across every
   window (measured: 25 windows, all `_NET_WM_PID = 3965`), so it finds many
   windows for one pid and abstains rather than guessing. `xdotool` is optional;
   without it this yields nothing and the paths below carry on.

Under alacritty either path answers directly and the "type into the agent once"
caveat disappears.

Resolution: con_id (`i3-msg [con_id=N] focus`; fast, dies on an i3 *restart*)
→ X window id (survives restarts; heals con_id when used) → a clear error
telling the user to type into the agent once. The old title-matching fallback
(parse the tee for the OSC title, fuzzy-match the i3 tree) was cut out
entirely: it was ambiguous between idle agents and read the `.raw` stream for
something that isn't a detection concern.

Why con_id and not PID: i3 on X11 doesn't expose a usable per-window PID, and
**Terminator shares one process across all its windows**, so PID→window is
impossible. At `seagents run` time the launching terminal is the focused i3 window,
so we capture its `con_id` then (guarded: only if the focused window is a
terminal-class window, so a stray dialog doesn't get recorded).

### 5. `seagents hud` — the floating panel

ratatui 0.28 TUI, polls `status::collect()` every 750 ms, color-codes rows
(blocked=red, done=cyan, working=yellow, idle=green). Navigation previews (reveal agent
window, then refocus the HUD so you keep navigating); Enter/click commits the jump.
The HUD remembers its own window (captured lazily on first preview) to return focus
to. Meant to run as an i3 floating + sticky window (see setup).

**Rows are named by directory, conversation title, or both (`t`).** The title is
the one Claude generates for the session — the human-readable line
`claude --resume` lists — which it also stamps into the terminal title behind a
state glyph (`✳ Detection update`). `read_session()` already parses that title
for the detection engine, so surfacing it costs nothing extra: it is
glyph-stripped by `status::conversation_title()` and carried on `Session.title`
(also emitted by `status --json`, for widget frontends). Two consequences worth
knowing:

- A session that hasn't named its conversation yet shows Claude's own banner
  (`Claude Code`), which says less than the directory does — that placeholder
  comes back **empty**, and the row falls back to the directory. Same for the
  stray punctuation-only titles some live tees carry.
- Titles are sentences where directories are words, so the row layout is a
  **ladder of candidate layouts** (`session_line`) rather than a fixed set of
  columns: the first that fits the width wins, and the optional columns drop
  widest-first — the command (`claude` on nearly every row), then the state
  word (the coloured dot already carries the state). The duration is allocated
  first and never sacrificed.
- In **both** mode the title yields *last*, not first. A 47-column window
  cannot hold four columns, and a `both` mode that dropped the title there
  would silently be `dir` mode — so the command and the state word are spent on
  the title before the row falls back to the directory alone. For the same
  reason the directory column is capped against the *window* (`width / 3`) and
  not just at 18 columns: one `20260612_doc_parser` on screen otherwise left a
  43-column row no room for a title at all.
- Column widths are measured once per frame (`Cols::measure`) and shared by
  every row, so the columns line up; a frame in which no agent has named its
  conversation draws no title column at all rather than a block of blanks.

Both view modes persist in `state_dir()` — one word per file (`hud-sort`,
`hud-label`: `urgency|dir|recent`, `both|title|dir`) — so the setting survives
closing the HUD window. `draw()` is covered headlessly through ratatui's
`TestBackend` (the *event loop* still needs a real terminal).

### 6. `seagents snapshot` / `restore` — surviving a reboot

`restore` rebuilds the workspaces your agents were on and relaunches each
window on **its own conversation** (`claude --resume <id>`), so a reboot costs
you the processes but not the layout or the context. Three problems:

**The layout must outlive the processes.** `~/.cache/seagents/` self-reaps — the
first `status` after a boot deletes every `.meta` whose child pid is gone, which
is all of them. So the snapshot lives at `state_dir()/layout.json` and is written
*continuously*: every wrapper's monitor thread offers a capture each tick,
globally throttled by the file's own mtime to one write per 30 s. The HUD's poll
loop and every `status` invocation offer one too — which is what covers agents
whose wrappers were started before this existed and so never call it themselves.
An unplanned reboot loses at most 30 s of layout change, and there is no command
to remember.

Continuous capture has one enemy: the *next* boot. Nothing is running after a
reboot, so the first HUD open, `status` or new agent would overwrite the old
boot's layout with an empty one before `restore` could read it. So the write
(`write_layout`, serialized on `layout.lock`) first compares the file's
`captured` with this boot's start — `btime` from `/proc/stat`
(`procfs::boot_time`) — and a layout from a previous boot that still holds
agents is moved to **`layout.prev-boot.json`** (replacing any older one); then
capture carries on as normal in the new boot. (A previous boot's *empty* layout
is just overwritten — setting it aside would bury an unrestored one from the
boot before.) `restore` reads `layout.prev-boot.json` when it exists, else
`layout.json` — the same-boot case (terminals closed or crashed, no reboot).
A full restore from the previous boot's file renames it to
`layout.restored.json` (kept, not deleted: it is the record of what the restore
worked from; `mv` it back to retry); `--dry-run` and `--workspace` leave it
pending. Its tags stay reserved against newly minted ones until then. `snapshot`
notes when a pending previous-boot layout, not the one it just wrote, is what
`restore` will use. `tools/test-restore.sh` plays the reboot with
`SEAGENTS_TEST_BTIME`, a test-only stand-in for btime.

**A window must be re-fillable.** i3's `append_layout` matches placeholders by
*swallow criteria*, and an ordinary terminal window offers nothing stable:
Terminator windows all share one class, carry no role, and show a title Claude
overwrites on the first repaint. Restored windows are therefore launched
carrying a minted handle, `seagents-<tag>`, and the placeholder swallows on that
alone.

The tag is the agent's own (§2), recorded per agent in `layout.json` along
with its `parent` and `depth`.
*Which* X11 property carries the handle is the terminal's business — Terminator
has `--role` and no `--class`; alacritty (and most others) are the reverse — so
the **stored layout is terminal-agnostic**: leaves keep a `tag`, and the swallow
criterion is built at restore time from `launch::WindowMatch`. The terminal is chosen **per agent**, from the
WM_CLASS captured with it — so a restore puts each window back in the terminal
it was actually running in, and a half-migrated setup (some agents in
Terminator, some in alacritty, swallowing on different properties) comes back
correctly. `SEAGENTS_TERMINAL` overrides that globally with a built-in recipe
(`terminator`, `alacritty`, `alacritty-daemon` — all verified end to end);
anything else uses
`SEAGENTS_TERMINAL_CMD` (a template over `{handle}`, `{cwd}`, `{cmd}`) together
with `SEAGENTS_TERMINAL_MATCH=role|instance|class`.
Those two go together on purpose: set the command without saying how to match
the window it opens and every placeholder silently stays empty. We emit the layout
JSON ourselves rather than shelling to `i3-save-tree`, and installing that tool
would not change it: it dumps a workspace whole (browsers and editors included)
where we need agents only with percentages renormalized, and it can only propose
criteria from a window's *current* class/instance/title — precisely the three
that don't discriminate here — commented out for hand-editing. The criterion
that works is a role we mint for a window that doesn't exist yet. Its output is
also not valid JSON, and it would be a subprocess per workspace every 30 s from
a thread that already holds the parsed tree.

Because `restore` launches those windows itself, it passes `--tag` (the
captured one) to `seagents run`, which then identifies its own window **by
handle — `handle_for(tag)` — instead of by focus**. Focus-based capture is a race a spawned window loses: the shell inside
reaches `run` while the terminal is still mapping, so several restored agents
would all record whichever window was focused *before* — one con_id shared by
many sessions. (Capture guards against that anyway: when two live sessions name
one window, the earliest launch keeps it and the rest are reported unplaced,
which the input-driven re-capture heals on the next keystroke.)

**A window must know which conversation it was.** The agent says so itself.
For an agent whose adapter takes hooks (Claude Code: `settings_flag`,
`identity`/`turns = "hook"`), `run` hands the child one extra `--settings`
carrying three hooks — `SessionStart`, `UserPromptSubmit`, `Stop` — each
running `'<abs path of this binary>' __hook` (absolute, so the agent's `PATH`
doesn't matter). Every payload carries `session_id` and `transcript_path`;
`__hook` finds its session by walking its own `/proc` ancestry (hook sh →
claude → `__lead` → wrapper) to the first pid with a live `.meta`
(`registry::enclosing_session`), and records the event in `.agent`. The
snapshot capture copies `.agent`'s `session_id` into the layout, and `restore`
resumes it (`resume_argv`: any stale `--resume`/`-c` flag dropped, and
`--resume <id>` inserted before a `--` — after one it would be read as prompt
text, and nothing after one is touched). Because `/clear` (and compact, and a resume) fires `SessionStart`
again with the new conversation, the id is always the one the window is
writing *now* — the stale-after-`/clear` problem the old resolvers had is gone,
and so is their `~/.claude/projects` hardcoding, which pointed at the wrong
directory under `CLAUDE_CONFIG_DIR=~/.claude-work` (the hook reports the real
path).

Details that matter:

- **One merged `--settings`, never a second one.** Verified 2026-09-28:
  repeated `--settings` flags *replace* each other (only the last one's hooks
  run), while settings-*file* hooks and flag hooks coexist. So the user's
  settings files are left alone (seagents never writes the user's config), and
  a `--settings X` the user passed (inline JSON or a path, `--settings X` or
  `--settings=X`) is loaded, has our three entries appended to its
  `hooks.<Event>` arrays, and goes back as ONE inline flag in its place. An X
  that can't be read or isn't a settings object passes through untouched and
  the agent runs without hooks, with a warning on stderr. Our flag goes right
  after `argv[0]`: anything after a `--` is the agent's positional text.
- **`claude --bare` skips hooks**, so nothing is injected under it (the
  adapter's `hookless_flags`); `.agent` stays empty.
- **`.meta` `argv` is the user's argv**, without the injected flag: `restore`
  relaunches from it, and a recorded `--settings` would stack on every restore.
- **`__hook` must never change the agent**: it always exits 0 (a `Stop` hook
  exiting 2 *keeps Claude from stopping*), prints nothing on stdout (hook
  stdout can be read as a decision), catches its own panics, and parses any
  arguments without error. It is one ancestry walk, one small read and one
  rename — no i3, no detection. Failures go to `~/.cache/seagents/hook.log`
  only under `SEAGENTS_DEBUG`. The wrapper forks the agent before its `.meta`
  exists, so a miss is retried for up to ½ s.
- **Every event refreshes identity**, not only `SessionStart`: a lost
  `SessionStart` heals at the next prompt. `source` comes only from
  `SessionStart`.

**The turn ledger** in `.agent` counts `Stop`s; it does not pair prompts with
turns (verified 2026-09-29: a message typed mid-turn fires `UserPromptSubmit`
reusing the running turn's `prompt_id` and is answered by a second `Stop` with a
fresh one; an Esc-interrupted turn fires `UserPromptSubmit` and no `Stop` at
all). Its stamps are **milliseconds** — a prompt in the same second as the last
`Stop` is the common case. `turn::Ledger::in_turn()` is the one definition of
"mid-turn", shared by `status`, `wait` and `send`:

- **Hook adapters** (Claude): prompted after the last `Stop`, **and** no
  interrupt record in the transcript since that prompt. On Esc Claude writes a
  `user` record whose text is `[Request interrupted by user]` (or `… for tool
  use]`); `transcript/claude.rs` looks for it in the transcript's last 256 KiB,
  newest first, stopping at the first record older than the prompt (and not
  parsing at all when the marker isn't in the tail). The screen is **not**
  consulted: Claude 2.1.284 streams a plain-text answer with no spinner and an
  idle title, so the detector says `idle` mid-answer, and a turn Esc'd before
  the detector ever saw it `working` would never have ended.
- **Screen adapters**: prompted after the last `Stop`, and `.state` not `idle`
  since the prompt.

The **state column** for hook adapters follows from it (`status::settle`):
`blocked` from the screen wins; else in a turn → `working`; else the screen's
verdict. The fused state is what `.state` records, so the done marker and the
"finished" notification fire when the turn really ends, not when the stream
merely looks idle. `status --json` reports `stops` and `in_turn`.

Residual, measured live on 2.1.284: an Esc that lands **before the model's
first response** (well under a second after submitting) doesn't interrupt a
turn — Claude puts the prompt back in the input box and writes neither an
interrupt record nor a `Stop` (and no hook event exists for it: the binary's
event list has no cancel). The transcript can't tell that from a stream in
progress either, because Claude writes an assistant text record only when the
block is complete. So such an agent reads as `in_turn`/`working` until its next
prompt or `Stop`, and a `wait` on it times out rather than returning.

**One-time cost of the hook cutover.** Wrappers started by a pre-hook binary
have no `.agent` (and the conversation id an old capture cached in their
`.meta` is no longer read), so a reboot before they are restarted restores them
*without* `--resume` — fresh conversations in the right windows. Restart those
agents once; every agent launched since carries its hooks.

`restore` is safe to run twice and safe to run when only half the agents died:
a restored agent comes back under its captured tag, so **a captured agent whose
tag is live is already on screen and is skipped** — tag identity alone makes
restore idempotent. Lineage comes back too: a child is relaunched with
`--parent` when its parent is live or restored earlier in the same run (each
workspace launches parents first, and waits up to 10 s for a parent's `.meta`);
a parent that is gone, or that restores on a later workspace than its child,
drops the lineage rather than failing the launch.

**One-time cost of the tag cutover.** Agents started by a pre-tag binary carry
no tag. A capture by the new binary backfills one into their `.meta`
(`registry::backfill_tags`), but their own (old-binary) monitor threads keep
writing the old layout format, whose tags are conversation-id prefixes /
`p<pid>` that match no live tag — so a `restore` run while such wrappers are
still alive can relaunch them as duplicates, and an old-format tag that happens
to be all digits is refused and not restored. Restart those agents (or reboot
and restore) once; after that every agent is tagged at launch.

### 7. `seagents spawn` — starting an agent on a prompt

The first command where seagents *acts* rather than observes, drawn narrowly:
it only launches, through the same launcher `restore` uses. Its primary
caller is another agent (ORCHESTRATION-SPEC), so every bound is derived from
something the caller can't casually change:

1. **Who is calling** = `registry::enclosing_session(own pid)`: the first
   `/proc` ancestor that is a live wrapper. None → a human. (Not an
   environment variable: everything an agent runs inherits and can edit
   those. An agent that deliberately escapes its tree — `setsid`,
   `systemd-run` — looks human; that is the threat model's line.)
2. **Caps and tag, under `tags.lock`** (`registry::claim_spawn`): for an
   agent caller the child is its child (`parent` = its tag, depth + 1), and
   depth ≤ 3, ≤ 4 live children per parent and ≤ 12 live agent-spawned
   agents machine-wide (`SEAGENTS_MAX_DEPTH` / `_MAX_CHILDREN` /
   `_MAX_SPAWNED`), counting other spawns' reservations; the refusal names the
   children in the way. A human's spawn has no parent and no caps. A
   requested `--tag` must be free of live agents, reservations **and the
   saved layout(s)** — unlike `run --tag`, which restore needs to reuse them.
3. **Permission mode**: from an agent, always explicit — the parent's own
   hook-reported `permission_mode` by default, refused above it (the adapter's
   `permission_modes` order, least privileged first), the least privileged
   when the parent's is unknown. From a human, passed only if given (the
   tool's configured default otherwise, as when typing `claude`). The reason
   for the asymmetry: this machine's default mode is `bypassPermissions`.
4. **`--worktree`**: `git worktree add <repo>/.seagents/worktrees/<tag> -b
   seagents/<tag>`, refusing an existing path or branch, with `.seagents/` in
   the repo's local `info/exclude`. Inside the repo because Claude's
   folder-trust dialog blocks every never-trusted directory (any mode), and a
   subdirectory inherits its parent's trust.
5. **The prompt** goes to `$XDG_RUNTIME_DIR/seagents/seed-<tag>` (0600;
   `registry::runtime_dir()`, falling back to the cache dir). `run --seed-file`
   reads and deletes it first thing and appends the adapter's `seed_prompt`
   (Claude: `-- <prompt>`, submitted as the first turn) to the **child's** argv
   only — never `.meta`'s, which `restore` replays. Adapters with no template,
   and prompts of 128 KiB or more (one argv string's limit), are refused before
   a window opens; the spec's paste-once-idle path for them isn't built, since
   no bundled adapter needs it (when it is, it pastes through
   `wrapper/inject.rs::deliver`, as `send` does). The prompt is visible in the agent's
   `/proc/<pid>/cmdline` to anyone who can read it (single-user machine).
6. **Launch** via `launch::spawn_terminal` with `keep_shell: false`, in the
   terminal class of the caller's own window (the focused one for a human;
   `SEAGENTS_TERMINAL` overrides), then **return** once the child's `.meta`
   exists (≤ 5 s) with its handle JSON. It does not wait for the agent to
   start: a never-trusted `--cwd` sits at Claude's trust dialog before any
   hook fires, which the detector calls `blocked` — `wait` reports it.

Errors exit 1 with the reason on stderr; stdout carries only the JSON.

### 8. `seagents wait` — the turn a caller is waiting on

`wait` blocks on the turn ledger, not the screen: polling `.state` for idle
returns at once right after `spawn` (the agent paints an idle input box before
it takes the seed) or right after a prompt (the detector ticks every 2 s), and
never sees a sub-tick turn as working at all. The caller holds a baseline N —
0 from `spawn`, `stops` from `send` — and polls every 250 ms:

| reason | exit | when |
|---|---|---|
| `turn` | 0 | `stops > N`: the turn after N ended; `last_message` is its final text (wins even if the agent has since exited) |
| `idle` | 0 | no `--after` and not in a turn: nothing to wait for, the last result at once |
| `timeout` | 2 | `--timeout` (default 5m; `30s`/`5m`/`1h`/seconds) passed — call again with the same N; nothing is consumed |
| `blocked` | 3 | `.state` is `blocked` (a permission prompt, the trust dialog); `rule` names the gate |
| `exit` | 4 | `/proc/<child_pid>` is gone (not `.meta`, which only goes when someone reaps) |
| `interrupted` | 5 | a turn opened since the last `Stop` is no longer running (Esc): it has no final message. Its own code so `wait && …` doesn't treat an abandoned turn as done |

1 is an error (unknown target; an N ahead of the ledger — a restored agent's
ledger restarts at 0, so wait without `--after`). `--for exit` waits for exit
only. Without `--after`, a running turn is waited on (N = current `stops`).
The reported `state` follows the ledger for hook agents (`.state` lags a turn's
end by up to one monitor tick). `wait` only reads: it never acknowledges
`done` and never writes `.state`.

The gate verdict has one source: `.state`, written by the wrapper's monitor
(the one process that sees every byte the agent prints), read by `wait`,
`send`'s gate and `stop`. Its 2 s tick would leave `blocked` standing for up
to a tick after a gate is answered, so input into a gated agent hurries it
(`wrapper/monitor.rs::Wake`): the control thread and the input pump nudge the
monitor, which then looks every 100 ms for a second — the agent repaints once
it has read the key, not when it is written — and `send --answer-permission`
replies only once the monitor has seen the gate go (≤ 1 s). A `wait` issued
the instant that returns gets the turn, not the answered gate (tested). The
alternative, `wait`/`send` rendering `.raw` themselves, was rejected: a
second detector per waiting process (a 1 MB vt100 render per poll while a
turn streams) that would still race the repaint.

### 9. `seagents send` / `stop` — driving an agent

Only the wrapper holds the agent's PTY master, so it is the wrapper that
writes: each listens on `<runtime dir>/<wrapper_pid>.sock` (§2), one JSON
request line in, one reply line out (`send.rs` has the format; any client
that speaks it is treated like the CLI).

**Authorization is the wrapper's**, per request, from `SO_PEERCRED`: the
connecting pid's `/proc` ancestry names the calling agent
(`registry::enclosing_session`, as for `spawn`). A human (no wrapper among its
ancestors) may send to and stop anyone and answer gates. An agent may act only
on its own descendants — walking the target's `parent` chain by tag, over live
`.meta`s, must reach the caller — so never on itself, its parent, a sibling or
a stranger (and A→B→A ping-pong can't happen), and may never
`--answer-permission`. Refusal: `forbidden`, exit 5. Also checked: the peer's
uid is ours.

**Submit when idle.** A `send` is held (up to `--timeout`, 5 min) until the
agent is out of its turn (`turn::Ledger::in_turn`, the one definition) and its
`.state` reads idle; then the body is written and the reply carries the
ledger's `stops` at that instant, `paired: true` — nothing else is in flight,
so `wait --after <stops>` returns the turn it started. Measured live
2026-09-29: a second `send` issued while the first's turn ran was held ~4 s
and each was answered by its own turn. Submissions are serialized in the
wrapper, and one counts as in flight until the agent's `UserPromptSubmit` hook
fires for it (or 5 s pass), so two back-to-back sends can't both see "idle"
and fold into one turn. `--now` writes at once (`paired: false`: a prompt sent
mid-turn is answered by its own later `Stop`, verified, but `wait --after N`
returns the running turn's). A **blocked** agent is refused at once — reason
`blocked`, exit 3, with the gate's `rule` from `.state` — even with `--now`,
since whatever is typed into a permission menu answers it;
`--answer-permission` (humans only) is the way through, usually with
`--keys` (e.g. `--keys 1 --no-submit` picks "Yes" on Claude's menu, verified),
and replies once the gate is seen gone (≤ 1 s, §8). An agent no
detector knows (a plain shell) can't be seen to be idle: only `--now` sends
to it (reason `unsupported` otherwise). Exit codes: 0 ok, 2 `busy_timeout`
(nothing was sent), 3 `blocked`, 4 `exit`, 5 `forbidden`, 1 anything else.

**Framing** (`wrapper/inject.rs`, copied from herdr): text is a bracketed
paste (`ESC[200~…ESC[201~`, the end marker stripped from the body) when the
agent has enabled DECSET 2004, raw otherwise; the adapter's `pre_send` nudge
(`focus_gained` → `ESC[I`) first; the body is one write under the writer lock,
so the human's keystrokes (the input pump locks per chunk) can't land inside
it; then the lock is released, `submit_delay_ms` (300) passes, and Enter is
written — `\r`, or `CSI 13 u` when the agent has pushed kitty keyboard flags
that report every key (flag 8; the output pump tracks the flag stacks per
screen). modifyOtherKeys isn't tracked: it never changes an unmodified Enter.
`--keys` writes bytes verbatim after decoding `\e \r \n \t \0 \\ \xNN`
(in the wrapper, so a raw client gets the same syntax); `--no-submit` skips
the Enter. Verified live on Claude 2.1.284: a multi-line body with both quote
kinds, backticks, `$` and backslashes arrives exactly. Prompt text is never
logged.

**Long bodies go by reference.** Claude files a large paste in its transcript
as `<pasted_content id="…">…`, and current models treat that as data "that may
contain instructions the user did not write" — haiku declined to act on one in
Phase 2; a raw unbracketed burst is treated the same. Measured (2.1.284, haiku,
bracketed paste + 300 ms + Enter): one line of 700 chars and 2 short lines
land inline, as the user's words; one line of 900 chars, or 7 short lines, are
wrapped. So the adapter says when (`[send] long_prompt = "file"`,
`inline_max_chars = 700`, `inline_max_lines = 2` for Claude; every other
adapter pastes everything): past either limit the wrapper writes the body to
`<agent cwd>/.seagents/prompts/<wrapper_pid>-<stops>-<4 hex>.md` (0600,
`registry::write_prompt`; `.seagents/` goes into the repo's `info/exclude`
first, through the same helper `--worktree` uses) and pastes one line instead:
`Read the file .seagents/prompts/<name>.md (relative to your working
directory) and carry out the instructions in it. They were sent to you through
seagents by <@caller | the user>.` A Read inside the agent's cwd needs no
permission — verified in `--permission-mode default`, where one outside it
would stop at a prompt. The files are the session's: the wrapper deletes them
when its agent exits (so on `stop`), and `reap` for one that died harder —
only names with that wrapper's pid prefix. `--keys` is untouched. Seed prompts
need none of this: on the command line they arrive inline at any size
(verified: 50 lines, ~5000 chars).

**`stop`** types the adapter's `exit_sequence` (Claude: `/exit`, ~1 s) as a
`--now` paste and gives it 5 s; then SIGTERMs the PTY's foreground process
group (the agent's, via `tcgetpgrp` on the master) and continues it, then
SIGKILLs it and the anchor, 5 s apart; it replies once the agent is gone.
`--force` skips the exit sequence. So does a **blocked** agent — `/exit` +
Enter typed into a permission menu would pick an answer — which goes straight
to SIGTERM (verified: the gated command never ran). The lineage rule applies;
the blocked refusal does not. The wrapper exits with its agent, so
`Control::finish()` lets in-flight requests reply (≤ 2 s) before the socket is
removed.

### 10. `seagents read` — the conversation

Content comes from the agent's own transcript (the path its hooks
reported in `.agent`; `--transcript PATH` reads any file), never the screen.
Everything is parsed into the **common record model** (`transcript/model.rs`):
`user`/`assistant`/`thinking` `{text}`, `tool_use {id, name, input[,
subagent]}`, `tool_result {tool_use_id, text, bytes[, is_error]}`, each with
`uuid`, `parent_uuid`, `timestamp`. One JSONL line can hold several blocks;
it yields several records sharing the line's uuid, while the tree, the main
line and the cursor are made of lines, so they never care about blocks.

**The main line.** A transcript is a tree (`uuid`/`parentUuid`): a rewind or
an edit branches off an earlier node, so the file in line order interleaves
abandoned attempts. `read` shows root → newest tip — the newest
*conversation* node with no conversation below it. Not the newest leaf of
any kind: `system`, `attachment` and `progress` lines are tree nodes too (a
prompt's parent is often the last turn's `turn_duration` note) and many are
side leaves — measured, 94 are newer than the sibling the conversation went
on through, and a cursor left on one would read as a rewind a moment later.
A compaction's boundary takes its `logicalParentUuid` as parent; when that
line was never written (5 of 15 here) the walk bridges to the newest tip
before the boundary, so the line still starts at the real beginning.

**Collapse rules** (`render.rs`, SESSIONS-SPEC's): user text in full behind
`│`, assistant text in full, `⋯ thinking (N words)`, `● Tool(short arg)`,
`  ⎿ first line (N bytes)`; `--raw` shows all. A real subagent (an `Agent` /
`Task` call) is one line at its call, `▸ subagent: <type> — <description>`,
labelled from its `subagents/agent-<id>.meta.json` sidecar when there is one
(matched by `toolUseId`) and from the call's input otherwise; the
`agent-a<word>-…` helper transcripts are never subagents. `isMeta` user lines
(image notes, skill bodies) are context, not the user's words: not shown. A
user line that only points at a prompt `send` wrote by reference gets the
file's words under it while the file exists (text mode only).

**The cursor** is `{transcript_path, offset, leaf}` (plus `token`, the same as
`<offset>:<leaf>:<path>` — the text mode's last line is `-- cursor: <token>`,
and `--from` takes that line, the token, or the JSON cursor). `leaf` is the
main line's tip. `--from` continues after it if the current main line still
runs through it; if not (a rewind), `{"rewound":{"fork":…}}` names the node
the two lines share and reading resumes after it; a different path (`/clear`
starts a new transcript) or a file shorter than `offset` is `{"reset":…}`,
read from the start. **Claude writes its transcript behind its hooks**
(measured 2.1.284: a read the moment `wait` returned saw the prompt but not
the answer, ~60 ms late), so a target read after a finished turn waits (≤ 1
s) until a line stamped at or after the `Stop` is there — the turn's
`stop_hook_summary`, written once the hooks have run.

**The offset index** (`index.rs`, SESSIONS-SPEC's `.idx`): one 53-byte entry
per tree node (offset, length, kind, timestamp, uuid, parent), so the main
line is found without parsing content and `--tail N` reads only the lines it
prints. Cached in `~/.cache/seagents/sessions/`, extended from the last
indexed byte on every open (transcripts are append-only; only complete lines
are indexed), rebuilt when the file is shorter than that or its first 4 KiB
no longer hash the same. Measured on the largest transcript here (53 MB):
`read --tail 20` in 65 ms cold, 6 ms warm; the whole main line as NDJSON in
90 ms.

---

## Key design decisions (and why)

- **pty-tee, not screen-scraping** — can't read another process's PTY; pty-tee is
  terminal-agnostic. (kitty introspection was the alternative, no kitty here.)
- **Vendored herdr detection (2026-07-08)** — we first wrote our own rules, hit
  the false-positive wall (chat text tripping "blocked"), and replaced them with
  herdr's engine + manifests wholesale. Rationale: rules are maintained upstream
  and self-update at runtime; only an engine-version bump (currently v2) ever
  requires re-syncing the vendored code. **This commits seagents to
  AGPL-3.0-or-later permanently** (LICENSE added; the "maybe relicense MIT"
  option is gone). Re-sync procedure: `src/vendor/herdr_detect/VENDORED.md`.
- **focus by recorded con_id** — the only reliable window handle given Terminator's
  shared PID and colliding idle titles.
- **self-reaping cache** — a fully-exited agent isn't interesting; "idle" already
  means finished-and-waiting-at-prompt (still alive).

---

## Limitations & gotchas

- **Existing sessions need relaunching after upgrades** — the tee/`con_id` only
  exist for agents started via `seagents run`. Agents already running, or started
  before a change, won't be tracked/fixed until relaunched.
- **Multiple agents as tabs in one Terminator window** share one `con_id` (one X
  window); i3 can't focus a tab within a window, so that case is ambiguous.
  Separate windows work perfectly.
- **Window identity needs one interaction — on a shared-process terminal
  only.** Under alacritty the `_NET_WM_PID` lookup answers exactly and this
  does not arise. Under Terminator, if a dialog steals focus at the
  `run` instant, the ids are recorded as `null` — the first keystroke into the
  agent heals them (input-driven re-capture). A session never typed into and
  never focused can't be focused from the HUD until then.
- **Mouse flicker on HUD preview** — with i3's default `mouse_warping output`, each
  preview warps the mouse to the agent's monitor and back. Setting `mouse_warping
  none` in i3 removes it, but that's a *global* i3 behavior change (undecided).
- **`restore` reproduces a layout exactly only when nothing is running.** i3
  appends a layout to a *workspace*; there is no way to drop a placeholder into
  a container that already holds live windows. The post-reboot case (nothing
  alive) is exact — verified by `tools/test-restore.sh`. Restoring alongside
  survivors puts the missing windows at workspace level instead, and says so.
- **Only `terminator`, `alacritty` and `alacritty-daemon` have built-in launch
  recipes.** All three are verified by `tools/test-restore.sh`.
  `alacritty-daemon` needs a daemon already running (`alacritty --daemon`) and
  reachable — `alacritty msg` finds it via `ALACRITTY_SOCKET` or the default
  socket path; without one it fails per window with "no socket found". Any other terminal needs
  `SEAGENTS_TERMINAL_CMD` *and* `SEAGENTS_TERMINAL_MATCH` — seagents errors
  rather than guessing, because guessing wrong shows up as placeholders that
  never fill and no error at all.
- **A previous boot's layout waits for the first restore.** Until a full
  `restore` consumes `layout.prev-boot.json`, every `restore` reads it rather
  than the live `layout.json` — so if you skip restoring after a reboot and
  then lose terminals in the new boot, `restore` brings back the *old* boot
  (dedup by tag keeps it from duplicating anything live). Delete or rename the
  file to fall back to the live layout. Boot identity is `btime`, which a large
  wall-clock step after boot shifts; a capture made before such a step can be
  mistaken for a previous boot's (harmless: it is set aside and restore
  dedups by tag).
- **Floating agents restore tiled** — placeholder matching is per-container, and
  an agent that comes back tiled is a smaller surprise than one that doesn't.
- **Restored windows land on whichever workspace is focused when they spawn**,
  so don't switch workspaces during a restore. (`restore` focuses each target
  workspace before appending and spawning; it can't stop you moving.)
- **Detection covers herdr's agent roster** — any agent with a bundled manifest
  (claude, codex, gemini, cursor, copilot, …) is classified when launched via
  `seagents run <agent>`; unrecognized commands show as ⚪ unknown.
- **`bg` on a suspended agent is a deferred `fg`, by design** — resuming in a
  background pgrp can't re-enter raw mode (SIGTTOU would re-stop the job), and a
  backgrounded fullscreen TUI is useless anyway, so the wrapper holds the agent
  stopped until it owns the terminal again. `fg` unwinds everything.
- **keyd `overload(meta, esc)` kills the HUD on `$mod`+drag** — mouse buttons
  don't pass through keyd, so a capslock(meta)+mouse-drag counts as a *tap* and
  emits Esc into the focused window on release; the HUD quits via its Esc
  binding (looks like a crash, exit 0). Same stray Esc also interrupts a focused
  Claude Code when dragging its window. Fix on each keyd machine:
  `[global] overload_tap_timeout = 200` in `/etc/keyd/default.conf`, then
  `keyd reload`. (Done on the laptop 2026-07-08; check other machines.) The HUD
  additionally ignores an Esc that arrives with more input pending, so torn
  mouse escape sequences can't fake a quit.

---

## Open items / backlog / next steps

- **Verify** the con_id focus fix live (see Current Status).
- Decide the **mouse_warping** question for the HUD preview.
- **README.md** for the repo landing page (not written yet).
- ~~**License** decision~~ — settled 2026-07-08: vendoring herdr's detector
  locks AGPL-3.0-or-later; LICENSE file added.
- Sweep **orphan `.raw`** files that have no `.meta` (only pre-`meta`-era leftovers).
- Add `restore` to i3 autostart (`exec --no-startup-id seagents restore`) once
  it has been through a real reboot, or keep running it by hand.
- Optional: **eww** port of the HUD for a prettier always-on widget.
- Optional: read **herdr's socket API** to fold herdr-hosted agents into the same
  HUD (the user runs both herdr and seagents).

---

## Dev workflow

```bash
cargo build            # or --release
cargo test             # all tests (vendored engine + manifests, title parsing, glyph strip)
cargo run -- status    # etc.

# Inspect detection against a real capture (state + matched rule; --json for
# the full evaluated-rule breakdown, like herdr's `agent explain`):
seagents debug-screen ~/.cache/seagents/<pid>.raw <cols> <rows> [--json]
# get <cols>/<rows> from: stty size < /proc/<child_pid>/fd/0
# SEAGENTS_DEBUG=1 surfaces manifest-update warnings on stderr

# Regression-test the Ctrl+Z suspend/resume chain headlessly (scripted pty
# driving an interactive bash — run after touching anything in src/wrapper/):
python3 tools/test-jobctl.py

# Debug the HUD post-mortem (its terminal vanishes with it):
SEAGENTS_HUD_LOG=/tmp/hud-events.log seagents hud
# appends every crossterm input event + the exit reason
# Tests that open windows run inside a nested, isolated desktop — headless
# weston + rootful Xwayland on :9 + its own i3, with HOME and XDG_RUNTIME_DIR
# pointed at a scratch dir — set up by tools/lib-nested-desktop.sh, which each
# sources (nd_init <terminal>; nd_start; cleanup on exit kills every window in
# it, then the desktop).
#
# Regression-test snapshot/restore. Agents are `sleep`, so nothing real is
# launched. Fails with a tree diff, and refuses to pass if the pre-restore
# kill left anything alive (a restore that does nothing would otherwise
# compare equal). Run it for each terminal recipe.
tools/test-restore.sh terminator
tools/test-restore.sh alacritty
tools/test-restore.sh alacritty-daemon

# Integration-test spawn → wait → status → send (idle-paired; multi-line and
# > 700-char bodies by reference, carried out in --permission-mode default;
# a Bash permission gate: wait says blocked, send --answer-permission, and an
# immediate wait gets the turn) →
# lineage/caps and the socket's lineage rule → focus → stop, with a real
# Claude (`--model haiku`, one-line prompts: a few cents a run) in
# target/sg-it/orchestration, whose folder trust it inherits from the repo.
# Keeps the real CLAUDE_CONFIG_DIR for auth, and runs as if from inside a
# Claude session (stand-ins for any of its session variables not already set),
# checking the agent's /proc/<pid>/environ holds none of them — the launcher's
# job, not the script's. PASS/FAIL per step.
tools/test-orchestration.sh [alacritty|terminator|alacritty-daemon]
```

The HUD and interactive relay need a real terminal — they can't be driven from a
pipe/CI, so validate those by hand.

---

## Conventions

- **Commits: no AI co-author.** Do not add `Co-Authored-By: Claude ...` or
  "Generated with" lines. The user authors commits solely. (Conventional-commit
  style, e.g. `fix(focus): ...`, is used.)
- Repo: `git@github.com:AnakTeka/seagents.git`, **private**, default branch `main`.
- Local git identity: `Yoga Yudistira <yudistirayoga97@gmail.com>`.

---

## Environment (dev machine — may differ on yours)

Arch Linux, i3wm, Terminator terminal, no kitty. The other laptop also runs i3wm.
Cargo/rustc were 1.96. seagents assumes X11 i3 (uses `i3-msg -t get_tree`); it has
not been tested on sway/Wayland.
