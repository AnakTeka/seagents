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
emulator. Everything else is vendored via Cargo. `xprop`/`xdotool` were present on
the dev machine but seagents currently only shells out to **`i3-msg`**.

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

Then reload i3 (`$mod+Shift+c` or `i3-msg reload`). If you use a terminal other
than Terminator, swap the launch command but keep a matchable `window_role`/title;
seagents itself is terminal-agnostic.

---

## Commands

```
seagents [hud]              floating agent HUD (also the default with no args)
seagents run <cmd> [args]   launch an agent, teeing its output (this is how tracking happens)
seagents status [--json] [--sort=urgency|dir|recent]
                           one-shot table of every tracked agent's state
                           (--json: machine-readable, for eww/widget frontends;
                            sorts: urgency = triage, longest-waiting first;
                            dir = grouped by cwd; recent = latest change first)
seagents focus <child_pid> focus that agent's i3 window
seagents snapshot [--refresh] [--print]
                           capture the agent layout now (it is also captured
                           continuously; --refresh re-resolves session ids)
seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]
                           rebuild that layout after a reboot, each window
                           resuming its own conversation
seagents update-manifests  fetch fresh detection rules from herdr.dev now
seagents debug-screen <raw_file> [cols] [rows] [--json] [--agent=<id>]
                           dev tool: render a .raw + the engine's explain verdict
```

HUD keys: `j`/`k` or arrows = navigate + **preview** (reveal the agent window,
keep focus on HUD); `g`/`G` = first/last row (vim-style, also previews);
`Enter`/left-click = **jump** (commit focus to the agent);
`s` = cycle sort (urgency → dir → recent; remembered across launches);
`r` = refresh now; `q`/`Esc` = quit.

---

## Architecture / how it works

Source layout under `src/`:

- **`src/main.rs`** — CLI dispatch; the `run()` pty-tee wrapper; `cache_dir()`,
  `session_paths()`, `write_meta()`, `terminal_size()`; manifest-update helpers.
- **`src/status.rs`** — `State` enum, `Session`, `collect()`, `print()`,
  `read_session()` (detection + reaping), OSC-title parsing (`all_titles`,
  `current_title`), `live_pty_size()`, `read_tail()`.
- **`src/detect.rs`** — `render()` (vt100) plus regression tests that pin the
  vendored engine's verdicts on live-capture scenarios.
- **`src/vendor/herdr_detect/`** — **herdr's detection engine, vendored**
  (see its `VENDORED.md`): per-agent TOML rule manifests evaluated by a
  region/priority engine, self-updating from herdr.dev. This replaced our
  hand-written `is_blocked()` rules entirely. Where herdr's answer to a rule
  differs from what seagents needs, `manifest/patches.rs` re-weights that one
  rule by id as the manifest loads, so the rest keeps updating upstream.
- **`src/focus.rs`** — `focus_window()` (exact handles: con_id, then X window
  id, with healing), `preview_window()`, `record_window()` (identity capture),
  `focused_con_id()`/`focused_terminal_ids()`, i3 tree helpers.
- **`src/hud.rs`** — the ratatui HUD, event loop, preview-on-navigation.
- **`src/snapshot.rs`** — the reboot snapshot: `capture()`, layout emission
  with i3 swallow criteria, `resolve_session_ids()`, `restore_cmd()`.

### 1. `seagents run` — the transparent pty-tee launcher

Runs `<cmd>` on an *inner* PTY and relays stdin/output byte-for-byte between your
real terminal and the child, while **tee**-ing a copy of the output stream to a
file. "pty-teeing" = a T-junction in the terminal stream (like `tee`, named after
the plumbing fitting): the output flows through to your terminal unchanged *and* a
copy branches off for status detection. Transparency comes from **relaying, not
interpreting** — we never parse the stream in the hot path, so colors, mouse,
alt-screen, resize (SIGWINCH) all pass through. Uses `portable-pty` + `crossterm`.

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
(not discardable); the wrapper waits with `WUNTRACED`, restores the outer
terminal, and suspends itself, so your shell shows the job as `Stopped`. `fg`
unwinds the chain: wrapper re-raws + resizes, continues the anchor, anchor
continues the agent's group and nudges a repaint with SIGWINCH.

### 2. Data model — `~/.cache/seagents/`

Per session, keyed by the wrapper's pid:

- **`<wrapper_pid>.raw`** — the teed output byte stream (for detection);
  rotated down to its last ~1 MB once it passes 16 MB.
- **`<wrapper_pid>.state`** — last observed state transition
  (`{"state": "idle", "since": <unix>, "seen": bool}`); powers the
  "idle for / waiting for" column and the **done** state: the wrapper marks a
  working→idle completion `seen: false` (shown as 🔵 done, ranked just under
  blocked) until acknowledged — by a HUD preview/jump of that agent, or by the
  agent's window gaining focus — which flips it to plain idle without touching
  the clock. Reader-written transitions are always `seen: true`, so sessions
  from pre-monitor wrappers never show a stuck done.
  **Maintained by the wrapper itself** (a monitor thread re-detects
  its own agent every 2s, skipping the pass while the tee is unchanged), so
  durations stay truthful with no HUD open. HUD/status polls also write it
  whenever their own detection disagrees — the writers race benignly on
  transitions (both stamp since≈now; writes are atomic tmp+rename, and a
  transient Unknown never overwrites a known state).
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
    "session_id": "5c603939-…",     // the agent's own conversation, for restore
    "session_id_source": "proc"     // how it was learned: "proc" | "title"
  }
  ```
  `session_id` is filled in lazily by the snapshot capture (see §6), not by
  `run` — nothing at launch time knows it yet.

Sessions self-reap: `read_session()` deletes both files when the child process is
gone (`/proc/<child_pid>` missing). So the HUD/status only ever show live agents;
there is no `ended` state.

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

**A window must be re-fillable.** i3's `append_layout` matches placeholders by
*swallow criteria*, and an ordinary terminal window offers nothing stable:
Terminator windows all share one class, carry no role, and show a title Claude
overwrites on the first repaint. Restored windows are therefore launched
carrying a minted handle, `seagents-<tag>`, and the placeholder swallows on that
alone.

*Which* X11 property carries the handle is the terminal's business — Terminator
has `--role` and no `--class`; alacritty (and most others) are the reverse — so
the **stored layout is terminal-agnostic**: leaves keep a `tag`, and the swallow
criterion is built at restore time from `WindowMatch`. `SEAGENTS_TERMINAL`
selects a built-in recipe (`terminator`, `alacritty` — both verified end to
end); anything else uses `SEAGENTS_TERMINAL_CMD` (a template over `{handle}`,
`{cwd}`, `{cmd}`) together with `SEAGENTS_TERMINAL_MATCH=role|instance|class`.
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

Because `restore` launches those windows itself, it also passes `--window` to
`seagents run`, which then identifies its own window **by handle instead of by
focus**. Focus-based capture is a race a spawned window loses: the shell inside
reaches `run` while the terminal is still mapping, so several restored agents
would all record whichever window was focused *before* — one con_id shared by
many sessions. (Capture guards against that anyway: when two live sessions name
one window, the earliest launch keeps it and the rest are reported unplaced,
which the input-driven re-capture heals on the next keystroke.)

**A window must know which conversation it was.** Nothing in a `.meta` records
it, so `resolve_session_ids()` recovers it from two sources, strongest first,
and caches the answer into the `.meta` so the pass is a no-op afterwards:

1. **`/proc` file descriptors** — a running Claude holds a directory fd on
   `/tmp/claude-<uid>/<project-slug>/<session-uuid>/`, validated against the
   transcript actually existing. Exact, but only present once a session has used
   that scratch area: 14 of 20 live sessions on the dev machine.
2. **Window title ↔ transcript `aiTitle`** — Claude writes the conversation's
   generated title into both the transcript and the terminal's title, so a
   transcript in the same project carrying the window's title is almost
   certainly the one. The title comes from the **i3 tree**, not from re-deriving
   the last OSC title out of the tee (two live sessions here have a stray `;`
   title later in the stream than their real one). Accepted only when it
   identifies exactly one *unclaimed* transcript whose mtime is at least the
   wrapper's start time — two windows in a project can show the same title (a
   resumed session and its parent), and the wrong conversation is worse than
   none.

Together: 17 of the dev machine's 19 placed agents resolved, the two refusals
being a genuinely ambiguous duplicate title and a session still showing the
default "Claude Code". **Matching on transcript mtime was tried and rejected —
it was wrong 3 times out of 3.** It is not a fallback, it is a coin flip.

`restore` is safe to run twice and safe to run when only half the agents died: a
conversation already on screen is skipped by id, and agents without an id are
matched *by count* per launch signature (project + argv, ignoring `--resume`),
so three live out of four captured restores exactly one.

---

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
- **Window identity needs one interaction**: if a dialog steals focus at the
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
- **A `/clear` invalidates a cached `session_id`.** Both resolvers describe the
  transcript a session is writing *now*; `/clear` starts a new one under a new
  id, and the cached value in the `.meta` goes stale until `snapshot --refresh`.
  The durable fix is Claude's `SessionStart` hook (see ORCHESTRATION-SPEC),
  which reports the id on every identity change — `--resume`, `/clear`, compact.
  Deliberately not wired in here: it means injecting `--settings` into every
  `claude` launch, which is the orchestration milestone's call to make.
- **Only `terminator` and `alacritty` have built-in launch recipes.** Both are
  verified by `tools/test-restore.sh`. Any other terminal needs
  `SEAGENTS_TERMINAL_CMD` *and* `SEAGENTS_TERMINAL_MATCH` — seagents errors
  rather than guessing, because guessing wrong shows up as placeholders that
  never fill and no error at all.
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
- **Wire the `SessionStart` hook** so `session_id` is exact for every new
  session (and survives `/clear`) instead of being recovered after the fact —
  ORCHESTRATION-SPEC already specs it; today's two resolvers are the bridge for
  sessions that predate it.
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
# driving an interactive bash — run after touching run()/lead()/the pumps):
python3 tools/test-jobctl.py

# Debug the HUD post-mortem (its terminal vanishes with it):
SEAGENTS_HUD_LOG=/tmp/hud-events.log seagents hud
# appends every crossterm input event + the exit reason
# Regression-test snapshot/restore. Restore rearranges workspaces and opens
# windows, so it runs inside a nested, isolated desktop (headless weston +
# rootful Xwayland on :9 + its own i3, with HOME pointed at a scratch dir);
# agents are `sleep`, so nothing real is launched. Fails with a tree diff, and
# refuses to pass if the pre-restore kill left anything alive (a restore that
# does nothing would otherwise compare equal). Run it for each terminal recipe.
tools/test-restore.sh terminator
tools/test-restore.sh alacritty
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
