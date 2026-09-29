//! seagents — observe coding agents running in your own terminal windows.
//!
//! This file is the command line and nothing else: the clap definition and a
//! dispatch to the module that owns each command.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

/// See which of your coding agents is blocked, done or working, and jump to
/// its i3 window.
#[derive(Parser, Debug)]
#[command(name = "seagents", version)]
struct Cli {
    /// With no command, opens the HUD.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// The floating agent HUD (also the default with no command)
    Hud,

    /// Launch an agent, teeing its output (this is how tracking happens)
    ///
    /// Everything after the command's first word is passed to it untouched,
    /// flags included: `seagents run claude --model opus`.
    Run {
        /// The handle of the i3 window this agent will live in. `restore`
        /// passes it for windows it launched itself, which may not be focused
        /// yet when the wrapper looks.
        #[arg(long, value_name = "HANDLE")]
        window: Option<String>,

        /// The agent's command line
        #[arg(
            value_name = "CMD",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        cmd: Vec<String>,
    },

    /// The in-PTY job-control anchor `run` wraps around the agent (internal)
    ///
    /// The PTY session leader must not BE the agent: a lone process group in
    /// its own session is "orphaned" and the kernel silently discards the
    /// SIGTSTP it raises on Ctrl+Z. The anchor forks the agent into a second
    /// process group (de-orphaning it), then relays stops/exits upward.
    #[command(name = "__lead", hide = true, disable_help_flag = true)]
    Lead {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },

    /// One-shot table of every tracked agent's state
    Status {
        /// Machine-readable output, for widget frontends (includes each
        /// agent's conversation title)
        #[arg(long)]
        json: bool,

        /// How to order the agents
        #[arg(long, value_enum, default_value_t = status::Sort::Urgency)]
        sort: status::Sort,
    },

    /// Focus that agent's i3 window
    Focus {
        /// The agent's `pid`, as `status --json` reports it
        child_pid: u64,
    },

    /// Capture the agent layout now (it is also captured continuously)
    Snapshot {
        /// Re-resolve every session's conversation id instead of trusting the
        /// cached one
        #[arg(long)]
        refresh: bool,

        /// Print the snapshot as JSON instead of a summary
        #[arg(long)]
        print: bool,
    },

    /// Rebuild the captured layout after a reboot, each window resuming its own
    /// conversation
    Restore {
        /// Show what would be restored, and launch nothing
        #[arg(short = 'n', long)]
        dry_run: bool,

        /// Seconds between window launches
        #[arg(long, value_name = "SECS", default_value_t = snapshot::DEFAULT_STAGGER_SECS)]
        stagger: u64,

        /// Restore only this workspace
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
    },

    /// Fetch fresh detection rules from herdr.dev now (the HUD also does this
    /// in the background at most once a day)
    UpdateManifests,

    /// Dev tool: render a .raw capture and show the detection engine's verdict
    DebugScreen {
        /// The `.raw` tee to render
        file: PathBuf,

        /// Columns to render at (`stty size < /proc/<pid>/fd/0` gives the
        /// live geometry)
        #[arg(default_value_t = 80)]
        cols: u16,

        /// Rows to render at
        #[arg(default_value_t = 24)]
        rows: u16,

        /// Which agent's rules to evaluate
        #[arg(long, default_value = "claude")]
        agent: String,

        /// Add the full evaluated-rule breakdown
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(dispatch(cli.command.unwrap_or(Command::Hud)));
}

fn dispatch(command: Command) -> i32 {
    match command {
        Command::Hud => hud::run(),
        Command::Run { window, cmd } => wrapper::run(cmd, window),
        Command::Lead { cmd } => wrapper::lead(cmd),
        Command::Status { json: true, sort } => status::print_json(sort),
        Command::Status { json: false, sort } => status::print(sort),
        Command::Focus { child_pid } => window::focus(child_pid),
        Command::Snapshot { refresh, print } => snapshot::snapshot_cmd(refresh, print),
        Command::Restore {
            dry_run,
            stagger,
            workspace,
        } => snapshot::restore_cmd(dry_run, stagger, workspace),
        Command::UpdateManifests => detect::update_manifests_now(),
        Command::DebugScreen {
            file,
            cols,
            rows,
            agent,
            json,
        } => detect::debug_screen(&file, cols, rows, &agent, json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args)
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .command
            .unwrap_or(Command::Hud)
    }

    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_command_is_the_hud() {
        assert!(matches!(parse(&["seagents"]), Command::Hud));
        assert!(matches!(parse(&["seagents", "hud"]), Command::Hud));
    }

    #[test]
    fn run_passes_the_agents_own_flags_through_untouched() {
        let Command::Run { window, cmd } =
            parse(&["seagents", "run", "claude", "--model", "x", "--window", "w", "-p", "--"])
        else {
            panic!("not run");
        };
        // --window after the agent's first word belongs to the agent.
        assert_eq!(window, None);
        assert_eq!(cmd, ["claude", "--model", "x", "--window", "w", "-p", "--"]);

        // Even --help, once the command has started.
        let Command::Run { cmd, .. } = parse(&["seagents", "run", "claude", "--help"]) else {
            panic!("not run");
        };
        assert_eq!(cmd, ["claude", "--help"]);

        // A command that itself starts with a hyphen is still the command.
        let Command::Run { cmd, .. } = parse(&["seagents", "run", "--model", "x"]) else {
            panic!("not run");
        };
        assert_eq!(cmd, ["--model", "x"]);
    }

    #[test]
    fn run_takes_window_before_the_command_in_both_spellings() {
        for args in [
            &["seagents", "run", "--window", "seagents-ab", "--", "claude", "--resume", "id"][..],
            &["seagents", "run", "--window=seagents-ab", "claude", "--resume", "id"][..],
        ] {
            let Command::Run { window, cmd } = parse(args) else {
                panic!("not run");
            };
            assert_eq!(window.as_deref(), Some("seagents-ab"));
            assert_eq!(cmd, ["claude", "--resume", "id"]);
        }
    }

    #[test]
    fn run_and_lead_need_a_command() {
        for args in [
            &["seagents", "run"][..],
            &["seagents", "run", "--window", "h"][..],
            &["seagents", "__lead"][..],
        ] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn lead_passes_everything_through() {
        let Command::Lead { cmd } = parse(&["seagents", "__lead", "bash", "--norc", "--help"])
        else {
            panic!("not __lead");
        };
        assert_eq!(cmd, ["bash", "--norc", "--help"]);
    }

    #[test]
    fn status_sort_accepts_both_spellings_and_rejects_nonsense() {
        for args in [
            &["seagents", "status", "--sort=dir"][..],
            &["seagents", "status", "--sort", "dir"][..],
        ] {
            let Command::Status { json, sort } = parse(args) else {
                panic!("not status");
            };
            assert!(!json);
            assert!(sort == status::Sort::Dir);
        }
        let Command::Status { json, sort } = parse(&["seagents", "status", "--json"]) else {
            panic!("not status");
        };
        assert!(json);
        assert!(sort == status::Sort::Urgency);
        let err = Cli::try_parse_from(["seagents", "status", "--sort=nope"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn sort_words_are_what_the_flag_accepts() {
        // The HUD persists Sort::word() and reads it back with Sort::parse;
        // both must stay the spelling `--sort` takes.
        use clap::ValueEnum;
        for sort in status::Sort::value_variants() {
            assert!(status::Sort::parse(sort.word()) == Some(*sort));
        }
    }

    #[test]
    fn restore_flags() {
        let Command::Restore {
            dry_run,
            stagger,
            workspace,
        } = parse(&["seagents", "restore"])
        else {
            panic!("not restore");
        };
        assert!(!dry_run);
        assert_eq!(stagger, snapshot::DEFAULT_STAGGER_SECS);
        assert_eq!(workspace, None);

        let Command::Restore {
            dry_run,
            stagger,
            workspace,
        } = parse(&["seagents", "restore", "-n", "--stagger=0", "--workspace=9: web"])
        else {
            panic!("not restore");
        };
        assert!(dry_run);
        assert_eq!(stagger, 0);
        assert_eq!(workspace.as_deref(), Some("9: web"));

        let err = Cli::try_parse_from(["seagents", "restore", "--stagger=soon"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn debug_screen_takes_file_then_geometry() {
        let Command::DebugScreen {
            file,
            cols,
            rows,
            agent,
            json,
        } = parse(&["seagents", "debug-screen", "x.raw", "212", "53", "--agent=codex", "--json"])
        else {
            panic!("not debug-screen");
        };
        assert_eq!(file, PathBuf::from("x.raw"));
        assert_eq!((cols, rows), (212, 53));
        assert_eq!(agent, "codex");
        assert!(json);

        let Command::DebugScreen { cols, rows, agent, .. } =
            parse(&["seagents", "debug-screen", "x.raw"])
        else {
            panic!("not debug-screen");
        };
        assert_eq!((cols, rows), (80, 24));
        assert_eq!(agent, "claude");
    }

    #[test]
    fn unknown_commands_and_bad_arguments_are_usage_errors() {
        for args in [
            &["seagents", "frobnicate"][..],
            &["seagents", "focus"][..],
            &["seagents", "focus", "abc"][..],
            &["seagents", "snapshot", "--bogus"][..],
        ] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
    }
}
