# seagents on herdr — spec

Design spec for moving seagents' agents out of its own pty wrapper and into a
**herdr server**, with each i3 terminal becoming a *view* of one agent.

Status: **implemented** and merged into `main` (2026-10-12). Written the same
day, the approach proven by hand first (see *Verified*); the decisions below were taken before building, and
*Decisions D1–D7* during it. Where this and the code disagree, the code and
[`HANDOFF.md`](./HANDOFF.md) win.

Related: [`HANDOFF.md`](./HANDOFF.md) (what is built).

---

## Problem

Away from the laptop, its agents are unreachable. Each agent's PTY belongs to
the seagents wrapper running inside one i3 terminal window, and that window is
the only way in: nothing on the network can show the agent or type into it.

herdr already solves remote access (`herdr --remote ssh <host>` from the PC),
but only for agents that live inside a herdr server. Ours live inside i3
terminals, so it finds none.

## Options considered

1. **seagents speaks herdr's protocol** (a stand-in `herdr` on the laptop that
   answers `herdr --remote`). herdr's client endpoint is a frozen contract
   (generation 1, `src/protocol/endpoint.rs`), so this is technically possible,
   but rejected: the laptop already runs the real herdr (0.9.0,
   `~/.local/bin/herdr`) and the stand-in would have to take its name. We'd
   also have to reproduce its binary surface encoding and render with `vt100`
   where herdr uses libghostty.
2. **seagents grows its own remote attach** (`ssh -t laptop seagents attach`).
   Workable, but it rebuilds a slice of herdr (snapshot, live stream, shared
   input, size arbitration), and you'd use a second remote client next to herdr.
3. **herdr owns the agents; i3 windows attach to them.** Chosen. Remote access
   comes free, and seagents loses code instead of gaining it.

## Verified (2026-10-12, by hand, herdr 0.9.0)

- `herdr workspace create --cwd D --no-focus`, then `herdr pane run <pane>
  'command claude'`, starts Claude inside the herdr server. `herdr agent list`
  reports it (`agent: claude`, state `blocked` at the trust prompt).
- `herdr agent attach <pane>` in a plain i3 terminal shows **just** that
  agent, full window, no herdr interface. Keys work; resize redraws.
- `herdr --remote ssh <laptop>` from the PC shows the same agent and drives
  it while the laptop window is attached.
- **Gotcha:** with Claude wrapped as `seagents run claude` (the shell alias)
  inside the pane, herdr does not recognise it as an agent; the foreground
  process is `seagents`. The wrapper and herdr cannot be stacked: one of them
  must own the agent.

Not yet checked carefully: what each side shows when the PC and laptop
windows differ in size while both are attached (they "seemed fine").

## Target architecture

```
                herdr server (laptop)  ← owns every agent's PTY, detects state
               ┌────────────────────────────┐
               │ claude A   claude B   codex │
               └───┬──────────┬─────────────┘
                   │          │
  laptop i3:   [window A]  [window B]        herdr agent attach A / B
                   ▲          ▲
  seagents:    run · focus · hud · snapshot  (i3 side + launcher, over herdr's CLI)

  PC:          herdr --remote ssh laptop     (herdr's own TUI, nothing of ours)
               seagents … over `herdr --machine laptop` (later)
```

seagents becomes **the i3 integration for herdr**: it launches agents into
herdr, opens and tracks the i3 window attached to each, and keeps the HUD,
focus and layout snapshot. herdr is the one source of truth for the agent's
process, screen, state, input and remote access.

### `seagents run` (and the `claude` alias)

`seagents run claude …` in an i3 terminal:
1. finds or creates the herdr workspace for the cwd (see *Open questions*),
   and a pane in it;
2. `herdr agent start <tag> --kind claude --pane <p> -- <args>` (the tag
   becomes herdr's agent name: both are `[a-z][a-z0-9_-]{0,31}`-shaped);
3. records the i3 window identity against the herdr pane, then runs
   `herdr agent attach <tag>` in the same terminal and closes the pane
   when it ends (see open question 2).

So the alias keeps working and the window looks as it does today.

## Capability map

| seagents today | after | notes |
|---|---|---|
| pty-tee wrapper (`src/wrapper/`: pumps, lead, monitor, inject, control) | **deleted** | herdr owns the PTY; `run` becomes the launcher above |
| state detection (`detect.rs`, `vendor/herdr_detect/`, `update-manifests`, `debug-screen`) | **deleted** → `herdr agent list`, `agent explain` | the Codex trust-dialog patch became the adapter's `[screen] dialogs` (question 5) |
| `status` / `hud` rows | herdr state + our window records | herdr's `idle`/`done`/`working`/`blocked`/`unknown` |
| `send` / `stop` (control socket, lineage rule, submit-when-idle, input-box check) | `herdr agent prompt` / `send-keys`, behind our idle hold and draft check | lineage rule deleted (question 4) |
| `wait` (hook turn ledger, §8) | **kept**: the ledger, plus herdr's dialog verdict | herdr waits on lifecycle state, not a turn (question 3) |
| `read` (transcript, §10) | **kept** | reads the agent's own JSONL; independent of the wrapper |
| `spawn` (§7) | an i3 window running `run`, which does `herdr agent start` | caps and permission bounding deleted with lineage; the seed goes in once idle, by reference |
| `focus` (§4) | kept | the window is the attach window; identity recorded at `run` |
| `snapshot` / `restore` (§6) | **kept**: relaunches through `run` with `--resume` | windows and agents die together (question 2), so herdr's persistence restores no agent; `run` reuses the empty tab herdr brings back |
| hooks (`hook.rs`, `[hooks] inject`) | **kept**, injected through `herdr agent start -- …`, found by `HERDR_PANE_ID` | Claude's settings by file path (D5); herdr's own integration optional (question 3) |
| Codex adapter | kept for transcript/spawn bits | herdr already identifies Codex |
| session cache (`registry.rs`) | reshaped | keyed by the herdr terminal id (stable across pane moves, unique across servers), not wrapper pid |

## Open questions (decide before building)

1. ~~**Workspace shape.**~~ **Decided 2026-10-12: one herdr workspace per
   cwd, one tab per agent** (groups like the HUD's `dir` sort; each agent
   full-size in the PC's herdr). A `spawn`ed agent opens as a tab in the
   workspace for its cwd, like any other.
2. ~~**Closing an i3 window.**~~ **Decided 2026-10-12: closing the window
   kills the agent**, as today. So `run` does not exec `herdr agent attach`:
   it stays as the attach's parent, and when the attach ends (window closed,
   or a detach) it closes the herdr pane. A window and its agent live and die
   together; there is no "detached" state for the HUD to show.
3. ~~**Turn tracking.**~~ **Resolved 2026-10-12 (live, isolated herdr
   session): keep the hook turn ledger.** herdr's `agent wait` has no
   baseline: called right after `agent prompt` it returns the *previous*
   idle (20 ms) before `working` shows (0.4 s later). A timeout tells the
   caller nothing, and an Esc'd turn looks like a finished one. A second
   prompt sent mid-turn was merged into the running turn, and the first
   answer was lost. `state_change_seq` is one global counter, reset when the
   server restarts. So herdr gives state and exit; turns, `stops`,
   `interrupted` and the final message stay ours. Hooks are still injected
   per launch through `herdr agent start … -- --settings <json>` (verified:
   the JSON passes through and the hooks fire). The ledger is keyed by herdr
   pane: hooks inherit `HERDR_PANE_ID`/`HERDR_SOCKET_PATH`, which replaces
   the `/proc` ancestry walk. herdr's own integration (not installed here)
   reports only the session id for Claude (no state); it is user-global and
   coexists with per-launch `--settings` hooks. If installed, its
   `agent_session` field can supply the session id.
4. ~~**Lineage and the input-box check.**~~ **Decided 2026-10-12.**
   - **No lineage.** Parent/child goes entirely: no `parent`/`depth` fields,
     no family rule on `send`/`stop`, no fan-out caps, no permission ceiling
     inherited from a parent, no HUD tree, and no `/proc` ancestry walk to
     find "who is calling". Any agent may drive any agent, and every agent is
     a top-level row. Why: once herdr owns the agents, an agent can call
     `herdr` directly, so these checks only guard the seagents path. What
     actually guards it is Claude's own permission prompts. That leaves a
     lot of code guarding a door with another door beside it.
   - **Submit when idle**, kept, behind a per-agent lock: wait for "not in
     turn" (the ledger) and herdr idle/done, record `stops`, call
     `herdr agent prompt` without `--wait`, then confirm through
     `UserPromptSubmit`. herdr itself sends immediately (a mid-turn prompt
     was merged and lost the earlier answer) and only refuses `blocked`. A
     human starting a turn between the check and the send is an accepted race.
   - **Blocked refusal:** herdr's own (`agent_blocked`).
   - **The draft check**, kept: herdr has no cursor in any API, but `agent
     read --format ansi` keeps the placeholder's dim styling (`ESC[2m`), so
     dim text means an empty box. Non-dim text equal to our last prompt is
     cleared with `send-keys ctrl+u`; anything else is refused as `draft`.
     This closes the old "cursor moved to the start" gap. Not yet checked
     for Codex. Verified: herdr pastes straight onto a human's draft.
5. ~~**Detection patches.**~~ **Resolved 2026-10-12.**
   - Claude `background_shell_working` re-weight: obsolete. Upstream deleted
     the rule (herdr `987b070f`, in 0.9.0).
   - Codex `seagents_trust_directory`: still needed. Upstream's rule still
     misses the trust dialog drawn over an update notice, so it reads as idle
     or unknown, never blocked. A herdr override would have to replace all of
     `codex.toml`, freezing upstream Codex fixes, so it is not used. Instead,
     the question is listed in the Codex adapter's `[screen] dialogs`, so
     seagents counts the agent blocked while it shows — no `/exit`, no seed,
     no send goes into it (see *Decisions*). herdr's
     contributor rules allow a bug report (not a PR); filing one is the
     user's call.
6. ~~**Reboot.**~~ **Follows from 2:** a window and its agent die together,
   so herdr's own persistence plays no part. `snapshot`/`restore` keep
   working as today: restore relaunches each window through `seagents run`
   with `--resume`.
7. ~~**Branch.**~~ **Done 2026-10-12:** `refactor/phase0` fast-forwarded
   into `main` (local); the cutover is on `feat/herdr-backend`.
8. **The `claude` alias inside herdr panes.** `herdr agent start` types
   `claude …` into the pane's interactive shell, where the user's alias turns
   it into `seagents run claude`, so herdr would see the wrapper, not the
   agent. Fix: `seagents run` inside a herdr pane (`HERDR_ENV=1`) execs the
   command directly (same pid, so herdr sees `claude`), so the alias stays.

## Decisions D1–D7 (2026-10-12, taken while building)

- **D1 — a detach is undone.** In an agent's window Ctrl+B is herdr's attach
  prefix (fixed in 0.9.0) and Ctrl+B q detaches. Since a window and its
  agent die together (question 2), a detach is neither "kill" nor "leave
  running": `run` starts the attach again at once. `run` stays the attach's
  parent and tells the cases apart: the window closing (SIGHUP, or stdin
  polls hung up) closes the agent's pane; the attach exiting 0 with the
  window still there is a detach; `terminal attach taken over` (another
  client's `--takeover`) is respected — the window says so and waits,
  never taking the agent back and never using `--takeover` itself; 3
  failures within 10 s end the session with the error. herdr 0.9.0's attach
  exits 0 on a detach and 1 (its reason on stderr) on a takeover or a
  server shutdown.
- **D2 — Ctrl+B** stays herdr's (not configurable in 0.9.0): Ctrl+B Ctrl+B
  sends one. Documented, not worked around.
- **D3 — `run` starts only agents herdr knows** (`herdr::kind_for`; exit 2
  otherwise). `run sleep` etc. are gone; the restore test runs real,
  unprompted `claude` instead.
- **D4 — tags start with a letter** (herdr's agent-name rule
  `[a-z][a-z0-9_-]{0,31}`; the tag is the herdr agent name). A directory
  whose name starts otherwise mints `d<name>-<hex>`. (`restore` folded tags
  captured before this rule the same way until 2026-10-12; that shim is
  gone.)
- **D5 — Claude's hook settings by file**: `<runtime dir>/hooks-<key>.json`
  (0600, removed with the session), passed as `--settings <path>`, since
  herdr types the agent's command line into the pane's shell (and its
  history). Codex's `-c hooks=…` stays inline.
- **D6 — start, then attach**: `run` prints `starting <tag>…` and attaches
  once `agent start` has returned, so a start failure shows in the window.
- **D7 — the license**: Apache-2.0 (was AGPL-3.0-or-later only because it
  vendored herdr's then-AGPL detector; that code is gone, and herdr itself is
  Apache-2.0 since 2026-07-22).

Found while building, and resolved the way most consistent with the above:

- **The agent runs under the herdr server's environment, not `run`'s.** An
  adapter's `[launch] pass_env` (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) and
  `SEAGENTS_DEBUG` are passed to a new pane with `herdr tab create --env`
  (paths only: herdr's command line is visible in `/proc`). Found when a
  test's `CODEX_HOME` did not reach Codex.
- **Codex's trust question reads as idle to herdr** — so the seed, too (not
  only `stop`), would have answered it. The `stop`-only guard became one
  adapter list, `[screen] dialogs`, read by `status::agent_status`, through
  which `status`, `wait`, `send`, the seed and `stop` all count the agent
  `blocked` (rule `seagents_dialog`) while one shows.
- **A seed pasted right after a trust dialog closes lost its Enter** —
  Claude reads idle while still settling. The seed waits for 1.5 s of idle,
  and any prompt whose hook does not report it, with exactly its text still
  in the box, gets Enter once more.
- **Session key = herdr terminal id**, not pane id: stable across pane moves
  and unique across herdr servers (the plan keyed by pane id).
- **The draft check reads herdr's ANSI screen** (`screen::input_box`): dim
  text is the placeholder — verified for Claude and Codex.
- **herdr brings closed tabs back too** on a server restart; `place()`
  reuses a tab labelled with the tag in any workspace at the cwd.

Fixed after an independent review of the cutover (2026-10-12): `reap` no
longer touches a session whose `run` is alive (it deleted the `.meta` of an
agent that had just exited, stranding its window), and the monitor ends the
window when the `.meta` goes anyway; `stop` waits for a held `send` at most
2 s; `run` starts the agent only once the pane's shell is in the
foreground, retrying `agent_pane_busy`; the dialog match normalises boxes,
wrapping and whitespace and fails closed on an unreadable screen; window-id
variables are stripped for a new server and a new terminal; a seed is never
dropped and never printed over the window; `wait`/`send` judge liveness on
the current `.meta`; the hook settings file is written after the `.meta`; a
half-made workspace is closed; Ctrl+C no longer ends a window that only
waits; `restore` leaves a tag herdr already holds to herdr.

## Verification

Verified 2026-10-12: by hand against an isolated herdr session (a pty
harness standing in for the window): start through the alias's in-pane
exec, hooks found by pane, send/wait pairing, back-to-back sends,
by-reference bodies, the draft refusal and clearing, a detach undone, a
takeover respected, window close ending the agent, `stop` (graceful,
`--force`, `/exit` typed in the window), the trust dialog with a seed, a
permission gate answered with `--answer-permission --keys 1`, the
dropped-turn rule (10.8 s), spawn from inside an agent's pane (no pane or
session variables leaked), the HUD's `x`, tab reuse after a server restart,
Codex's trust question and `stop`. In a nested desktop:
`tools/test-orchestration.sh alacritty --codex` and `tools/test-restore.sh`
with terminator, alacritty and alacritty-daemon — all pass.

Still to check by hand on the real desktop (HANDOFF § Open items): typing
`claude` in a fresh Terminator window with no herdr server running; the
HUD's preview and jump; `focus` on Terminator; `herdr --remote ssh` from the
PC against agents seagents started, with the window sizes differing; and a
real reboot through `restore`.

## Cutover shape

Per [`CLAUDE.md`](../CLAUDE.md), no wrapper and herdr paths side by side. The wrapper is the
hub that `status`, `hud`, `send`, `stop`, `wait` and `spawn` all hang off, so
those cannot move one at a time: they move **together, in one change**, and
the wrapper, the vendored detector and the control socket are deleted in that
same change, with `HANDOFF.md` carried over. No unit tests (user decision,
2026-10-12): the existing `#[cfg(test)]` modules are removed in a change of
their own before the cutover, and new paths are verified by running them.
The `tools/` end-to-end scripts stay and are updated for the new design. `read` and the
transcript layer are untouched.

**Decided (2026-10-12): full rewrite, one cutover** — done that way.

## Later

- `seagents hud --machine laptop` (or `status`/`wait`/`send` with it): the
  same commands against the laptop from the PC, through herdr's
  `--machine` API forwarding. Needs a saved herdr machine profile.
