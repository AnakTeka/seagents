# Vendored: herdr's agent-detection engine

- **Upstream:** https://github.com/ogulcancelik/herdr — `src/detect/`
- **Vendored from:** commit `cc88b3b8e5bb9f7d9f23ed6ae85a52fd7b5b9ed6` (2026-09-02,
  post-v0.8.2 master; previously v0.7.3 `77f0339c`)
- **License:** AGPL-3.0-or-later (herdr is dual-licensed; we use it under AGPL).
  Including this code requires seagents to remain AGPL-3.0-or-later.
- **Engine version:** 3 (`MANIFEST_ENGINE_VERSION` in `manifest_update.rs`)

## What this is

herdr's manifest-driven screen-state detection: per-agent TOML rule manifests
(`manifests/*.toml`, ~21 agents) evaluated by a region/priority rule engine
(`manifest.rs`), with a self-updater (`manifest_update.rs`) that fetches newer
manifests from `https://herdr.dev/agent-detection/index.toml` (via `curl`) and
caches them under `state_dir()/agent-detection/`. Rules update upstream without
any code change here; only a `min_engine_version` bump beyond 3 requires a
re-sync of this directory.

Local overrides: drop a manifest at `~/.config/seagents/agent-detection/<agent>.toml`
to shadow both remote and bundled rules (herdr's hot-tuning workflow).

Rule patches: `manifest/patches.rs` (seagents-local, not vendored) re-weights
individual upstream rules by id as each bundled/remote manifest loads — the way
to disagree with one rule without freezing the file behind an override. An
explicit override is applied literally, patches skipped.

## Local modifications (re-apply after any re-sync)

Substantive deltas are marked with `// seagents:` comments in the code; the
mechanical path retargets (`crate::config::`/`crate::detect::` → `super::…`,
applied throughout all files including tests) and the added
`#[allow(dead_code)]` attributes are not individually marked — this list is
their record.

- `mod.rs`: removed the process-identification half (herdr's
  `crate::platform`-coupled `ForegroundJob` machinery and its tests) — seagents
  spawns its agents itself and maps argv basename via `identify_agent()`.
  Removed herdr-internal `full_lifecycle_hook_authority()` and
  `session_identity_only_integration()`, plus the agent-launch surface seagents
  never calls (`Agent::ALL`, `interactive_agent_executable()`) and the tests
  covering them. Added `pub mod config;`; `#[allow(dead_code)]` on
  `should_skip_state_update`.
- `config.rs`: **not vendored** — seagents-local replacement for herdr's
  `crate::config` (`config_dir()`/`state_dir()` under seagents' XDG paths).
- `manifest.rs`: `crate::config::` → `super::config::` (one site); added
  `mod patches;` and the `patches::apply()` call in `loaded_manifest()`
  (skipped for `ManifestSource::Override`);
  `#[allow(dead_code)]` on `manifest_summaries`, `explain`, `explain_for_label`
  and `reload_manifests` (unused vendored API surface, kept for parity — the
  binary reloads per agent via `reload_manifests_for_agents`).
- `manifest_update.rs`: removed `auto_update()` (tokio/AppEvent coupling) —
  callers use `check_and_update()` + `agents_needing_cache_reload()` (raised to
  `pub(crate)`) + `manifest::reload_manifests_for_agents()` directly, which is
  the work `auto_update()` did around its AppEvent; its three tokio/AppEvent
  tests rewritten to drive `check_and_update()` directly;
  `tracing::warn!` → local `warn_log!` (stderr, gated on `SEAGENTS_DEBUG`);
  `crate::config::` → `super::config::`; `fetch_text` keeps plain
  `std::process::Command` (upstream moved to its Windows window-hiding wrapper)
  and pins https catalogs to https across redirects
  (`--proto '=https' --proto-redir '=https'`).
- `manifest/tests.rs`: `crate::config::`/`crate::detect::` paths retargeted
  into this module tree.
- `manifest/patches.rs`: **not vendored** — seagents' rule re-weighting table.
  Re-check every entry against the new rules on a re-sync.

## Re-sync procedure

1. Get the reference clone (gitignored, absent on fresh machines):
   `git clone https://github.com/ogulcancelik/herdr && git -C herdr checkout <new pin>`
   — then diff `herdr/src/detect/` against this directory.
2. Re-copy upstream files and re-apply the modifications above (all
   mechanical), and update the pinned commit at the top of this file.
3. Re-check `manifest/patches.rs`: each entry names an upstream rule id, and a
   rule that was renamed, removed or reshaped upstream needs its patch revisited
   (`patches_only_touch_their_own_manifest` catches an id that no longer exists).
4. `cargo test`, then live-verify with `seagents debug-screen`.
