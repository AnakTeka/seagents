# seagents on herdr — spec

Design spec for moving seagents' agents out of its own pty wrapper and into a
**herdr server**, with each i3 terminal becoming a *view* of one agent.

Status: **spec only, nothing implemented.** Written 2026-10-12 on branch
`refactor/phase0`. The approach was proven by hand the same day (see
*Verified*); everything else here is a plan with its open questions marked.

Related: [`HANDOFF.md`](./HANDOFF.md) (what is built), [`ORCHESTRATION-SPEC.md`](./ORCHESTRATION-SPEC.md)
(spawn / wait / send / stop / read, which this reshapes).

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
| state detection (`detect.rs`, `vendor/herdr_detect/`, `update-manifests`) | **deleted** → `herdr agent list/get` | loses `manifest/patches.rs` (our Codex trust-dialog rule): must go upstream or be re-checked |
| `status` / `hud` rows | herdr state + our window records | herdr's `idle`/`done`/`working`/`blocked`/`unknown` |
| `send` / `stop` (control socket, lineage rule, submit-when-idle, input-box check) | `herdr agent prompt` / `send-keys`, behind our idle hold and draft check | lineage rule deleted (question 4) |
| `wait` (hook turn ledger, §8) | `herdr agent wait`, or keep the ledger | herdr waits on lifecycle state, not a turn; see open questions |
| `read` (transcript, §10) | **kept** | reads the agent's own JSONL; independent of the wrapper |
| `spawn` (§7) | `herdr agent start` + an i3 window | caps and permission bounding deleted with lineage |
| `focus` (§4) | kept | the window is the attach window; identity recorded at `run` |
| `snapshot` / `restore` (§6) | kept, reduced | herdr has its own persistence (`src/persist/`, `agent_resume.rs`); restore may only need to reopen attach windows |
| hooks (`hook.rs`, `[hooks] inject`) | open | herdr ships its own Claude/Codex integrations (`herdr integration`) |
| Codex adapter | kept for transcript/spawn bits | herdr already identifies Codex |
| session cache (`registry.rs`) | reshaped | keyed by herdr agent name / pane id, not wrapper pid |

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
     `stop` reads the screen through herdr and refuses to type `/exit` while
     "Do you trust the contents of this directory?" is showing. herdr's
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

## Cutover shape

Per `CLAUDE.md`, no wrapper and herdr paths side by side. The wrapper is the
hub that `status`, `hud`, `send`, `stop`, `wait` and `spawn` all hang off, so
those cannot move one at a time: they move **together, in one change**, and
the wrapper, the vendored detector and the control socket are deleted in that
same change, with `HANDOFF.md` carried over. No unit tests (user decision,
2026-10-12): the existing `#[cfg(test)]` modules are removed in a change of
their own before the cutover, and new paths are verified by running them.
The `tools/` end-to-end scripts stay and are updated for the new design. `read` and the
transcript layer are untouched.

**Decided (2026-10-12): full rewrite, one cutover.** The size of the change
is accepted; do not shrink it, stage it behind flags or keep the wrapper
alongside. Before starting:
- settle the open questions above;
- run one more hand test of the hardest item (3 or 4) against herdr.

## Later

- `seagents hud --machine laptop` (or `status`/`wait`/`send` with it): the
  same commands against the laptop from the PC, through herdr's
  `--machine` API forwarding. Needs a saved herdr machine profile.
