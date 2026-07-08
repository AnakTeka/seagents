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
agent's status (blocked / working / idle) and, on click, focuses that agent's real
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

**Known-unverified:** the focus-by-`con_id` disambiguation (see Design Decisions)
was the last change and hasn't been confirmed live by the user. Test: launch two
idle claudes (both showing the generic `✳ Claude Code` title) in separate windows,
open the HUD, and confirm clicking each focuses its *own* window (not the same one).

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
seagents status [--json]   one-shot table of every tracked agent's state
                           (--json: machine-readable, for eww/widget frontends)
seagents focus <child_pid> focus that agent's i3 window
seagents update-manifests  fetch fresh detection rules from herdr.dev now
seagents debug-screen <raw_file> [cols] [rows] [--json] [--agent=<id>]
                           dev tool: render a .raw + the engine's explain verdict
```

HUD keys: `j`/`k` or arrows = navigate + **preview** (reveal the agent window,
keep focus on HUD); `Enter`/left-click = **jump** (commit focus to the agent);
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
  hand-written `is_blocked()` rules entirely.
- **`src/focus.rs`** — `focus_window()` (con_id-first, title fallback),
  `preview_window()`, `focused_con_id()`/`focused_terminal_con_id()`,
  `session_for_child()`, i3 tree helpers, `core_title()`.
- **`src/hud.rs`** — the ratatui HUD, event loop, preview-on-navigation.

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
  (`{"state": "idle", "since": <unix>}`); powers the "idle for / waiting for"
  column. **Maintained by the wrapper itself** (a monitor thread re-detects
  its own agent every 2s), so durations stay truthful with no HUD open;
  HUD/status polls only write it as a fallback for sessions from pre-monitor
  wrappers. Transition-based on purpose: output age can't work because a
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
    "con_id": 103471613296720       // i3 window recorded at launch (focus handle)
  }
  ```

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

**Primary path:** the `con_id` recorded in `.meta` at launch → `i3-msg [con_id=N]
focus`. Exact, no ambiguity. **Fallback:** match the agent's current window title
against the i3 tree (`core_title` strips the leading state glyph so a changing
spinner doesn't break the match).

Why con_id and not PID: i3 on X11 doesn't expose a usable per-window PID, and
**Terminator shares one process across all its windows**, so PID→window is
impossible. At `seagents run` time the launching terminal is the focused i3 window,
so we capture its `con_id` then (guarded: only if the focused window is a
terminal-class window, so a stray dialog doesn't get recorded).

### 5. `seagents hud` — the floating panel

ratatui 0.28 TUI, polls `status::collect()` every 750 ms, color-codes rows
(blocked=red, working=yellow, idle=green). Navigation previews (reveal agent
window, then refocus the HUD so you keep navigating); Enter/click commits the jump.
The HUD remembers its own window (captured lazily on first preview) to return focus
to. Meant to run as an i3 floating + sticky window (see setup).

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
- **con_id capture assumes the launching terminal is focused** at `run` time (true
  for interactive launches). If a dialog steals focus at that instant, `con_id` is
  recorded as `null` and focus falls back to title matching.
- **Mouse flicker on HUD preview** — with i3's default `mouse_warping output`, each
  preview warps the mouse to the agent's monitor and back. Setting `mouse_warping
  none` in i3 removes it, but that's a *global* i3 behavior change (undecided).
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
