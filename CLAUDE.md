# seagents

**Read [`HANDOFF.md`](./HANDOFF.md) first** — it is the full context for this
project (what it is, architecture, current status, gotchas, and next steps).

Quick facts:

- seagents = the i3 integration for herdr: agents run in a herdr server (which
  owns their terminals, detects their state, and is reachable with
  `herdr --remote ssh <host>`), each shown in its own i3 window
  (`seagents run claude` → `herdr terminal attach`), plus a HUD, focus,
  snapshot/restore and orchestration (`spawn` / `wait` / `send` / `stop` /
  `read`). Requires herdr ≥ 0.9.0. All herdr calls go through `src/herdr.rs`.
- Build: `cargo build --release`. Install:
  `install -Dm755 target/release/seagents ~/.local/bin/seagents`.
- The HUD and an agent's window need a real terminal — validate them by hand
  (or a pty harness), not in a pipe. Experiment against an isolated herdr
  session (`HERDR_SESSION=<name>`), never the default one.

Conventions:

- **Refactor by cutover, not by accretion.** When a feature needs existing code
  reshaped, reshape it and delete the old path in the same change. Do not leave
  the two side by side: no parallel implementation, no `_v2`/`_new` name, no
  compatibility shim, no flag that keeps the old behaviour reachable, no dead
  helper "in case". Move the shared thing to where it now belongs, update every
  call site, remove the duplicate, and carry `HANDOFF.md` over in the same
  commit — one source of truth per concern, at every commit.
  This is a private single-author project with no external consumers, so nothing
  here is owed a deprecation cycle: a half-finished cutover costs more than the
  full one.
- **No unit tests.** Don't write `#[cfg(test)]` tests (the existing ones were
  removed). Verify behaviour by running it. The end-to-end scripts in
  `tools/` (`test-orchestration.sh`, `test-restore.sh`, …) are not unit tests
  and stay.
- **Refactors, cutovers and deleting code are pre-approved.** Size is not a
  reason to hold back: rewriting a subsystem, cutting a whole command over to a
  new design, or deleting modules, commands and their tests (including recent,
  working code) needs no permission. Don't keep code because effort went into
  it, and don't propose a smaller, staged or flag-gated version to avoid a big
  change. When unsure whether to keep or delete something superseded, delete
  it. Name the notable deletions in the summary so they are visible.
- **Commits: do NOT add a Claude/AI co-author or "Generated with" trailer.** The
  user authors commits solely. Conventional-commit style (`fix(...)`, `feat(...)`).
- Default branch `main`; remote `git@github.com:AnakTeka/seagents.git` (private).
