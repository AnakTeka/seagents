# seagents

**Read [`HANDOFF.md`](./HANDOFF.md) first** — it is the full context for this
project (what it is, architecture, current status, gotchas, and next steps).

Quick facts:

- seagents = an observer/HUD for coding agents running in your own i3wm terminals
  (not a multiplexer). Commands: `run` / `status` / `focus` / `hud`.
- Build/test: `cargo build --release`, `cargo test`. Install:
  `install -Dm755 target/release/seagents ~/.local/bin/seagents`.
- The HUD and the pty relay need a real terminal — validate them by hand, not in a pipe.

Conventions:

- **Refactor by cutover, not by accretion.** When a feature needs existing code
  reshaped, reshape it and delete the old path in the same change. Do not leave
  the two side by side: no parallel implementation, no `_v2`/`_new` name, no
  compatibility shim, no flag that keeps the old behaviour reachable, no dead
  helper "in case". Move the shared thing to where it now belongs, update every
  call site, remove the duplicate, and carry the tests and `HANDOFF.md` over in
  the same commit — one source of truth per concern, at every commit.
  This is a private single-author project with no external consumers, so nothing
  here is owed a deprecation cycle: a half-finished cutover costs more than the
  full one. If a cutover turns out to be genuinely too large for the change in
  hand, say so and stop, rather than shipping the fork and leaving it.
- **Commits: do NOT add a Claude/AI co-author or "Generated with" trailer.** The
  user authors commits solely. Conventional-commit style (`fix(...)`, `feat(...)`).
- Default branch `main`; remote `git@github.com:AnakTeka/seagents.git` (private).
