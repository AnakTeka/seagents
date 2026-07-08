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
seagents status            one-shot table of every tracked agent's state
seagents focus <child_pid> focus that agent's i3 window
seagents debug-screen <raw_file> [cols] [rows]   dev tool: render a .raw + print is_blocked verdict
```

HUD keys: `j`/`k` or arrows = navigate + **preview** (reveal the agent window,
keep focus on HUD); `Enter`/left-click = **jump** (commit focus to the agent);
`r` = refresh now; `q`/`Esc` = quit.

---

## Architecture / how it works

Source is small (~5 modules under `src/`):

- **`src/main.rs`** — CLI dispatch; the `run()` pty-tee wrapper; `cache_dir()`,
  `session_paths()`, `write_meta()`, `terminal_size()`.
- **`src/status.rs`** — `State` enum, `Session`, `collect()`, `print()`,
  `read_session()` (detection + reaping), OSC-title parsing (`all_titles`,
  `current_title`, `title_state`), `live_pty_size()`, `read_tail()`.
- **`src/detect.rs`** — `render()` (vt100) and `is_blocked()` (rules ported from
  herdr's `claude.toml`), with unit tests.
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

### 2. Data model — `~/.cache/seagents/`

Per session, keyed by the wrapper's pid:

- **`<wrapper_pid>.raw`** — the teed output byte stream (for detection).
- **`<wrapper_pid>.meta`** — JSON identity:
  ```json
  {
    "wrapper_pid": 1437882,
    "child_pid": 1437883,          // the actual claude process
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

State precedence: **blocked** (from rendered screen) wins over **title-based**
(working/idle) wins over **unknown**.

- **working / idle** come from Claude's **OSC terminal title**, which it broadcasts
  in the raw stream — no terminal emulator needed:
  - braille spinner prefix (U+2800–U+28FF) + space → **working**
  - `✳ ` (U+2733) + space → **idle/done**
  - We take the last *classifiable* title, ignoring transient empty titles.
- **blocked** has no title signal, so we replay the raw tail through the **`vt100`**
  crate to reconstruct the visible screen, then run `is_blocked()` — rules ported
  from herdr's `claude.toml`: a decision prompt (`do you want to proceed?`,
  `enter to select`, MCP `[ ]` checkboxes) confirmed by a cancel footer or a
  selectable option, minus known false-positive screens (transcript viewer, model
  picker). **Rendering is geometry-sensitive** — wrong cols/rows → garbled screen →
  detection silently fails. We read the child's **live** terminal size from
  `/proc/<child_pid>/fd/0` via `TIOCGWINSZ` (falling back to meta, then 80×24).

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
- **Rust** — so we *could* lift herdr's AGPL detector; in the end we wrote our own
  detection, so **there is no herdr code in seagents** (see License below).
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
- **Detection is Claude-Code-specific** — the OSC-title glyphs and `is_blocked`
  rules are tuned for Claude Code. Other agents would need their own rules.

---

## Open items / backlog / next steps

- **Verify** the con_id focus fix live (see Current Status).
- Decide the **mouse_warping** question for the HUD preview.
- **README.md** for the repo landing page (not written yet).
- **License**: `Cargo.toml` says `AGPL-3.0-or-later` from when we planned to reuse
  herdr's code. Since we didn't, AGPL is now *optional* — could relicense to
  MIT/Apache. Decide before making the repo public.
- Sweep **orphan `.raw`** files that have no `.meta` (only pre-`meta`-era leftovers).
- Optional: **eww** port of the HUD for a prettier always-on widget.
- Optional: read **herdr's socket API** to fold herdr-hosted agents into the same
  HUD (the user runs both herdr and seagents).

---

## Dev workflow

```bash
cargo build            # or --release
cargo test             # 9 unit tests (detect rules, title parsing, glyph strip)
cargo run -- status    # etc.

# Tune blocked-detection against a real capture:
seagents debug-screen ~/.cache/seagents/<pid>.raw <cols> <rows>
# get <cols>/<rows> from: stty size < /proc/<child_pid>/fd/0
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
