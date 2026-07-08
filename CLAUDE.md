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

- **Commits: do NOT add a Claude/AI co-author or "Generated with" trailer.** The
  user authors commits solely. Conventional-commit style (`fix(...)`, `feat(...)`).
- Default branch `main`; remote `git@github.com:AnakTeka/seagents.git` (private).
