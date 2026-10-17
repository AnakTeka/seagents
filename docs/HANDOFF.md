# seagents — Handoff

Context document for picking this project up cold (e.g. a fresh Claude session
on another machine). Read this first; it replaces the in-session memory that
does not travel between machines.

---

## TL;DR — what seagents is

**seagents = "see agents": the i3 integration for [herdr](https://github.com/ogulcancelik/herdr).**
Your coding agents (Claude Code, Codex, any agent herdr knows) run inside a
**herdr server**, which owns their terminals, detects their state, and lets
any herdr client show and drive them — including `herdr --remote ssh <this
machine>` from another computer. seagents gives each agent its **own i3
window** (`seagents run claude` starts the agent in herdr and turns the
current terminal into a full-window view of it, through `herdr terminal
attach`), a floating **HUD** of every agent's state (blocked / done /
working / idle) that jumps to the agent's window, and **orchestration** —
agents spawning, waiting on, driving and reading other agents.

The core value: **at a glance, see which agent is blocked and waiting on you,
and jump straight to it** — on this machine, in i3 windows; from any other,
through herdr.

Until 2026-10-12 seagents ran each agent itself, on a pty of its own inside
the window (a "pty-tee" wrapper), which made the window the only way in:
nothing could reach an agent from another machine.
[`HERDR-BACKEND-SPEC.md`](./HERDR-BACKEND-SPEC.md) is the design of the move
to herdr, with every decision and why.

---

## Current status

The herdr backend is merged into `main` (2026-10-12):

| capability | command | state |
|---|---|---|
| start an agent in herdr, shown in this window | `seagents run <agent>` | done (§1): one workspace per directory, one tab per agent; window and agent live and die together; a detach is undone |
| status | `seagents status` | done (§3): herdr's detection fused with the hook turn ledger (§6) |
| focus the agent's i3 window | `seagents focus <target>` | done (§4) |
| floating HUD | `seagents hud` | done (§5): j/k preview + click-to-focus; tags; send/stop keys |
| reboot survival | `seagents snapshot` / `restore` | done (§6) |
| start an agent on a prompt | `seagents spawn` | done (§7) |
| wait for a turn | `seagents wait` | done (§8): turn / idle / timeout / blocked / exit / interrupted / no_hooks |
| drive an agent | `seagents send` / `stop` | done (§9): through herdr; submit-when-idle, the input-box check |
| read the conversation | `seagents read` | done (§10) |
| agents | Claude Code, Codex | both verified live through herdr 0.9.0 (Claude 2.1.28x, codex-cli 0.152.0) |

Verified end to end in a nested desktop by `tools/test-orchestration.sh
alacritty --codex` and `tools/test-restore.sh` (all three terminals), and by
hand against an isolated herdr session (§ Dev workflow). There is no lineage:
any agent may drive any agent (decided 2026-10-12, HERDR-BACKEND-SPEC). The
agent-facing workflow is the skill, `skills/seagents-orchestrate/SKILL.md`.

**Upgrading from a pre-herdr build.** Agents started by the pre-herdr binary
are not seen by this one: restart them after installing, then delete what they
left in `~/.cache/seagents/` (files named `<pid>.*`), which this binary ignores.

---

## Getting set up on a new machine

`target/` and the `herdr` reference clone are gitignored, so after cloning you
build fresh, from the checkout:

```bash
cargo build --release
install -Dm755 target/release/seagents ~/.local/bin/seagents   # ensure ~/.local/bin is on PATH
```

Runtime deps on the machine: **herdr ≥ 0.9.0** (`~/.local/bin/herdr` or on
`PATH`), **i3wm**, **i3-msg** (ships with i3), a terminal emulator. seagents
starts a herdr server when none is running (`herdr::ensure_server`; herdr's
own CLI does not). **`xdotool`** is optional but worth having: it is what lets
a one-process-per-window terminal identify an agent's window exactly (§4);
without it seagents falls back to the correlation paths.

Auto-track every agent by aliasing `claude` (zsh):

```zsh
alias claude='seagents run claude'
```

The alias also works inside herdr's own panes: herdr starts an agent by typing
`claude …` into the pane's shell, which the alias turns into `seagents run
claude`, and `run` inside a herdr pane (`HERDR_ENV=1`) just execs the command
(§1) — so herdr finds the agent itself in its pane.

From another machine: `herdr --remote ssh <this machine>` shows every agent
seagents started (workspace = directory, tab = agent) and drives them; the i3
windows here keep showing them.

Add the HUD keybind + float rule to your i3 config (`~/.config/i3/config`).
`$mod+o` is only an example — pick any free key:

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

**The orchestration skill.** `skills/seagents-orchestrate/SKILL.md` teaches an
agent the spawn → wait → send → read → stop loop (§7–§10) from the CLI as
built. Claude Code loads skills from `~/.claude/skills/<name>/SKILL.md` (or
`$CLAUDE_CONFIG_DIR/skills/` when that is set), so link
the directory rather than copying it, and the skill follows the repo:

```bash
ln -s "$PWD/skills/seagents-orchestrate" "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills/seagents-orchestrate"
```

It states flags, JSON shapes and exit codes, so a change to any of those
changes the skill in the same commit.

---

## Commands

```
seagents [hud]              floating agent HUD (also the default with no args)
seagents run [--tag T] [--seed-file F] <agent> [args]
                           start the agent in herdr and show it in this
                           window (this is how tracking happens); every agent
                           gets a tag (below) (§1)
seagents spawn [--cwd DIR] [--tag T] [--agent claude] [--permission-mode M]
               [--worktree] [--arg A]... (--prompt-file F | [--] PROMPT)
                           open a new terminal running an agent on a prompt;
                           prints {"tag","pid","pane_id","cwd","stops"} once
                           herdr has it up (§7)
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
                           closing its herdr pane, then SIGKILL (§9)
seagents read (<target> | --transcript PATH) [--json] [--from CURSOR]
              [--tail N] [--raw]
                           the conversation's main line from the agent's
                           transcript, collapsed text or NDJSON records,
                           ending with a cursor to continue from (§10)
seagents status [--json] [--sort=urgency|dir|recent]
                           one-shot table of every tracked agent's state
                           (--json: machine-readable, for eww/widget frontends,
                            incl. each agent's conversation `title`, `tag`,
                            `pid`, `pane_id`, `herdr_state`, and from its
                            hooks `stops`, `in_turn`, `transcript_path`,
                            `session_id`, `hooks_ok` (null: no hooks; false:
                            silent), and `capabilities` derived from its
                            adapter; sorts: urgency = triage, longest-waiting
                            first; dir = grouped by cwd; recent = latest
                            change first)
seagents focus <target>    focus that agent's i3 window (target: `tag`,
                           `@tag`, or the `pid` status --json reports)
seagents snapshot [--print]
                           capture the agent layout now (it is also captured
                           continuously)
seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]
                           rebuild that layout after a reboot, each window
                           resuming its own conversation
```

`seagents --help` and `seagents <command> --help` are generated from the clap
definition in `main.rs`, so they are the authoritative reference. Every flag
takes both `--flag value` and `--flag=value`. `run` passes everything after
the agent's first word through untouched (`seagents run claude --model opus
--help` hands `--help` to claude); `--tag` is only `run`'s own when it comes
before the command. Usage errors exit 2.

HUD keys: `j`/`k` or arrows = navigate + **preview** (reveal the agent window,
keep focus on HUD); `g`/`G` = first/last row (vim-style, also previews);
`Enter`/left-click = **jump** (commit focus to the agent);
`s` = cycle sort (urgency → dir → recent; remembered across launches);
`t` = cycle the name columns — **both** (directory + title, the default) →
**title** only → **dir** only (remembered across launches);
`p` = send the selected agent a prompt (a one-line input in the footer; Enter
sends, Esc cancels); `x` = stop the selected agent (asks y/n);
`?` = the full key list, as an overlay (the footer hint is one line and a
floating window clips it); `r` = refresh now; `q`/`Esc` = quit.

While the `?` overlay is up it owns the keyboard: any key but `q` dismisses it
rather than acting, so a keystroke aimed at the overlay can't navigate the list
underneath. It draws over the agent list (no room for both in a floating
window), and if the window is too short for all eleven bindings the bottom border
says `▼ n more` — otherwise `q / esc` would silently fall off the end. The send
line and the stop confirmation own the keyboard the same way (`q` there is a
letter).

In an agent's window, **Ctrl+B is herdr's attach prefix** (fixed in herdr
0.9.0): Ctrl+B Ctrl+B sends one Ctrl+B (Claude's "background this command"),
and Ctrl+B q — herdr's detach — is undone at once (§1).

---

## Architecture / how it works

Source layout under `src/`:

- **`src/main.rs`** — the command line only: the clap-derived `Cli`/`Command`
  and `dispatch()` to the module that owns each command.
- **`src/herdr.rs`** — **the one place seagents talks to herdr**: every call
  is the `herdr` CLI (herdr 0.9.0's), its JSON parsed here and nowhere else.
  `bin()` (`HERDR_BIN_PATH`, `PATH`, `~/.local/bin/herdr`), `kind_for()` (an
  argv0 → herdr agent kind: the adapter key), `server_running()` /
  `server_socket()` / `ensure_server()`, `agents()` / `agent_on(terminal)` /
  `panes()` / `pane_of(terminal)` / `tabs()`,
  `create_workspace()` / `create_tab()` / `rename_tab()`, `start_agent()`,
  `process_info()`, `attach()`, `prompt()` / `send_text()` / `send_keys()` /
  `run_line()`, `read()`, `blocked_rule()` (`agent explain`), `close_pane()`.
  The CLI rather than the socket API because the CLI is the surface herdr
  keeps stable (it checks the server's protocol itself). Which server is the
  environment's business, as for any `herdr` command: `HERDR_SOCKET_PATH`,
  else `HERDR_SESSION`, else the default.
- **`src/run.rs`** — `seagents run` (§1): the in-pane exec, `place()` (the
  agent's workspace and tab), `clean_shell()`, `start()` (the agent up, and
  its pid), `identify_window()`, `attach_loop()` (D1: a detach undone, a
  takeover respected, a closed window ending the agent, the agent's exit
  ending the attach) over `Watch` (one `poll()` on pidfds, a signal pipe and
  the tty), the `monitor()` thread, `deliver_seed()`, `notify_transition()`.
- **`src/screen.rs`** — what seagents reads off an agent's screen itself, as
  herdr hands it over: `input_box()` (what the input box holds — dim text is
  a placeholder, §9) and `shows_any()`.
- **`src/send.rs`** — `seagents send` / `stop` (§9): `submit()` (the gate,
  the input-box check `make_room()`, by-reference bodies, the landing wait),
  `stop_session()`, the `Reply`/`Refusal` → exit codes; `prompt_target()` /
  `stop_target()` for the HUD.
- **`src/hook.rs`** — the agent's lifecycle hooks (§6): `inject()` (the one
  merged hook flag `run` hands a hook-capable agent — Claude's `--settings`,
  as a file path; Codex's trusted `-c hooks=…` — per the adapter's `[hooks]
  inject`) and `run()`, the hidden `__hook` command that records each event in
  `.agent`, finding its session from `HERDR_PANE_ID`/`HERDR_SOCKET_PATH`.
- **`src/registry.rs`** — the one owner of the session cache (§2): `cache_dir()`,
  sidecar paths (`Files`), the typed `Meta` / `StateRecord` / `AgentRecord`,
  `Session::alive()` (the liveness rule), `list()` (live, never deletes) /
  `reap()` (finishes the dead) / `finish()` (tombstone, then every file of
  the session) / `resolve_exited()` + `final_agent()` (an agent that may have
  exited, for `wait`/`read`) / `by_key()` / `by_pane()` (the hook's lookup),
  the locked `update_meta()` / `update_state()` / `update_agent()`,
  `send_lock()`; tags (`valid_tag()`, `claim_tag()` →
  `TagClaim::create_meta()`, the only way a `.meta` is born; `pick_tag()` for
  `spawn`), `runtime_dir()`, `hooks_file()`, and `resolve_target()`, the one
  resolver every command that addresses an agent uses; a session's prompt
  files (`write_prompt()` / `remove_prompts()`) and `exclude_seagents()`, the
  one way `.seagents/` gets into a repo's `info/exclude`. No other module
  builds a session path or parses one.
- **`src/turn.rs`** — `Ledger::in_turn()`, the one definition of "this agent
  is mid-turn" (§6), per adapter: the hook ledger plus the transcript's
  interrupt record and the dropped-turn rule, or the screen for hookless
  agents.
- **`src/transcript/`** — the agent's own conversation file (§10), the
  shared layer `read` and the future sessions browser read through:
  `model.rs` (the common record model), `claude.rs` (the one Claude JSONL
  parser: a line's cheap `head()` for the index, its `records()`, the
  subagent sidecars, and `interrupted_since()` for the turn ledger),
  `codex.rs` (the same four for a Codex rollout, plus `sniff()`, which
  `Format::detect` uses for `read --transcript`),
  `index.rs` (the lazy, incrementally extended offset index cached under
  `sessions/`), `mainline.rs` (the tree, the main line, `resume()` for a
  cursor), `render.rs` (the collapse rules); `interrupted_since()` and
  `responded_since()` in `mod.rs` are the turn ledger's two questions of it;
  `Transcript` there ties them together. `Format` names a parser; the
  adapter's `[read] transcript` picks it.
- **`src/read.rs`** — `seagents read`: the source (a target's reported
  transcript, or `--transcript`), the cursor and its token, continuing
  after one (`rewound`/`reset`), text and NDJSON output.
- **`src/status.rs`** — `State` enum, `Session`, `collect()` (reaps, then one
  `herdr agent list`), `print()`, `read_session()`, `agent_status()` (herdr's
  status corrected for the dialogs herdr misses — the one place anything asks
  whether an agent waits at a dialog), `settle()` (screen fused with the turn
  ledger), `observe_state()` (the `.state` transition rules), `track()` (the
  monitor's pass), `conversation_title()`.
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
  --tag <tag> [--seed-file <f>] -- <argv>`, so the new window's PATH doesn't
  matter; `Run::keep_shell` is the one difference between its two callers —
  `restore` leaves the window at a shell when the agent exits (`; exec
  "$SHELL"`), `spawn` closes it with the agent. The window starts as a user's
  shell would: the calling agent's own session variables (every bundled
  adapter's `[launch] session_env` — Claude's `CLAUDECODE`,
  `CLAUDE_CODE_SESSION_ID`, messaging socket and token, … but never
  `CLAUDE_CONFIG_DIR` or other user config) and the herdr pane it runs in
  (`herdr::PANE_ENV`: `HERDR_ENV`, `HERDR_PANE_ID`, … — with them the new
  window's `run` would take itself for an agent herdr started and exec
  untracked) are removed from the terminal's environment *and* `unset` at the
  head of its `sh -c` line, since a daemon-hosted window (`alacritty msg`)
  gets the daemon's environment, not ours. Inherited, the session variables
  made a Claude started outside tmux never write its transcript (bisected in
  Phase 2). The caller's window ids (`window::WINDOW_ENV`: `WINDOWID`,
  `ALACRITTY_WINDOW_ID`, `TERMINATOR_UUID`) are removed from the terminal's
  environment too — a terminal that sets none of its own would hand the
  caller's to `run`, which would take the caller's window for its own — but
  not `unset` in the new window, where they are already its own.
- **`src/wait.rs`** — `seagents wait` (§8): `decide()`, the whole reason
  logic as a pure function over one look at the ledger, herdr's dialog
  verdict and liveness; `parse_duration()`.
- **`src/spawn.rs`** — `seagents spawn` (§7): prompt checks, `--permission-mode`
  to argv, the tag, the seed file, `--worktree`, the launch, and the wait for
  the agent to start.
- **`src/adapter.rs`** + **`src/adapters/*.toml`** — per-agent orchestration
  facts as data: the settings flag and
  how it is injected (an adapter with hooks is one whose agent reports its
  conversation and keeps the exact turn ledger), the permission modes and
  each one's argv, which variables are the calling agent's session
  (`session_env`) and which configuration paths the pane is given
  (`pass_env`), the input box, the exit sequence, the dialogs herdr misses
  (`[screen] dialogs`). Keyed by the herdr agent kind of `argv[0]`
  (`herdr::kind_for`); `~/.config/seagents/agents/<agent>.toml` replaces the
  bundled file whole (an unparseable override is an error, not a silent
  default); other herdr kinds get the generic adapter.
  Bundled: `claude` and `codex` (§11). `run` reads `[hooks]` (how — `inject =
  "settings" | "config"` — the flag's spellings, the events,
  `hookless_flags`) and `pass_env`; `restore` reads
  `[restore]` (the argv that resumes a conversation, and the stale words it
  replaces); `spawn` reads `permission_argv`; `capabilities()` is what `status
  --json` reports per agent; `send` reads `[send]` (when a body goes by
  reference: `long_prompt`, `inline_max_chars`, `inline_max_lines`, and the
  input box: `input_glyph`, `input_clear` as herdr key names); `stop` reads
  `[stop] exit_sequence`; `[read] transcript` names the transcript format
  (`claude`, `codex`) that `read` and the turn ledger's interrupt check parse.
- **`src/i3.rs`** — the only `i3-msg` caller: `tree()`, `windows()`,
  `focused_node()`, `focus()`, `msg()`, `active_outputs()`, `quote()`.
- **`src/procfs.rs`** — `/proc` access: `alive()`, `parent_pid()`,
  `ancestors()` (the whole chain to init, or None), `boot_time()`.
- **`src/fsutil.rs`** — `write_atomic()`, the one tmp+rename writer every
  cache and state file goes through (`<path>.<pid>.tmp`, removed on failure);
  `create_private()`, `private_dir()`, `config_dir()` / `state_dir()` (XDG),
  `read_tail()`.
- **`src/clock.rs`** — `unix_now()` (seconds: `.meta`, `.state`, the
  snapshot) and `unix_now_ms()` (the `.agent` turn ledger).
- **`src/hud.rs`** — the ratatui HUD, event loop, preview-on-navigation.
- **`src/snapshot.rs`** — the reboot snapshot, layout capture/restore only:
  `capture()` (conversation ids read from each session's `.agent`), layout
  emission with i3 swallow criteria, `restore_cmd()` (which
  launches through `launch`).

### 1. `seagents run` — the agent in herdr, shown in this window

`seagents run claude …` in an i3 terminal:

1. **Inside a herdr pane** (`HERDR_ENV=1` and `HERDR_PANE_ID` set) it is not
   the launcher: it execs the command as it is (same pid, so herdr sees
   `claude` in its pane). This is the alias case — herdr's `agent start`
   types `claude …` into the pane's shell — and a `seagents run` typed into a
   pane of herdr's TUI. A `--seed-file` is then deleted and dropped.
2. The command must be an agent herdr can start (`herdr::kind_for`: claude,
   codex, gemini, opencode, … 23 kinds in 0.9.0), else exit 2.
3. `herdr::ensure_server()`: a herdr server for this environment, started if
   none is — `herdr server`, detached into its own session, without the
   calling agent's session variables, the herdr pane variables or the
   window's id variables (`window::WINDOW_ENV`: a server would hand them all
   to every pane — a server started from inside an agent gave every pane its
   `CLAUDECODE` & co.). herdr's CLI does not start a server itself.
4. The tag is claimed under `tags.lock` (§2), held through the next step.
5. **`place()`**: the agent's place in herdr is **a tab of its own, labelled
   with its tag, in the workspace of its launch directory** (decided
   2026-10-12) — a new workspace, labelled with the directory's name, when no
   pane is at that cwd. A tab already labelled with the tag, holding one pane
   and no agent, is taken instead, in any workspace at that cwd: herdr brings
   tabs back after a server restart (measured: and one that had been closed,
   too), and `restore` relaunches an agent under its old tag. A new tab's
   shell is given the adapter's `pass_env` (`CLAUDE_CONFIG_DIR`,
   `CODEX_HOME`) and `SEAGENTS_DEBUG` from `run`'s environment (`--env`):
   **the agent runs under the herdr server's environment, not `run`'s.**
6. Hooks injected (§6) — Claude's merged settings go to
   `$XDG_RUNTIME_DIR/seagents/hooks-<key>.json` (0600) and are passed as
   `--settings <path>`, because herdr types the command line into the pane's
   shell, where inline JSON would show and land in its history; Codex's `-c
   hooks=…` stays inline. The `.meta` is written **before** the agent starts
   — its first hook fires before `agent start` returns, and finds its session
   by it — and before the hook settings file, so whoever finishes the session
   finds that too. The window's title is set to the tag then (see "The window
   title" below).
7. `starting <tag>…` is printed; `clean_shell()` unsets any calling-session
   variable the pane's shell already holds (a server someone else started
   from inside an agent); then, once the pane's foreground is its shell
   again (herdr refuses `agent start` with `agent_pane_busy` while anything
   else is — a shell still running its rc files, or that `unset`; its CLI
   retries only 2 s), `herdr agent start <tag> --kind <kind> --pane <p>
   --timeout 120000 -- <args>`, retried on `agent_pane_busy` for up to 15 s
   (verified with an rc file running `sleep 4`). `agent_not_ready` (up, and stopped at a
   dialog on the way — a folder-trust prompt) counts as started: the window is
   where it is answered. Then the agent's pid: the pane's foreground process
   group, once it is no longer the shell's — herdr has detected the agent by
   then, so this is immediate; up to 5 s is allowed. The group's leader is
   the process the pane's shell forked for the command herdr typed, the
   agent itself (the `claude` → `seagents run claude` alias execs it in
   place), so it exits when the agent does. Any failure — herdr's, or no pid
   — closes the pane, finishes the session and exits 1 with the message. Ctrl+C or the window closing
   while it starts cancels it. After that, only the window closing (SIGHUP,
   SIGTERM, or the tty gone) or the agent exiting ends the session: Ctrl+C reaches the agent
   through the raw attach, and in a window that only waits (taken over, below)
   it does nothing (verified).
8. The agent's pid goes into `.meta`, and a pidfd is opened on it (Linux ≥
   5.3); the window is identified on a thread (§4); the **monitor** thread starts;
   a seed (from `spawn`) is delivered by its own thread once the agent has
   read idle for 1.5 s (§7).
9. **The attach loop** (`attach_loop()`): `herdr terminal attach <terminal
   id>` as a child, stdio inherited (its stderr piped, to read why it ended).
   The window shows the agent alone, full-size. `run` then sleeps in one
   `poll()` (`run::Watch`) on: a pidfd for the attach, the agent's pidfd, a
   self-pipe that SIGHUP and SIGTERM write to (`signal_hook::low_level::pipe`;
   not SIGINT), and stdin's `POLLHUP` — no timers. Then:
   - **Window closed** — SIGHUP, or stdin polls as hung up — the agent's pane
     is closed, which ends the agent: **the window and its agent live and die
     together** (decided 2026-10-12).
   - **A detach** (Ctrl+B q; attach exits 0, the window still there) is
     **undone**: the attach is started again at once (decision D1), so the
     window never sits empty over a running agent.
   - **Taken over** by another client (`herdr terminal attach --takeover`;
     attach exits 1, `terminal attach taken over`): the window says so and
     waits — it never takes the agent back, and never uses `--takeover`
     itself. Closing it still ends the agent; Ctrl+C in it does not. herdr's normal remote view
     (`herdr --remote`) is a different kind of client and takes nothing over
     (verified 2026-10-12: the laptop window and the PC both showed and drove
     one agent).
   - **Attach failing** 3 times within 10 s ends the session with its error;
     the 1 s pause between retries is the same `poll()` with a timeout, so
     the agent exiting (an attach that failed because its pane just closed)
     or the window closing cuts it short.
   - **The agent exiting** (`/exit`, a crash, `stop`): its pidfd fires; `run`
     decides "agent gone" first, then SIGTERMs the attach — herdr's attach
     takes SIGTERM like a detach (exit 0, terminal restored, ~5–40 ms), so
     the decision must come first or it would be re-attached — waits for it,
     closes the pane and exits: back at the shell for a window opened by
     hand, the window closed for a spawned one. Measured 2026-10-16 in a pty
     against an isolated server: 22–38 ms from a SIGKILLed Claude to `run`
     gone, 11–18 ms from a gracefully exited one; an `echo` typed 20 ms
     after the kill ran in the window's own shell. (Before, the window went
     on showing herdr's pane shell until the monitor's 2 s tick closed the
     pane: a Ctrl+D typed then closed that shell and left the window's own
     needing another, and an arrow key could land split.) A key typed in the
     few ms before the attach has exited still goes to the pane's shell.
     A pidfd does not fire on a stopped process, so Ctrl+Z (below) ends
     nothing.

   On the way out `run` closes the pane if it is still there and `finish()`es
   the session: a tombstone, then every file, its prompt files and its hook
   settings.

**The monitor** (`run::monitor`, every 2 s): the **window title**
(below); one `herdr agent list` →
`status::track` (herdr's verdict fused with the ledger, into `.state`, §3);
desktop notifications on transitions; the done marker cleared when the
agent's window is focused; the **screen clock** the dropped-turn rule needs
(§6) — while the agent sits idle in a turn, it hashes the screen (`herdr pane
read`) and stamps `.state`'s `screen_changed_ms` when it changes; and the
reboot snapshot offer. The agent's exit is not its business (`Watch` has
it, above): it holds a lock through each tick that `run` takes, and marks
ended, before it finishes the session, so no tick writes into a finished one.

**The window title.** herdr's `terminal attach` client never passes the
agent's own title (OSC 0/2) on to the outer terminal — only herdr's full UI
shows it — so without help the i3 window kept the shell's title (zsh's
preexec: `seagents run claude …`). `run` sets it itself (`run::set_title`):
the tag as soon as the `.meta` is written, before the start and the attach,
then each monitor tick the agent's title as herdr reports it
(`terminal_title_stripped`: Claude's conversation name, its state glyph
already stripped by herdr) whenever it changes — the tag while the agent has
none. One `ESC ] 2 ; <title> BEL` in a single `write()` to `/dev/tty`, with
control characters (ESC, BEL, C1 such as ST 0x9c) dropped from the title.
Verified 2026-10-12 in a nested alacritty against an isolated herdr server:
the title went `tt` → `Claude Code` → `TITLES exact reply test` after the
first turn, and the attach screen stayed intact.

**Desktop notifications on transitions.** The monitor fires `notify-send` when
its agent finishes (working → idle, normal urgency) or starts waiting on input
(→ blocked, critical urgency) — suppressed while the agent's own i3 window is
focused. `SEAGENTS_NO_NOTIFY=1` disables.

**Job control**: Ctrl+Z goes through the attach to the agent, which stops in
its herdr pane — the pane's shell shows, `fg` there resumes it. While it is
suspended herdr does not list it as an agent (status `unknown`); its pid is
alive, and a pidfd fires only on exit, so nothing ends (verified
2026-10-16: stopped, `run` stayed, `fg` resumed it). The old wrapper's `__lead` anchor and its job-control
forwarding are gone with the wrapper.

**The mouse in a window**: herdr's direct attach captures the mouse by
default (`ui.mouse_capture`), whatever runs in the terminal — so over the
pane's shell (an agent suspended with Ctrl+Z) the window could not select
text. The attach therefore reads its own config, `herdr::attach_config`: the
user's herdr config with `ui.mouse_capture = false`, rewritten to
`<runtime dir>/herdr-attach.toml` on every attach and passed as
`HERDR_CONFIG_PATH`. The mouse still reaches a program that asks for it
(Claude does: its own selection and scrolling work). The user's config is
left alone, so herdr's full UI (`herdr`, `herdr --remote`) keeps its mouse —
clicking tabs and spaces needs capture on (verified 2026-10-16).

**A server restart** ends every agent (their terminals were the server's):
each `run`'s pidfd on its agent fires and it ends. herdr brings the tabs back
empty; `restore` reuses them (§6).

### 2. Data model — `~/.cache/seagents/`

**Private.** The cache holds the last prompt seagents sent each agent
(`.meta`), its last message (`.agent`, the tombstones) and where its
conversations are, so the directory is 0700 and every file in it is created
0600 — the sidecars and tombstones (`fsutil::write_atomic`), the locks,
`hook.log`, and `sessions/` (0700) with its indexes. The state dir
(`~/.local/state/seagents`: the layout, HUD prefs) is 0700 too, its layout
and prefs 0600. One helper decides every such directory,
`fsutil::private_dir`: made 0700, then `lstat`ed — a real directory owned by
us, a loosened mode tightened. Under `HOME` a symlink or someone else's
directory is left as the user arranged it; in `/tmp` it is refused. With no
`HOME` the cache is `/tmp/seagents-<uid>/cache` (inside the checked runtime
fallback, below), and where that is refused, a path nothing can be written
under.

Per session, keyed by the **herdr terminal id** of the agent's pane
(`term_65d9…`): herdr's own name for it, fixed for its life (a pane moved to
another workspace gets a new pane id; its terminal keeps its id) and unique
across herdr servers.

- **`<key>.meta`** — JSON identity:
  ```json
  {
    "key": "term_65d9c80ddc3621",
    "pane_id": "w1:p1",            // the pane it started in (its hooks' HERDR_PANE_ID)
    "socket": "/home/me/.config/herdr/herdr.sock", // the server (its hooks' HERDR_SOCKET_PATH)
    "run_pid": 305301,             // the `run` holding the window
    "pid": 305358,                 // the agent (its pane's foreground group); null while starting
    "cwd": "/home/me/projects/myapp", // launch cwd = grouping key
    "argv": ["claude", "--model", "haiku"],
    "started": 1783468316,
    "con_id": 103471613296720,     // i3 window recorded at launch (focus handle)
    "window_id": 30728026,         // X11 id (survives an i3 restart)
    "tag": "myapp-3f9a",           // the agent's name, and its herdr agent name
    "hooks": true,                 // run handed the agent our hooks (absent: false)
    "engaged_at": 1783468320,      // first prompt given (a seed, or a send)
    "last_prompt": "…",            // the last prompt seagents submitted (§9)
    "submitted_ms": 1783468390100  // when (Unix ms): still landing until its hook fires
  }
  ```
  `argv` is the user's command line — never what `run` injected (§6) —
  because `restore` relaunches from it. Parsed as a typed record with every
  field optional, and fields this binary doesn't know carried through
  untouched — a `run` outlasts a reinstall. A `.meta` with no `key` is not a
  session. Which adapter drives the agent follows from `argv[0]`
  (`herdr::kind_for`).
- **`<key>.state`** — last observed state transition (`{"state": "idle",
  "since": <unix>, "seen": bool}`, plus `"rule"` — the dialog's rule id,
  e.g. `live_blocked_form` — while `blocked`, `"screen"` — herdr's own
  verdict when the ledger overrode it, e.g. `working` with `screen: idle`
  mid-turn — and `"screen_changed_ms"`, the monitor's screen clock, §6);
  powers the "idle for / waiting for" column and the **done** state: a
  working→idle completion is marked `seen: false` (shown as 🔵 done, ranked
  just under blocked) until acknowledged — by a HUD preview/jump of that
  agent, or by the agent's window gaining focus — which flips it to plain
  idle without touching the clock. Whoever observes the working→idle edge
  marks it — the monitor or a reader (the HUD, `status`). herdr keeps no
  time and its own "done"/seen marker is per server and cleared only by
  `herdr agent focus` (which would move a remote client's view), so both
  stay seagents'. Transition-based on purpose: output age can't work
  because a focused Claude repaints on the terminal's focus-in report.
- **`<key>.agent`** — what the agent reports about itself, written by
  `__hook` from its own lifecycle events (§6); absent for agents without
  hooks:
  ```json
  {
    "transcript_path": "/home/me/.claude/projects/…/1f81….jsonl",
    "session_id": "1f81…",          // the conversation, for restore's --resume
    "stops": 7,                     // +1 at every Stop; never resets
    "prompted_at": 1783468390120,   // last UserPromptSubmit (Unix ms)
    "stopped_at": 1783468399870,    // last Stop (Unix ms)
    "last_message": "…",            // the last turn's final assistant text
    "updated": 1783468399870
  }
  ```
- **`<key>.lock`** — empty; `flock`ed around every change to `.meta`,
  `.state` or `.agent`.
- **`<key>.send.lock`** — held across one whole `send` or `stop`
  (`registry::send_lock`, §9). Its own file because a `send` waits for a hook
  that takes `.lock`.
- **`tags.lock`** — one for the whole cache; see Tags.
- **`<tag>.exited`** — a tombstone: what `finish()` keeps of a session
  (`{tag, key, exited_at, agent, cwd, stops, last_message, transcript_path,
  session_id}`), written before anything is removed, so `wait` and `read`
  still answer for an agent that exited (§8, §10). Not written while a live
  agent holds the tag; removed when a new agent claims it and by `reap` after
  24 h. `registry::resolve_exited()` is the resolver that sees them (live,
  then a dead unfinished session by that tag, then the tombstone) and
  `registry::final_agent()` the ledger read that falls back to one; `send`,
  `stop` and `focus` use `resolve_target()`, live agents only.
- **`<agent cwd>/.seagents/prompts/<key>-<stops>-<4 hex>.md`** — a body sent
  by reference (§9) and every seed (§7), 0600. Outside the cache, but the
  session's: named and deleted by `registry`.
- **`sessions/<session id>.idx`** — a transcript's offset index (§10),
  about a conversation rather than a live agent, so never removed with a
  session (swept daily instead, §10). `registry::sessions_dir()`.
- **`$XDG_RUNTIME_DIR/seagents/`** (`registry::runtime_dir()`; else
  `/tmp/seagents-<uid>`, used only if `lstat` says it is a real directory
  owned by us) — `seed-<tag>` (`spawn` → `run`, deleted by `run` first
  thing) and `hooks-<key>.json` (Claude's merged settings, removed with the
  session).

**Liveness** (`Session::alive`): the `run` holding the window is alive, and
so is the agent — or it has no pid yet and started under 180 s ago (`agent
start` waits up to 120 s). `list()` applies it and never deletes. A caller
that holds a session across time judges it on the `.meta` as it is now
(`Session::alive_now`: `wait`, `send`), not the one it resolved — which may
predate the agent's pid. `reap()` (from `status::collect`, so `status` and
the HUD) `finish()`es every session **whose `run` is gone**; a session whose
`run` is alive is never touched, ended agent or not — `run` owns its own
ending (closes the pane, the window, then finishes). (Reaping on the agent's
pid alone once deleted the `.meta` of an agent that had just `/exit`ed, from
under its `run`, while an open HUD reaped every 750 ms: the window stayed on
a bare shell.) One whose `run` is gone while its agent still runs in herdr (a
SIGKILLed `run`) has its pane closed first: window and agent die together. It also sweeps orphan sidecars (`term_*.state|agent|lock|send.lock`
with no `.meta`). Nothing but a session's own `run` and `reap` (once that
`run` is gone) ever finishes a session, so a `.meta` never disappears from
under a live `run`.

**Tags.** Every agent has one: its name for every command (`focus reviewer`),
the window handle `restore` swallows on (`seagents-<tag>`), and its **herdr
agent name** (`herdr agent list` shows it; herdr's rule for names is the
tag rule). `run --tag T` asks for one; otherwise `run` mints `<cwd
basename>-<4 hex>`. Charset `[a-z][a-z0-9_-]{0,31}` — a leading letter, which
herdr requires and which keeps a bare target from reading as a pid; a
directory whose name starts with anything else mints `d<name>-<hex>`
(`20260612_doc_parser` → `d20260612-doc-parser-3f2a`, decision D4).
`registry::claim_tag()` checks uniqueness
— against live agents and herdr's own agent names — under an exclusive `flock`
on `tags.lock` and keeps it held through `place()` until
`TagClaim::create_meta()` has written the `.meta`, so two concurrent `run`s
cannot both get a name (nor both make a workspace for one directory). A
malformed or taken `--tag` exits 2 before anything is started. Minting also
avoids every tag recorded in the layout snapshots, so a new agent can't take
a placeholder a pending restore will want back; a *requested* tag may be one
of those — that is how `restore` relaunches an agent under its old name.
`spawn` picks its tag the same way (`pick_tag`), without holding it; the
window's `run` claims it.

**Every sidecar goes through `registry.rs`**, because each has writers in
several processes (`.meta`: `run`, window healing from `focus`/the HUD, the
last prompt from `send`; `.agent`: each `__hook` the agent runs). Each change
is `flock` on `.lock` → read → mutate the typed record → write a
pid-suffixed tmp file → rename over → unlock; readers take no lock. Nothing
may write a sidecar once the `.meta` is gone: `update_meta`, `update_agent`
and `update_state` all check for it first.

### 3. `seagents status` — state

**herdr detects the state.** It owns each agent's terminal and classifies its
screen with its per-agent rule manifests (self-updating from herdr.dev,
maintained upstream); `status::collect()` asks for every agent at once —
`herdr agent list` — and matches them to sessions by terminal id. herdr's
`idle`/`done` read as idle (herdr's own done marker is not used, §2),
`working`, `blocked` (with its rule, from `herdr agent explain --json`,
fetched only for a blocked agent), `unknown` (also: herdr does not see the
agent — still starting, or suspended).

**The dialogs herdr misses** (`status::agent_status`, the one place anything
asks): an adapter's `[screen] dialogs` lists screen text that means the agent
waits at a dialog herdr does not read as one — Codex's folder-trust question,
which herdr 0.9.0 reads as idle. While one shows, the agent counts as
`blocked` (rule `seagents_dialog`) to `status`, `wait`, `send`, the seed and
`stop` alike: anything typed there answers it, and an Enter there trusts the
folder (§11). The match (`screen::shows_any`) drops box-drawing characters
and treats any run of whitespace as one space, so a question drawn in a box,
wrapped across rows or overdrawn still matches; and a screen that cannot be
read counts as one — what cannot be checked is not typed into. Costs one
`herdr pane read` per look at an agent whose adapter lists dialogs.

This replaced the vendored copy of herdr's engine (`src/vendor/herdr_detect`,
from herdr v0.7.3, with seagents' patches), deleted 2026-10-12. Its two
patches: Claude's `background_shell_working` re-weight is obsolete (herdr
deleted the rule, `987b070f`, in 0.9.0); Codex's trust-dialog rule became the
`[screen] dialogs` entry above. `herdr agent explain <pane>` shows herdr's
verdict and the rule that matched (what `debug-screen` did).

**Fused with the turn ledger** (§6) by `status::settle`, exactly as before:
for a hook agent, blocked wins; else in a turn → working; else herdr's
verdict.

### 4. `seagents focus` — jump to the agent's i3 window

**Exact handles only — no fuzzy matching.** X11 can't be asked "which window
hosts this pty" (Terminator shares one process across windows, so
`_NET_WM_PID` is useless). `run` records the window's `con_id` + **X window
id** in the meta at launch, on a thread, while the agent starts — strongest
source first:

**Most terminals make correlation unnecessary**, and two paths try those
first:

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

3. **The handle** `restore`/`spawn` put on a window they open
   (`seagents-<tag>`, `window_ids_for_handle`, polled up to 5 s while the
   window maps).
4. **The focused terminal** — right when a human just typed the command into
   it (guarded: only a terminal-class window, never the HUD). Captured while
   the agent starts, not after: a start takes seconds, in which the user may
   move on.

Under alacritty the first two answer directly. Resolution: con_id (`i3-msg
[con_id=N] focus`; fast, dies on an i3 *restart*) → X window id (survives
restarts; heals con_id when used) → a clear error. The pty wrapper also
re-captured the ids on every input burst (typing proved the window focused);
with herdr's attach in the window there is no input pump, so on Terminator a
window identified wrongly at launch stays so (§ Limitations).

### 5. `seagents hud` — the floating panel

ratatui 0.28 TUI, polls `status::collect()` every 750 ms, color-codes rows
(blocked=red, done=cyan, working=yellow, idle=green). Navigation previews (reveal agent
window, then refocus the HUD so you keep navigating); Enter/click commits the jump.
The HUD remembers its own window (captured lazily on first preview) to return focus
to. Meant to run as an i3 floating + sticky window (see setup).

**Rows are named by directory, conversation title, or both (`t`).** The title is
the one Claude generates for the session — the human-readable line
`claude --resume` lists — which it also stamps into the terminal title behind a
state glyph (`✳ Detection update`); herdr reports it per agent
(`terminal_title_stripped`), so surfacing it costs nothing extra: it is
cleaned by `status::conversation_title()` and carried on `Session.title`
(also emitted by `status --json`, for widget frontends). Two consequences worth
knowing:

- A session that hasn't named its conversation yet shows Claude's own banner
  (`Claude Code`), or the command line the shell put in the title before the
  agent set its own (`claude --model haiku`), which say less than the
  directory does — those come back **empty**, and the row falls back to the
  directory. Same for stray punctuation-only titles.
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

**Tags.** The name column shows `@tag` when somebody chose the tag
(`spawn --tag reviewer`) and the directory when `run` minted it
(`registry::minted_from`: the directory's tag base plus four hex digits, which
says nothing the directory doesn't). Spawned agents usually run in their
spawner's directory, so without the tag a fleet reads as N rows all named
`seagents`. Title mode is unchanged: the title names the row. Rows are flat:
there is no lineage (decided 2026-10-12; the HUD's tree went with it).

**Send / stop from the HUD** (`p`, `x`) run `send::prompt_target` /
`send::stop_target` — the code `seagents send`/`stop` run — on a background
thread (a send is held until the agent is idle; a stop takes its grace
periods), and the outcome lands in the footer: `sent to @w · stops 3` (the
`wait --after` baseline), `stopped @w`, or the refusal's own text. The
blocked refusal applies, and `p` on a blocked row says so at once rather than
after the prompt is typed — Enter jumps to the window, where the gate is
answered. There is deliberately no
answer-the-gate key: what `1` picks differs per gate, and approving one
unseen from a list row is the thing the blocked refusal exists to prevent.
The target is fixed when the input opens, since rows move under the urgency
sort. Bracketed paste is on, so a pasted multi-line prompt arrives whole
(newlines show as `↵` in the one-line input); typed prompt text is never
written to `SEAGENTS_HUD_LOG`.

Verified 2026-10-12, headless (a pty harness, an isolated herdr session):
the HUD lists the agents, `x` then `y` stops the selected one, `q` quits. If
no herdr server is running the footer says so at start.

Both view modes persist in `state_dir()` — one word per file (`hud-sort`,
`hud-label`: `urgency|dir|recent`, `both|title|dir`) — so the setting survives
closing the HUD window. The HUD needs a real terminal.

### 6. `seagents snapshot` / `restore` — surviving a reboot

`restore` rebuilds the workspaces your agents were on and relaunches each
window on **its own conversation** (`claude --resume <id>`), so a reboot costs
you the processes but not the layout or the context. Three problems:

**The layout must outlive the processes.** `~/.cache/seagents/` self-reaps — the
first `status` after a boot deletes every `.meta` whose child pid is gone, which
is all of them. So the snapshot lives at `state_dir()/layout.json` and is written
*continuously*: every `run`'s monitor thread offers a capture each tick,
globally throttled by the file's own mtime to one write per 30 s. The HUD's
poll loop and every `status` invocation offer one too.
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

The tag is the agent's own (§2), recorded per agent in `layout.json`.
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
one window, the earliest launch keeps it and the rest are reported unplaced.)

**herdr's own persistence.** A herdr server brings its workspaces and tabs
back when it restarts, empty (the processes are gone) — measured, including a
tab that had been closed. `run` takes such a tab when one at the agent's cwd
is labelled with its tag (`run::place`), so a restore after a herdr restart
does not pile up empty tabs; ones no restore claims stay until closed in
herdr.

**A window must know which conversation it was.** The agent says so itself.
For an agent whose adapter takes hooks (`[hooks] inject`: Claude Code,
Codex), `run` hands the child one
extra flag (Claude: `--settings`; Codex: `-c hooks=…`, §11) carrying three hooks — `SessionStart`, `UserPromptSubmit`, `Stop` — each
running `'<abs path of this binary>' __hook` (absolute, so the agent's `PATH`
doesn't matter). Every payload carries `session_id` and `transcript_path`;
`__hook` finds its session from the environment herdr gives everything in the
agent's pane — `HERDR_PANE_ID` and `HERDR_SOCKET_PATH` — matched against the
`.meta`s (`registry::by_pane`; a pane moved since has a new id, but the agent
keeps the old one in its environment and so does `.meta`), and records the
event in `.agent`. The
snapshot capture copies `.agent`'s `session_id` into the layout, and `restore`
resumes it (`resume_argv`, per the adapter's `[restore]`: the stale
selection words dropped — Claude's `--resume`/`-r`/`--continue`/`-c`, Codex's
`resume`/`fork`/`--last`, each with its id — and the resume argv inserted
right after the command: Claude's `--resume <id>`, Codex's `resume <id>`
subcommand; so before any `--`, after which it would be read as prompt text
and nothing is touched. An adapter with no `resume` restores fresh). Because `/clear` (and compact, and a resume) fires `SessionStart`
again with the new conversation, the id is always the one the window is
writing *now* — the stale-after-`/clear` problem the old resolvers had is gone,
and so is their `~/.claude/projects` hardcoding, which pointed at the wrong
directory under a custom `CLAUDE_CONFIG_DIR` (the hook reports the real
path).

Details that matter:

- **One merged `--settings`, never a second one.** Verified 2026-09-28:
  repeated `--settings` flags *replace* each other (only the last one's hooks
  run), while settings-*file* hooks and flag hooks coexist. So the user's
  settings files are left alone (seagents never writes the user's config), and
  a `--settings X` the user passed (inline JSON or a path, `--settings X` or
  `--settings=X`) is loaded, has our three entries appended to its
  `hooks.<Event>` arrays, and goes back as ONE flag in its place — written to
  `<runtime dir>/hooks-<key>.json` (0600, removed with the session) and passed
  by path (decision D5): herdr starts the agent by typing its command line
  into the pane's shell, where inline JSON would show in the window and land
  in the shell's history. An X that can't be read or isn't a settings object
  passes through untouched and the agent runs without hooks, with a warning
  on stderr. Our flag goes right after `argv[0]`: anything after a `--` is
  the agent's positional text.
- **`claude --bare` skips hooks**, so nothing is injected under it (the
  adapter's `hookless_flags`); `.agent` stays empty.
- **`.meta` `argv` is the user's argv**, without the injected flag: `restore`
  relaunches from it, and a recorded `--settings` would stack on every restore.
- **`__hook` must never change the agent**: it always exits 0 (a `Stop` hook
  exiting 2 *keeps Claude from stopping*), prints nothing on stdout (hook
  stdout can be read as a decision), catches its own panics, and parses any
  arguments without error. It is one directory scan, one small read and one
  rename — no i3, no herdr. Failures go to `~/.cache/seagents/hook.log`
  only under `SEAGENTS_DEBUG` (in the hook's environment, which is the pane's:
  `run` passes `SEAGENTS_DEBUG` on to a new pane). `run` writes the `.meta`
  before it starts the agent, so the first hook finds it.
- **Every event refreshes identity**, not only `SessionStart`: a lost
  `SessionStart` heals at the next prompt.

**The turn ledger** in `.agent` counts `Stop`s; it does not pair prompts with
turns (verified 2026-09-29: a message typed mid-turn fires `UserPromptSubmit`
reusing the running turn's `prompt_id` and is answered by a second `Stop` with a
fresh one; an Esc-interrupted turn fires `UserPromptSubmit` and no `Stop` at
all). Its stamps are **milliseconds** — a prompt in the same second as the last
`Stop` is the common case. `turn::Ledger::in_turn()` is the one definition of
"mid-turn", shared by `status`, `wait` and `send`:

- **With hooks** (Claude, Codex): prompted after the last `Stop`, **and**
  no interrupt record in the transcript since that prompt, **and** not dropped
  before it began (below). On Esc Claude writes a
  `user` record whose text is `[Request interrupted by user]` (or `… for tool
  use]`); `transcript/claude.rs` looks for it in the transcript's last 256 KiB,
  newest first, stopping at the first record older than the prompt (and not
  parsing at all when the marker isn't in the tail). The screen does **not**
  otherwise end a turn: Claude 2.1.284 streams a plain-text answer with no
  spinner and an idle title (the old vendored detector said `idle`
  mid-answer; herdr 0.9.0 reads it working), and a turn Esc'd before the
  detector ever saw it `working` would never have ended.

herdr's own `agent wait` / `agent prompt --wait` were measured against this
(2026-10-12, isolated session) and do not replace it: called right after
`agent prompt` it returned the *previous* idle 0.4 s before `working` showed,
a timeout tells the caller nothing, an Esc'd turn looks finished, and its
`state_change_seq` is one counter for the whole server, reset when it
restarts. So turns, `stops`, `interrupted` and the final message stay ours.
- **Without hooks** nothing reports a prompt, so an agent is never in a turn:
  its state is herdr's reading of the screen alone.

The **state column** for an agent with hooks follows from it (`status::settle`):
`blocked` from the screen wins; else in a turn → `working`; else the screen's
verdict. The fused state is what `.state` records, so the done marker and the
"finished" notification fire when the turn really ends, not when the stream
merely looks idle. `status --json` reports `stops` and `in_turn`.

**A turn dropped before it began.** An Esc that lands **before the model's
first response** (well under a second after submitting; measured on 2.1.284)
doesn't interrupt a turn — Claude puts the prompt back in the input box and
writes neither an interrupt record nor a `Stop`, fires no hook (the binary's
event list has no cancel), and the transcript records after the prompt
(`last-prompt`, `mode`, `permission-mode`, …) are the ones every turn writes
at its start. There is no exact trace, so `turn.rs` reads the absence of
everything a live turn does: herdr's own screen verdict (`.state`'s
`screen`) is idle, the screen has not changed for 10 s since the prompt
(`.state`'s `screen_changed_ms`: `run`'s monitor hashes the screen every tick
while the agent sits idle in a turn — it used to be the tee's mtime), and the
transcript has no assistant record (text, reasoning, tool call) since it
(`transcript::responded_since`). Thinking and a slow first token show a
spinner whose timer repaints every second (working); a streamed answer
changes the screen; a tool call is an assistant record; a permission prompt
is `blocked`. Such a turn then ends — `wait` says `interrupted` (verified
through herdr 2026-10-12: rc 5, 10.8 s after the Esc) and the agent reads
idle. Codex never
meets the rule: it records `turn_aborted` even then, and its turn timer
repaints every second. After a dropped turn the prompt is still in the
input box, and a paste lands after it — the two used to arrive as one
prompt (`…the seaReply with exactly: …`, verified); `send` now clears it
first (§9).

`restore` is safe to run twice and safe to run when only half the agents died:
a restored agent comes back under its captured tag, so **a captured agent whose
tag is live is already on screen and is skipped** — tag identity alone makes
restore idempotent.

### 7. `seagents spawn` — starting an agent on a prompt

The command that starts an agent on a prompt, through the same launcher
`restore` uses. Its primary caller is another agent, but there is **no lineage** (decided 2026-10-12): no parent, no depth, no
fan-out caps, no permission ceiling from the caller, no `--arg` checks. Once
herdr owns the agents an agent can call `herdr` directly, so those checks
only guarded the seagents path; what guards an agent is its own permission
prompts (an agent running `seagents spawn` runs a shell command).

1. **The agent** must be one herdr can start (`--agent`, default `claude`).
2. **The tag** (`registry::pick_tag`): a requested `--tag` must be free of
   live agents, herdr's agent names **and the saved layout(s)** — unlike
   `run --tag`, which restore needs to reuse them; else one is minted. Not
   held: the window's `run` claims it, and refuses it there if another took
   it meanwhile.
3. **`--permission-mode M`** becomes argv through the adapter's
   `[launch.permission_argv]` (Claude: `--permission-mode <m>`; Codex: an
   `-a`/`-s` pair, §11); none asked for, none passed — the tool's configured
   default, as when typing the command.
4. **`--worktree`**: `git worktree add <repo>/.seagents/worktrees/<tag> -b
   seagents/<tag>`, refusing an existing path or branch, with `.seagents/` in
   the repo's local `info/exclude`. Inside the repo because Claude's
   folder-trust dialog blocks every never-trusted directory (any mode), and a
   subdirectory inherits its parent's trust.
5. **The prompt** goes to `<runtime dir>/seed-<tag>` (0600), which `run
   --seed-file` reads and deletes first thing. It reaches the agent **once
   the agent has read idle for 1.5 s** (`run::deliver_seed`) — past any
   dialog it opened on the way up, which is answered in its window — through
   `send::submit`, **always by reference**: written to
   `<cwd>/.seagents/prompts/<key>-0-<hex>.md` (0600, deleted with the
   session) and submitted as the one line telling the agent to read it. A
   prompt submitted through herdr travels on a `herdr` command line, which
   every local user can read in `/proc`; the prompt must not be there. The
   settle time is measured: right after a folder-trust dialog closes Claude
   reads idle while still settling, and the Enter of a prompt pasted then was
   lost (2026-10-12) — `send` also presses Enter once more if the prompt's
   hook does not report it and the box still holds exactly its text (§9).
   A seed that finds the box holding someone's draft, or the agent busy or
   at a dialog, keeps waiting — it is never dropped (verified: held behind a
   draft, delivered once it was cleared). The seed thread prints nothing (the
   window's terminal is the agent's screen); with `SEAGENTS_DEBUG` it logs to
   `~/.cache/seagents/run.log`.
   The cwd must be writable (or `--worktree`), checked before a window opens.
6. **Launch** via `launch::spawn_terminal` with `keep_shell: false`, in the
   terminal class of the caller's own window (found from the herdr pane it
   runs in; the focused one for a human; `SEAGENTS_TERMINAL` overrides), then
   **return** once the agent has started in herdr — its `.meta` has a pid
   (≤ 150 s; ~4 s measured) — with `{"tag","pid","pane_id","cwd","stops":0}`.
   A `.meta` that appears and goes again is a start that failed; the window
   says why. An agent stopped at a dialog on the way up counts as started
   (the seed waits); `wait` reports it `blocked`.

Errors exit 1 with the reason on stderr; stdout carries only the JSON.Errors exit 1 with the reason on stderr; stdout carries only the JSON.

### 8. `seagents wait` — the turn a caller is waiting on

`wait` blocks on the turn ledger, not the screen: polling `.state` for idle
returns at once right after `spawn` (the agent paints an idle input box before
it takes the seed) or right after a prompt (the detector ticks every 2 s), and
never sees a sub-tick turn as working at all. The caller holds a baseline N —
0 from `spawn`, `stops` from `send` — and polls every 250 ms:

| reason | exit | when |
|---|---|---|
| `turn` | 0 | `stops > N`: the turn after N ended; `last_message` is its final text (wins even if the agent has since exited) |
| `idle` | 0 | no `--after`, not in a turn, alive and not at a gate: nothing to wait for, the last result at once |
| `timeout` | 2 | `--timeout` (default 5m; `30s`/`5m`/`1h`/seconds) passed — call again with the same N; nothing is consumed |
| `blocked` | 3 | the agent waits at a dialog — herdr's verdict, or one of the adapter's `[screen] dialogs` (`status::agent_status`, asked at most once a second); `rule` names it |
| `exit` | 4 | the session fails the liveness rule (§2) — also for an ended agent, from its tombstone, for a day |
| `interrupted` | 5 | a turn opened since the last `Stop` is no longer running (Esc, or dropped before its first response, §6): it has no final message. Its own code so `wait && …` doesn't treat an abandoned turn as done |
| `no_hooks` | 6 | the agent was handed hooks and given a prompt ≥ 20 s ago, and none has reported (`.agent` has no `session_id`): no turn will ever be seen to end. `message` names the adapter and `hook.log` |

1 is an error (unknown target; an N ahead of the ledger — a restored agent's
ledger restarts at 0, so wait without `--after`). `--for exit` waits for exit
only. Without `--after`, a running turn is waited on (N = current `stops`);
with no turn running the agent's condition is the answer at once, by the same
rules — `blocked` at a gate (the trust dialog comes before any prompt, so no
turn is open there), `exit` when gone, else `idle`. `wait::decide()` is the
whole table, a pure function over one look.
An exited agent is answered for by the same table: its ledger is final (from
its files while they last, then its tombstone — a waiter that had the session
in hand when a reap came reads the tombstone), so a turn past N is still
`turn`, anything else `exit` with the last `last_message`; `state` is null.
The reported `state` follows the ledger for hook agents (`.state` lags a turn's
end by up to one monitor tick). `wait` only reads: it never acknowledges
`done` and never writes `.state`.

**Hook health** (`hook::health`, the `no_hooks` row): a hook agent whose
hooks never fire — a future Codex hashing hook trust differently from the
copy seagents reproduces, a Claude changing what `--settings` means, a user's
`disableAllHooks` — keeps an empty ledger, and a `wait` would block to its
timeout with no clue. So `run` records in `.meta` that it injected hooks
(`hooks`), and `send` when the agent was first given a prompt (`engaged_at`:
a submitted `send`, the seed among them); ≥ 20 s after that with no
`session_id` —
which every hook event carries — the hooks are silent. "Given a prompt", not
"seen working": Codex's screen reads `working` while it boots an MCP server,
before any prompt, and its first hook fires only at the first prompt
(measured). `status --json` reports it as `hooks_ok`. Verified live with
`claude --settings '{"disableAllHooks":true}'` seeded: `wait --after 0` →
`no_hooks`, rc 6.

The dialog verdict comes from herdr, asked directly (at most once a second),
not from `.state`: `.state` lags by up to a monitor tick, and right after a
dialog is answered it would still say `blocked`. `send --answer-permission`
replies once the dialog is seen gone (≤ 1 s), so a `wait` issued the instant
it returns gets the turn, not the answered dialog (verified 2026-10-12).

### 9. `seagents send` / `stop` — driving an agent

herdr writes into the agent's pane: `herdr agent prompt` (a paste the way the
agent takes one — bracketed when it asked for it — then Enter), `pane
send-text` (a paste without Enter, `--no-submit`), `pane send-keys` (herdr's
key names). What herdr does not do, `send.rs` does around every write, in the
calling process — the CLI, the HUD, or `run`'s seed thread. There is no
control socket and no authorization: any agent may drive any agent (decided
2026-10-12).

**One at a time.** Every `send` and `stop` holds the session's
`<key>.send.lock` for its whole length (`registry::send_lock`), so two never
interleave, from any processes.

**Submit when idle.** herdr sends at once, and a prompt that lands mid-turn
is folded into the running turn — its first answer lost (measured
2026-10-12). So a `send` is held (up to `--timeout`, 5 min) until the agent is
out of its turn (`turn::Ledger::in_turn`), herdr reads it idle (or done), and
no earlier submission of ours is still landing (`.meta`'s `submitted_ms`:
until the agent's `UserPromptSubmit` has fired since, or 5 s pass); then the
body is written and the reply carries the ledger's `stops` at that instant,
`paired: true`, so `wait --after <stops>` returns the turn it started. The
reply waits for that hook (≤ 5 s), so a `wait --after <stops>` issued the
instant `send` returns sees the new turn open — without that, after an Esc'd
turn the ledger still read "prompted, no longer running" and such a `wait`
returned `interrupted` for the *old* turn (found live on Codex). If the hook
does not come and the input box still holds exactly the text, its Enter was
lost — an agent still settling after a trust dialog drops one — and Enter is
pressed once more. A human starting a turn between the check and the write is
an accepted race (herdr has no "prompt only if idle"). Verified 2026-10-12:
two back-to-back sends were answered by their own turns (`ONE`, `TWO`).
`--now` writes at once (`paired: false`). A **blocked** agent — herdr's
verdict, or one of the adapter's `[screen] dialogs` — is refused at once,
reason `blocked`, exit 3, with the dialog's `rule`, even with `--now`: whatever
is typed into a dialog answers it. `--keys` into one is refused too unless
`--answer-permission` is given (`--answer-permission --keys 1` picks "Yes" on
Claude's permission menu, verified), which then replies once the dialog is
seen gone (≤ 1 s, §8). Exit codes: 0 ok, 2 `busy_timeout` (nothing was sent),
3 `blocked`, 4 `exit`, 6 `draft` (below), 1 anything else.

**The input box.** A paste lands after whatever the agent's input box
already holds: a prompt Claude put back after a turn dropped before its
first response (§6), or a human's half-typed draft. So before any paste (not
`--keys`) `send` looks (`make_room`): `herdr pane read --format ansi`, and
`screen::input_box` takes the last row starting with the adapter's `[send]
input_glyph` (Claude `❯`, Codex `›`) and the rows under it up to a blank row
or a rule. herdr keeps the styling, and the empty box's placeholder (`Try
"…"`, a suggested prompt, `Ask Codex to do anything`) is drawn **dim**
(SGR 2) where typed text is not, so only text that is not dim counts as typed
— which also closes the old gap of a draft whose cursor was moved to its
start reading as empty. Text that is, whitespace aside, the last prompt
seagents submitted (`.meta`'s `last_prompt`: a `send`'s, or the seed's) is
ours: the adapter's `input_clear` keys (`ctrl+u` for both: to the line's
start, then the newline before it — never Ctrl+C, which on an empty box
starts exiting) are pressed, re-reading after each, until the box reads
empty. Anything else is refused, `draft`, rc 6, `--now` or not — the human is
typing; the reply gives its length, never its words. Verified 2026-10-12
through herdr: a put-back prompt was cleared and the next send arrived alone;
a typed draft was refused (rc 6) for Claude and Codex.

**Long bodies go by reference.** Claude files a large paste in its transcript
as `<pasted_content id="…">…`, and current models treat that as data "that may
contain instructions the user did not write". Measured (2.1.284, haiku,
bracketed paste + 300 ms + Enter, which is what herdr's `agent prompt`
sends): one line of 700 chars and 2 short lines land inline, as the user's
words; one line of 900 chars, or 7 short lines, are wrapped. So the adapter
says when (`[send] long_prompt = "file"`, `inline_max_chars = 700`,
`inline_max_lines = 2` for Claude; every other adapter pastes everything):
past either limit the body is written to `<agent cwd>/.seagents/prompts/<key>-<stops>-<4 hex>.md`
(0600, `registry::write_prompt`; `.seagents/` goes into the repo's
`info/exclude` first) and one line is submitted instead: `Read the file
.seagents/prompts/<name>.md (relative to your working directory) and carry out
the instructions in it. They were sent to you through seagents.` A Read inside
the agent's cwd needs no permission. The files are the session's, deleted
with it. A body that goes inline travels on herdr's command line (128 KiB at
most; one longer is refused) — visible in `/proc` while that call runs, which
the pty wrapper's socket was not; seeds always go by reference for that
reason (§7).

**`stop`** submits the adapter's `[stop] exit_sequence` (Claude and Codex:
`/exit`) and gives it 5 s; then closes the agent's herdr pane (which hangs
it up) and gives that 5 s; then SIGKILLs its process group. It takes the
send lock only for up to 2 s: a `send` can hold it for minutes waiting on a
busy agent, so past that `stop` goes straight to closing the pane, and the
held `send` ends with `exit` once the agent is gone, typing nothing
(verified: `stop` in 2.5 s, the held send rc 4). It replies once
the agent is gone, and `run`, which holds its window, then closes the window
too. `--force` skips the exit sequence. So does an agent at a dialog —
herdr's, or one of the adapter's `[screen] dialogs` (`/exit` + Enter typed
there would answer it: verified 2026-10-12, Codex at its trust question, which
herdr reads as idle — `stop` closed the pane and its config gained no trust
entry) — and one whose box holds someone's draft (the sequence would be
submitted as part of it). A prompt seagents sent that the agent put back is
cleared first (verified: `stop` then exited via `/exit` in 1.4 s).

### 10. `seagents read` — the conversation

Content comes from the agent's own transcript (the path its hooks
reported in `.agent`, or kept in its tombstone once it has exited and been
reaped; `--transcript PATH` reads any file, its format told
from its first lines — a Codex rollout wraps every line in `{timestamp,
type, payload}`, a Claude transcript never does), never the screen.
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

**Collapse rules** (`render.rs`): user text in full behind
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

**The offset index** (`index.rs`, the `.idx` files): one 53-byte entry
per tree node (offset, length, kind, timestamp, uuid, parent), so the main
line is found without parsing content and `--tail N` reads only the lines it
prints. Cached in `~/.cache/seagents/sessions/`, extended from the last
indexed byte on every open (transcripts are append-only; only complete lines
are indexed), rebuilt when the file is shorter than that or its first 4 KiB
no longer hash the same. Measured on the largest transcript here (53 MB):
`read --tail 20` in 65 ms cold, 6 ms warm; the whole main line as NDJSON in
90 ms. The header records the transcript's canonical path (format v2; a v1
index is swept and rebuilt), and a read that changes nothing touches the
index's mtime. `index::cache_path()` — every cached open — sweeps the
directory at most once a day (the mtime of `sessions/.swept`; one `stat`
otherwise): an index whose transcript is gone, or unread for 30 days, goes,
as do `*.tmp` leftovers a day old. Verified with a scratch HOME: a gone
transcript's, a 40-day-old and a v1 index went on the first read after the
stamp aged past a day, the live one stayed.

### 11. Codex — the second adapter

Measured on codex-cli 0.152.0 (2026-09-29), live in tmux with a scratch
`CODEX_HOME` (seagents never writes `~/.codex`; only Codex's own sessions
and logs land there). `src/adapters/codex.toml` carries the facts:

- **Hooks are Claude's.** Same events (`SessionStart`, `UserPromptSubmit`,
  `Stop`, plus `PreToolUse`, `PermissionRequest`, `PostToolUse`,
  `Pre/PostCompact`, `SessionEnd`, `SubagentStart/Stop`, and an `Interrupt`
  Claude lacks), same payload fields; `transcript_path` is the rollout
  (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`). So `__hook` is
  unchanged. `SessionStart` fires at the first prompt, not at launch (a
  resumed session's too, `source: "resume"`); every event carries the path.
- **Configured under `[hooks]` in config.toml, or a `-c` override** —
  which Codex keeps as its own "session flags" layer
  (`/<session-flags>/config.toml`) beside the user's. **A hook runs only
  once trusted** (`codex exec` silently skips untrusted ones; the TUI stops
  at "Hooks need review"): `hooks.state."<layer>:<event_snake>:<group>:<handler>".trusted_hash`
  = `sha256:` of the canonical JSON of the hook's normalized identity
  (codex-rs `hook_hash`; reproduced exactly, checked against the hash Codex
  wrote in `/hooks`). Codex reads that state from the user layer
  *and* the session-flags layer, so `run` passes ONE `-c hooks={…}` holding
  our three hooks and their trust — no `--dangerously-bypass-hook-trust`
  (which would also run the user's unreviewed hooks), nothing written. A
  dotted `-c 'hooks.state."…".trusted_hash=…'` does not work (Codex splits
  keys on every `.`), hence the one inline table. A user's own `-c hooks…`
  overrides are applied in order, as Codex would, and merged into it.
- **Exit**: `/exit` (or `/quit`), instant. **Paste**: bracketed paste +
  300 ms + Enter (herdr's `agent prompt`) submits; a large paste shows as
  `[Pasted Content N chars]` but is submitted whole as the user's words
  (4,392 chars / 31 lines, acted on), so Codex takes no prompt files — its
  seed does, as every seed (§7). Its `-c hooks={…}` goes on the command line
  herdr types into the pane, quoted by herdr (verified: the hooks fire).
- **Esc** fires `Interrupt` and no `Stop`, and writes `turn_aborted` to the
  rollout — even pressed before the first token (Claude's known gap). The
  ledger reads the rollout record (`codex::interrupted_since`), the same
  seam as Claude's, so the `Interrupt` hook is not injected.
- **Session variables** a Codex shell tool sees: `CODEX_CI`,
  `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_PERMISSION_PROFILE`,
  `CODEX_SANDBOX*`, … — stripped by the launcher; `CODEX_HOME` is the user's,
  and passed on to the agent's pane (`pass_env`, §1).
- **Folder trust**: a never-trusted directory opens on "Do you trust the
  contents of this directory?"; answering writes `[projects."…"]` into
  config.toml. herdr 0.9.0 reads it as **idle** (seen 2026-10-12; upstream's
  rule misses it drawn over an update notice too), so the adapter lists it in
  `[screen] dialogs` and seagents counts the agent blocked there (§3): no
  seed, no send, no `/exit` goes in (verified by `tools/test-orchestration.sh
  --codex`, step 6b). A bug report to herdr is the user's call.
- **Update notice**: an outdated Codex opens on "Update available!" with a
  menu, which herdr reads as blocked; `send --answer-permission --keys 2`
  skips it.
- **Resume**: `codex resume <id> …` takes every TUI option, and a `-c`
  before the subcommand still applies (our hooks fire in the resumed
  session).
- **Permission modes**: no mode flag; `-a` (0.152 takes only `on-request`
  | `never`) and `-s` (`read-only` | `workspace-write` |
  `danger-full-access`). The adapter names Claude's four modes and maps each
  to a pair (`[launch.permission_argv]`, nearest not more privileged):
  `plan` and `default` → `-a on-request -s read-only` (every write is an
  approval request — verified live: a write prompt stopped at Codex's "Would
  you like to run" gate, `wait` rc 3, no file), `acceptEdits` → `-a
  on-request -s workspace-write`, `bypassPermissions` → `-a never -s
  danger-full-access`.
- **The rollout** is a log, not a tree: no uuids, no parents. Nodes are its
  lines (uuid = byte offset + 1), chained; `thread_rolled_back {num_turns}`
  hangs off the node before the user turns it drops, so they become a side
  branch the main line skips. A "user" message is the user's only when its
  `content_item_kinds` say `user.*` (older rollouts: not one of Codex's
  context tags) — environment context, AGENTS.md, `<turn_aborted>` are
  filed under the user role too.

---

## Key design decisions (and why)

- **herdr owns the agents; i3 windows are views (2026-10-12)** — the pty-tee
  wrapper made the window the only way into an agent, so nothing could reach
  it from another machine. Speaking herdr's remote protocol ourselves was
  rejected (herdr is installed here and the stand-in would take its name; its
  surface encoding and libghostty rendering would have to be reproduced), and
  so was growing our own remote attach (a second remote client next to
  herdr). With herdr owning the agents, `herdr --remote ssh` works as it is,
  and seagents lost its relay, its vendored detector and its control socket.
  HERDR-BACKEND-SPEC.md has the reasoning and every decision (D1–D7).
- **One process holds the window (`run`)**, as the parent of the attach — not
  an `exec` into it — because the window and its agent live and die together
  (closing the window must close the pane), and a detach must be undone.
- **The hook turn ledger stays** — herdr's `wait` has no baseline and cannot
  tell an interrupted turn from a finished one (§6).
- **No lineage** — once any agent can call `herdr` directly, seagents-side
  family rules, caps and permission ceilings only guard the seagents path.
- **The herdr CLI, not its socket API** — the CLI is the surface herdr keeps
  stable across versions; everything goes through `src/herdr.rs`.
- **focus by recorded con_id** — the only reliable window handle given Terminator's
  shared PID and colliding idle titles.
- **self-reaping cache** — a fully-exited agent isn't interesting; "idle" already
  means finished-and-waiting-at-prompt (still alive).
- **License**: Apache-2.0 (decision D7). It was AGPL-3.0-or-later only while
  it vendored herdr's then-AGPL detection code, which is gone.

---

## Limitations & gotchas

- **The agent runs under the herdr server's environment, not `run`'s.**
  `FOO=bar seagents run claude` does not reach Claude; only the adapter's
  `pass_env` (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) and `SEAGENTS_DEBUG` are
  passed to a new pane (as `--env`, so paths only — herdr's command line is
  visible in `/proc`). A tab `run` reuses (§1) keeps the environment its shell
  has. Start the herdr server from the environment you want agents to have,
  or let `run` start it (`herdr::ensure_server`).
- **herdr starts the agent by typing its command into the pane's shell** —
  so it runs your shell's rc files and aliases (the `claude` alias is fine,
  §1), it shows in the shell's history, and `run /abs/path/claude` loses its
  path: herdr types the kind's bare executable, so the one on the pane
  shell's `PATH` runs. A shell that does not reach a prompt (zsh with no
  `.zshrc` opens its new-user wizard) swallows it: the test harness gives its
  scratch HOME an empty `.zshrc` for that reason. herdr refuses agent
  arguments holding control characters (a newline in an argument fails the
  start).
- **herdr takes `--session` out of its own argv, anywhere before a `--`**
  (herdr 0.9.0, `session.rs`): a `send` text — or anything else seagents
  hands the herdr CLI as one argument — that is exactly `--session` (and the
  argument after it) or starts with `--session=` is consumed as herdr's
  option, not passed on. A `--` does not help (the CLI then pastes it as
  text), so it is left as a known edge; agent arguments to `agent start` sit
  after its `--` and are safe.
- **herdr may resume an agent itself after its server restarts** — its own
  Codex resume (only for argv exactly `resume <id>`, so not one seagents
  started with hooks) or its Claude integration if installed — and that
  agent then holds its name. `restore` leaves such a tag to herdr and says so
  ("herdr already runs an agent named …"), rather than open a window whose
  `run` would refuse the tag.
- **Ctrl+B is herdr's attach prefix** in an agent's window (fixed in 0.9.0):
  Claude's own Ctrl+B is Ctrl+B Ctrl+B. Ctrl+B q (detach) is undone.
- **Start latency**: herdr's `agent start` settles for ≥ 3 s, so a window
  shows `starting <tag>…` for a few seconds before the agent.
- **Text submitted inline travels on a `herdr` command line** (visible in
  `/proc` while it runs); seeds and long bodies go by reference instead (§9).
- **Agents started by the pre-herdr binary are not tracked** after
  installing this one — restart them.
- **Codex leaves empty `.git`/`.codex`/`.agents` directories in its cwd** (its
  Linux sandbox's mount points) when the cwd is not already a repo root. Claude
  then reads that `.git` as the root of a separate, never-trusted project, so
  the next Claude started there opens on the folder-trust dialog (`wait` reports
  it as `blocked`). Mixing both tools in one non-root directory hits this; the
  integration test runs Codex in a directory of its own for that reason.
- **Multiple agents as tabs in one Terminator window** share one `con_id` (one X
  window); i3 can't focus a tab within a window, so that case is ambiguous.
  Separate windows work perfectly.
- **Window identity on a shared-process terminal (Terminator)** comes from
  the handle (windows seagents opened) or the focused window at launch; if a
  dialog steals focus at that instant the ids are recorded as `null` or
  wrong, and — with no input pump since the move to herdr — nothing heals
  them until the agent is relaunched. Under alacritty identity is exact
  (`ALACRITTY_WINDOW_ID`).
- **Mouse flicker on HUD preview** — with i3's default `mouse_warping output`, each
  preview warps the mouse to the agent's monitor and back. Setting `mouse_warping
  none` in i3 removes it, but that's a *global* i3 behavior change (undecided).
- **`restore` reproduces a layout exactly only when nothing is running.** i3
  appends a layout to a *workspace*; there is no way to drop a placeholder into
  a container that already holds live windows. The post-reboot case (nothing
  alive) is exact — verified by `tools/test-restore.sh`. Restoring alongside
  survivors puts the missing windows at workspace level instead, and says so.
- **herdr keeps empty tabs** of agents that ended across its own restart;
  `restore` reuses those labelled with a restored tag, the rest stay until
  closed in herdr.
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
  file to fall back to the live layout.
- **Floating agents restore tiled** — placeholder matching is per-container, and
  an agent that comes back tiled is a smaller surprise than one that doesn't.
- **Restored windows land on whichever workspace is focused when they spawn**,
  so don't switch workspaces during a restore.
- **keyd `overload(meta, esc)` kills the HUD on `$mod`+drag** — mouse buttons
  don't pass through keyd, so a capslock(meta)+mouse-drag counts as a *tap* and
  emits Esc into the focused window on release; the HUD quits via its Esc
  binding (looks like a crash, exit 0). Fix on each keyd machine:
  `[global] overload_tap_timeout = 200` in `/etc/keyd/default.conf`, then
  `keyd reload`. The HUD additionally ignores an Esc that arrives with more
  input pending, so torn mouse escape sequences can't fake a quit.

---

## Open items / backlog / next steps

- **Hand checks on the real desktop** still open (HERDR-BACKEND-SPEC
  § Verification): `claude` in a fresh Terminator window with no herdr server
  running, the HUD's preview and jump, `focus` on Terminator, `herdr --remote
  ssh` against agents seagents started, and a real reboot through `restore`.
- `seagents hud` / `status` / `wait` / `send` **against another machine**
  through herdr's `--machine` API forwarding (needs a saved herdr machine
  profile) — HERDR-BACKEND-SPEC § Later.
- A bug report to herdr for Codex's trust question reading idle (the user's
  call; herdr's contributor rules allow a reproducible bug report, not a PR).
- Decide the **mouse_warping** question for the HUD preview.
- Add `restore` to i3 autostart (`exec --no-startup-id seagents restore`) once
  it has been through a real reboot, or keep running it by hand.
- Optional: **eww** port of the HUD for a prettier always-on widget.

---

## Dev workflow

```bash
cargo build --release
cargo run -- status    # etc.

# herdr's verdict on an agent's screen, and the rule that matched:
herdr agent explain <pane> [--json]
herdr pane read <pane> --source visible [--format ansi]

# Try seagents against an isolated herdr session, never the one you work in:
HERDR_SESSION=sgtest herdr server &        # (strip CLAUDECODE & co. first)
HERDR_SESSION=sgtest XDG_STATE_HOME=/tmp/sgstate seagents run claude …
HERDR_SESSION=sgtest herdr server stop; herdr session delete sgtest
# (XDG_CONFIG_HOME must stay: herdr keeps its socket under it, and a long
#  path overflows sun_path. The cache, ~/.cache/seagents, is shared — test
#  tags leave tombstones there for a day.)

# Debug the HUD post-mortem (its terminal vanishes with it):
SEAGENTS_HUD_LOG=/tmp/hud-events.log seagents hud
# appends every crossterm input event + the exit reason
# Tests that open windows run inside a nested, isolated desktop — headless
# weston + rootful Xwayland on :9 + its own i3, with HOME and XDG_RUNTIME_DIR
# pointed at a scratch dir (and so a herdr server of its own: herdr keeps its
# socket under HOME) — set up by tools/lib-nested-desktop.sh, which each
# sources (nd_init <terminal>; nd_start; cleanup on exit kills every window in
# it, the herdr server, then the desktop).
#
# Regression-test snapshot/restore. Each window runs a real `claude --model
# haiku`, never prompted (it waits at its trust dialog). Fails with a tree
# diff, and refuses to pass if the pre-restore kill left anything alive (a
# restore that does nothing would otherwise compare equal). Run it for each
# terminal recipe.
tools/test-restore.sh terminator
tools/test-restore.sh alacritty
tools/test-restore.sh alacritty-daemon

# Integration-test spawn (herdr lists the agent by its tag) → wait → status →
# send (idle-paired; multi-line and > 700-char bodies by reference, carried
# out in --permission-mode default; a Bash permission gate: wait says
# blocked, send and bare --keys refused, --answer-permission --keys 1, and an
# immediate wait gets the turn) → read (--json holds every answer and the
# gated call; --from its cursor after one more turn gives just that turn) →
# a prompt Esc'd before its first response (interrupted; the next send clears
# it from the input box and arrives alone) → a draft in the box (send rc 6) →
# focus → the trust dialog → stop (its herdr pane gone, the window closed) →
# closing a window ends its agent → the tombstone, with a real Claude
# (`--model haiku`, one-line prompts: a few cents a run) in
# target/sg-it/orchestration, whose folder trust it inherits from the repo.
# Keeps the real CLAUDE_CONFIG_DIR for auth, and runs as if from inside a
# Claude session (stand-ins for any of its session variables not already set),
# checking the agent's /proc/<pid>/environ holds none of them. PASS/FAIL per step.
tools/test-orchestration.sh [alacritty|terminator|alacritty-daemon] [--codex]
# --codex: the same loop with a real Codex (gpt-5.6-luna, low effort) in a
# scratch CODEX_HOME that links the real auth.json only (update check off) —
# seeded turn, capabilities, send, a 41-line paste, Esc → interrupted then a
# fresh turn, a draft in its composer (send rc 6), read, stop; then Codex at
# its folder-trust question: blocked (seagents_dialog), its seed held back,
# stop closes the pane, no trust written. Skipped with a note when codex
# isn't installed.
```

The HUD and an agent's window need a real terminal — validate them by hand
(or with a pty harness), not in a pipe.

---

## Conventions

The project's conventions (cutover rule, no unit tests, commit style) are in
[`CLAUDE.md`](../CLAUDE.md). Machine-specific notes, when a checkout has any,
go in an untracked `CLAUDE.local.md` at the repo root.

---

## Environment

Developed and verified on Linux with X11 i3, Terminator and alacritty, herdr
0.9.0, Rust 1.96. seagents assumes X11 i3 (it uses `i3-msg -t get_tree`); it
has not been tested on sway/Wayland.
