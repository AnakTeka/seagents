//! seagents-local replacement for herdr's `crate::config` paths (NOT vendored).
//!
//! The vendored detection code needs two directories:
//! - `config_dir()/agent-detection/<agent>.toml` — local manifest overrides
//! - `state_dir()/agent-detection/`              — cached remote manifests + update status
//!
//! We point both at seagents' own XDG locations so we never contend with a
//! herdr installation's files (both tools update from the same catalog URL).

use std::path::PathBuf;

fn xdg_dir(env: &str, home_fallback: &str) -> PathBuf {
    std::env::var_os(env)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(home_fallback)
        })
        .join("seagents")
}

/// `$XDG_CONFIG_HOME/seagents` (default `~/.config/seagents`).
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_STATE_HOME/seagents` (default `~/.local/state/seagents`).
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}
