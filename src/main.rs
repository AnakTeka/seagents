//! seagents — observe coding agents running in your own terminal windows.

use std::fs;
use std::thread;

mod clock;
mod detect;
mod hud;
mod i3;
mod launch;
mod procfs;
mod registry;
mod snapshot;
mod status;
mod vendor;
mod window;
mod wrapper;

fn main() {
    let mut args = std::env::args();
    let _bin = args.next();
    match args.next().as_deref() {
        Some("run") => {
            // `--window <handle>` (before the agent's own argv) names the i3
            // window this agent will live in. `restore` passes it because a
            // window it spawned itself may not be focused yet when we look —
            // see `window_ids_for_handle`.
            let mut window: Option<String> = None;
            let mut cmd: Vec<String> = Vec::new();
            let mut rest = args.peekable();
            while let Some(arg) = rest.peek() {
                match arg.as_str() {
                    "--window" => {
                        rest.next();
                        match rest.next() {
                            Some(r) => window = Some(r),
                            None => {
                                eprintln!("seagents run: --window needs a value");
                                std::process::exit(2);
                            }
                        }
                    }
                    "--" => {
                        rest.next();
                        break;
                    }
                    _ => break,
                }
            }
            cmd.extend(rest);
            if cmd.is_empty() {
                eprintln!("usage: seagents run [--window <handle>] <cmd> [args...]");
                std::process::exit(2);
            }
            std::process::exit(wrapper::run(cmd, window));
        }
        // Hidden: the in-PTY job-control anchor `run` wraps around the agent.
        // The PTY session leader must not BE the agent: a lone process group in
        // its own session is "orphaned" and the kernel silently discards the
        // SIGTSTP it raises on Ctrl+Z. The anchor forks the agent into a second
        // process group (de-orphaning it), then relays stops/exits upward.
        Some("__lead") => {
            let cmd: Vec<String> = args.collect();
            if cmd.is_empty() {
                std::process::exit(2);
            }
            std::process::exit(wrapper::lead(cmd));
        }
        Some("status") => {
            let mut json = false;
            let mut sort = status::Sort::Urgency;
            for a in args {
                match a.as_str() {
                    "--json" => json = true,
                    s if s.starts_with("--sort=") => match status::Sort::parse(&s[7..]) {
                        Some(parsed) => sort = parsed,
                        None => {
                            eprintln!("usage: seagents status [--json] [--sort=urgency|dir|recent]");
                            std::process::exit(2);
                        }
                    },
                    _ => {
                        eprintln!("usage: seagents status [--json] [--sort=urgency|dir|recent]");
                        std::process::exit(2);
                    }
                }
            }
            std::process::exit(if json {
                status::print_json(sort)
            } else {
                status::print(sort)
            });
        }
        Some("focus") => {
            let pid = args.next().and_then(|s| s.parse::<u64>().ok());
            match pid {
                Some(p) => std::process::exit(window::focus(p)),
                None => {
                    eprintln!("usage: seagents focus <child_pid>");
                    std::process::exit(2);
                }
            }
        }
        // Dev tool: render a .raw file and show the detection engine's verdict
        // (the equivalent of herdr's `agent explain`) for tuning against real
        // captures. Add --json for the full evaluated-rule breakdown.
        Some("debug-screen") => {
            let mut file = String::new();
            let mut dims: Vec<u16> = Vec::new();
            let mut json = false;
            let mut agent_name = "claude".to_string();
            for a in args {
                match a.as_str() {
                    "--json" => json = true,
                    s if s.starts_with("--agent=") => agent_name = s[8..].to_string(),
                    s => match s.parse::<u16>() {
                        Ok(n) => dims.push(n),
                        Err(_) => file = s.to_string(),
                    },
                }
            }
            let cols = dims.first().copied().unwrap_or(80);
            let rows = dims.get(1).copied().unwrap_or(24);
            let data = std::fs::read(&file).unwrap_or_default();
            let screen = detect::render(&data, rows, cols);
            let title = status::current_title(&data).unwrap_or_default();
            println!("{screen}");
            let explain = match vendor::herdr_detect::parse_agent_label(&agent_name) {
                Some(agent) => vendor::herdr_detect::manifest::explain_with_input(
                    agent,
                    vendor::herdr_detect::manifest::DetectionInput {
                        screen: &screen,
                        osc_title: &title,
                        osc_progress: "",
                    },
                ),
                None => {
                    eprintln!("unknown agent {agent_name:?}");
                    std::process::exit(2);
                }
            };
            println!("\n---");
            println!("osc_title = {title:?}");
            println!(
                "state = {} (manifest {} v{})",
                vendor::herdr_detect::manifest::agent_state_label(explain.state),
                explain
                    .source
                    .as_ref()
                    .map(|s| s.kind())
                    .unwrap_or("none"),
                explain.manifest_version.as_deref().unwrap_or("?"),
            );
            match &explain.matched_rule {
                Some(rule) => println!(
                    "matched_rule = {} (priority {}, region {})",
                    rule.id, rule.priority, rule.region
                ),
                None => println!(
                    "matched_rule = none ({})",
                    explain.fallback_reason.as_deref().unwrap_or("no fallback")
                ),
            }
            if let Some(warning) = &explain.warning {
                println!("warning = {warning}");
            }
            if json {
                let value = vendor::herdr_detect::manifest::explain_to_json_value(&explain);
                println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
            }
            std::process::exit(0);
        }
        // Refresh detection manifests from herdr.dev right now (the HUD also
        // does this in the background at most once a day).
        Some("update-manifests") => std::process::exit(update_manifests_now()),
        // Reboot survival: the layout snapshot is written continuously by
        // every wrapper, so these two are only ever "now" and "put it back".
        Some("snapshot") => {
            let rest: Vec<String> = args.collect();
            std::process::exit(snapshot::snapshot_cmd(&rest));
        }
        Some("restore") => {
            let rest: Vec<String> = args.collect();
            std::process::exit(snapshot::restore_cmd(&rest));
        }
        Some("hud") | None => std::process::exit(hud::run()),
        Some("help") | Some("-h") | Some("--help") => {
            usage();
            std::process::exit(0);
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!("seagents {}", env!("CARGO_PKG_VERSION"));
    eprintln!("usage:");
    eprintln!("  seagents [hud]                 the floating agent HUD (default)");
    eprintln!("  seagents run <cmd> [args...]   launch an agent, teeing its output");
    eprintln!("  seagents status [--json] [--sort=urgency|dir|recent]");
    eprintln!("                                 show every tracked agent's state");
    eprintln!("  seagents focus <child_pid>     focus that agent's i3 window");
    eprintln!("  seagents snapshot [--refresh] [--print]");
    eprintln!("                                 capture the agent layout now (also automatic)");
    eprintln!("  seagents restore [--dry-run] [--stagger=N] [--workspace=NAME]");
    eprintln!("                                 rebuild that layout, resuming each session");
    eprintln!("  seagents update-manifests      fetch fresh detection rules from herdr.dev");
}

/// Explicit `update-manifests`: fetch, report per-agent results, reload.
fn update_manifests_now() -> i32 {
    match vendor::herdr_detect::manifest_update::check_and_update() {
        Ok(output) => {
            reload_updated_manifests(&output);
            for (agent, remote) in &output.status.agents {
                let version = remote.cached_version.as_deref().unwrap_or("-");
                match &remote.last_error {
                    Some(err) => println!("{agent:<10} {:<8} {version}  {err}", remote.last_result),
                    None => println!("{agent:<10} {:<8} {version}", remote.last_result),
                }
            }
            0
        }
        Err(err) => {
            eprintln!("manifest update failed: {err}");
            1
        }
    }
}

/// Refresh the in-process manifest cache for every agent whose rules moved.
/// Not just the ones this run committed: herdr and seagents share one remote
/// cache directory, so a manifest the other tool fetched leaves `updated`
/// empty while the loaded rules are stale.
fn reload_updated_manifests(output: &vendor::herdr_detect::manifest_update::ManifestUpdateOutput) {
    let activated = vendor::herdr_detect::manifest_update::agents_needing_cache_reload(output);
    if !activated.is_empty() {
        vendor::herdr_detect::manifest::reload_manifests_for_agents(&activated);
    }
}

/// Background refresh for the HUD: at most once per day, silently.
/// `SEAGENTS_NO_UPDATE=1` disables the automatic phone-home entirely
/// (`seagents update-manifests` still works on demand).
pub(crate) fn spawn_manifest_update() {
    if std::env::var_os("SEAGENTS_NO_UPDATE").is_some() {
        return;
    }
    thread::spawn(|| {
        // Throttle on our own attempt stamp, not just the updater's recorded
        // last check: a failed catalog fetch never advances last_check_unix,
        // which would otherwise mean one curl attempt per HUD launch while
        // offline.
        let stamp = vendor::herdr_detect::config::state_dir()
            .join("agent-detection/last-attempt");
        let fresh_attempt = fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|e| e.as_secs() < 24 * 3600);
        if fresh_attempt {
            return;
        }
        if let Some(parent) = stamp.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&stamp, b"");
        if let Ok(output) = vendor::herdr_detect::manifest_update::check_and_update() {
            // The manifest cache is behind an RwLock; the HUD's next poll
            // picks up the new rules.
            reload_updated_manifests(&output);
        }
    });
}

/// One line of manifest-update health for the HUD footer / status output —
/// the vendored engine only improves while updates apply, and a persistent
/// failure (worst case: manifests requiring a newer engine after an upstream
/// bump) would otherwise rot silently behind SEAGENTS_DEBUG.
pub(crate) fn manifest_health_warning() -> Option<String> {
    let status = vendor::herdr_detect::manifest_update::load_status();
    let failing: Vec<&String> = status
        .agents
        .iter()
        .filter(|(_, s)| s.last_result == "failed")
        .map(|(agent, _)| agent)
        .collect();
    if failing.is_empty() {
        return None;
    }
    let engine_drift = status.agents.values().any(|s| {
        s.last_error
            .as_deref()
            .is_some_and(|e| e.contains("requires engine"))
    });
    Some(if engine_drift {
        "detection rules need a newer engine: re-sync src/vendor/herdr_detect (see VENDORED.md)"
            .to_string()
    } else {
        format!(
            "manifest update failing for {} agent(s), try `seagents update-manifests`",
            failing.len()
        )
    })
}
