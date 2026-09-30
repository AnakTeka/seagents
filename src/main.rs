//! seagents — observe coding agents running in your own terminal windows.
//!
//! This file is the command line and nothing else: the clap definition and a
//! dispatch to the module that owns each command.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod adapter;
mod clock;
mod detect;
mod fsutil;
mod hook;
mod hud;
mod i3;
mod launch;
mod procfs;
mod read;
mod registry;
mod send;
mod snapshot;
mod spawn;
mod status;
mod transcript;
mod turn;
mod vendor;
mod wait;
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
        /// The agent's name ([a-z0-9][a-z0-9-]{0,31}, not all digits); minted
        /// as `<cwd basename>-<4 hex>` when omitted. Refused (exit 2) while a
        /// live agent holds it. A terminal seagents opened carries the handle
        /// `seagents-<TAG>`, which is how the wrapper finds its own window.
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,

        /// The spawning agent's tag; must be live. Sets this agent's lineage
        /// (depth = the parent's + 1), within the same fan-out caps as `spawn`
        /// (exit 2 past them).
        #[arg(long, value_name = "TAG")]
        parent: Option<String>,

        /// A prompt to start the agent on (as `spawn` writes it): read,
        /// deleted, and passed per the agent's adapter — for Claude, as a
        /// positional prompt after `--` on the agent's command line, never in
        /// the command line recorded for `restore`
        #[arg(long, value_name = "FILE")]
        seed_file: Option<PathBuf>,

        /// The agent's command line
        #[arg(
            value_name = "CMD",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        cmd: Vec<String>,
    },

    /// Open a new terminal running an agent on a prompt; prints its handle
    ///
    /// Prints `{"tag","child_pid","wrapper_pid","cwd","parent","depth","stops"}`
    /// once the agent is registered (not once it has started working: follow
    /// with `seagents wait <tag> --after 0`). Run from inside an agent, the
    /// child is that agent's: its `parent` and depth come from the caller's
    /// process ancestry, fan-out is capped (depth 3, 4 live children per
    /// parent, 12 spawned in all; the user sets them in
    /// ~/.config/seagents/config.toml, [spawn] max_depth / max_children /
    /// max_spawned), and its permission mode never ranks above the caller's. The window closes when the agent
    /// exits. Exit 1, with the reason on stderr, on any refusal.
    Spawn {
        /// Directory to run the agent in (default: this one)
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,

        /// The new agent's name (minted as `<dir>-<4 hex>` when omitted);
        /// refused while live, reserved by another spawn, or recorded in the
        /// saved layout
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,

        /// Which agent to launch
        #[arg(long, value_name = "AGENT", default_value = "claude")]
        agent: String,

        /// Permission mode for the agent: plan, default, acceptEdits or
        /// bypassPermissions (Codex gets the matching -a/-s pair). From an
        /// agent: default the caller's own, never above it. From a human:
        /// passed only if given
        #[arg(long, value_name = "MODE")]
        permission_mode: Option<String>,

        /// Run in a new git worktree, <repo>/.seagents/worktrees/<TAG> on
        /// branch seagents/<TAG> — without any of the caller's uncommitted
        /// changes, and with a cold build directory
        #[arg(long)]
        worktree: bool,

        /// One extra argument for the agent, repeatable:
        /// `--arg --model --arg haiku`. From an agent, none that sets
        /// permissions or sandboxing (the adapter's `privileged` list, e.g.
        /// --dangerously-skip-permissions, --settings, Codex's -a/-s/-c
        /// approval_policy=…): use --permission-mode
        #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
        args: Vec<String>,

        /// Read the prompt from this file
        #[arg(long, value_name = "FILE", conflicts_with = "prompt")]
        prompt_file: Option<PathBuf>,

        /// The prompt (one argument; `--` before it if it starts with `-`)
        #[arg(value_name = "PROMPT", required_unless_present = "prompt_file")]
        prompt: Option<String>,
    },

    /// Block until an agent ends a turn, is blocked, or exits; prints the outcome
    ///
    /// Prints `{"tag","reason","stops","state","rule","last_message","transcript_path"}`,
    /// plus `"message"` when the reason alone doesn't say what to do.
    /// `reason` and the exit code: `turn` 0 (stops passed N; `last_message` is
    /// that turn's final text), `idle` 0 (no --after and not in a turn: the last
    /// result, at once), `timeout` 2 (call again with the same N), `blocked` 3
    /// (waiting on a human; `rule` names the gate), `exit` 4 (also for an
    /// agent that exited earlier, for a day), `interrupted` 5 (the turn after
    /// N was cut short, with no final message), `no_hooks` 6 (the agent was
    /// given a prompt at least 20 s ago and its hooks have never reported, so no turn
    /// will be seen to end; `message` says which adapter and where to look);
    /// 1 is an error. Never marks the agent's result as seen.
    Wait {
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid`
        target: String,

        /// Wait for the turn after this `stops` count (0 after `spawn`).
        /// Without it: the running turn, or return at once when idle
        #[arg(long, value_name = "N")]
        after: Option<u64>,

        /// `exit` waits for the agent to exit, and nothing else
        #[arg(long = "for", value_enum, default_value_t = wait::For::Turn)]
        what: wait::For,

        /// Give up after this long: 30s, 5m, 1h, or seconds
        #[arg(long, value_name = "DUR", default_value = "5m", value_parser = wait::parse_duration)]
        timeout: std::time::Duration,
    },

    /// Submit text to an agent as a new turn; prints the baseline for `wait`
    ///
    /// Prints `{"ok","tag","stops","paired"}`: `wait <tag> --after <stops>`
    /// then waits for the turn this started. By default the text is held
    /// until the agent is idle (out of its turn, the screen idle), so the
    /// next turn to end is the one it opened (`paired: true`). A blocked agent
    /// (a permission prompt, a trust dialog) is refused at once, with the
    /// gate's `rule`. From inside an agent, only that agent's own descendants
    /// can be sent to. The text goes in as a bracketed paste when the agent
    /// takes one, then Enter 300 ms later. Text an agent would take as
    /// quoted data rather than as your words (Claude: over 700 characters or
    /// 2 lines) is written to `<its cwd>/.seagents/prompts/` instead, and it
    /// is sent one line telling it to read that file. A paste lands after
    /// whatever is in the agent's input box, so that box must be empty or
    /// hold a prompt seagents sent that the agent put back (Esc'd before its
    /// first response), which is cleared first; anything else is someone's
    /// draft and is refused, --now too. Refusals print
    /// `{"ok":false,"reason","error"[,"rule"]}` and exit: 3 blocked, 2
    /// busy_timeout (still busy at --timeout; nothing was sent), 4 exit, 5
    /// forbidden, 6 draft (text someone typed is in the input box; nothing
    /// was sent), 1 anything else (incl. `unsupported`: no detector knows
    /// the agent, so only --now can send to it).
    Send {
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid`
        target: String,

        /// Submit at once, even mid-turn (the agent may fold it into the
        /// running turn: `paired: false`). Never past a gate
        #[arg(long)]
        now: bool,

        /// Write TEXT as raw keys, not a paste. Escapes: \e (Esc), \r, \n,
        /// \t, \0, \\, \xNN; e.g. '\e[B' is Down
        #[arg(long)]
        keys: bool,

        /// Don't press Enter after the text
        #[arg(long)]
        no_submit: bool,

        /// Write even though the agent is blocked at a gate — to answer it
        /// (usually with --keys). Humans only: refused from inside an agent
        #[arg(long)]
        answer_permission: bool,

        /// How long to wait for the agent to be idle: 30s, 5m, 1h, or seconds
        #[arg(long, value_name = "DUR", default_value = "5m", value_parser = wait::parse_duration)]
        timeout: std::time::Duration,

        /// Read the text from this file
        #[arg(long, value_name = "FILE", conflicts_with = "text")]
        prompt_file: Option<PathBuf>,

        /// The text (one argument; `--` before it if it starts with `-`)
        #[arg(value_name = "TEXT", required_unless_present = "prompt_file")]
        text: Option<String>,
    },

    /// Make an agent exit: its exit command, then SIGTERM, then SIGKILL
    ///
    /// Types the agent's exit command (Claude: `/exit`) and gives it 5 s;
    /// then SIGTERMs its process group, then SIGKILLs it, 5 s apart. A
    /// blocked agent skips the exit command (typed into a permission menu,
    /// its Enter would pick an answer), and so does one whose input box holds
    /// a draft someone typed (the command would be submitted as part of it);
    /// a prompt seagents sent that the agent put back is cleared first.
    /// Prints `{"ok":true,"tag"}` once the agent is gone. From inside an
    /// agent, only its own descendants. Exit 5 forbidden, 1 any other failure
    /// — including an agent that had already exited (no live agent by that
    /// name).
    Stop {
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid`
        target: String,

        /// Skip the exit command: SIGTERM at once
        #[arg(long)]
        force: bool,
    },

    /// Print an agent's conversation — its main line — ending with a cursor
    ///
    /// Read from the agent's own transcript (the path its hooks reported),
    /// or any transcript file with --transcript. Abandoned branches (a
    /// rewind, an edit) are left out. Text: the user's words behind a bar,
    /// the assistant's in full, thinking, tool calls and results one line
    /// each (--raw: in full), subagents as `▸ subagent: <type> —
    /// <description>`; the last line is `-- cursor: <offset>:<leaf>:<path>`.
    /// --json: one record per line, `{"uuid","parent_uuid","timestamp",
    /// "kind",…}` with kind user|assistant|thinking (`text`), tool_use
    /// (`id`,`name`,`input`[,`subagent`]) or tool_result
    /// (`tool_use_id`,`text`,`bytes`[,`is_error`]), then
    /// `{"cursor":{"transcript_path","offset","leaf","token"}}`. With --from,
    /// only what came after that cursor — preceded by
    /// `{"rewound":{"fork"}}` when the conversation was rewound past it
    /// (continuing from the fork), or `{"reset":{"transcript_path"}}` when
    /// the transcript changed (`/clear`; read from its start). Exit 1, with
    /// the reason on stderr, when there is nothing to read.
    Read {
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid`
        #[arg(required_unless_present = "transcript", conflicts_with = "transcript")]
        target: Option<String>,

        /// Read this transcript file instead of an agent's
        #[arg(long, value_name = "PATH")]
        transcript: Option<PathBuf>,

        /// NDJSON records instead of text
        #[arg(long)]
        json: bool,

        /// Only what came after this cursor (a previous read's last line, its
        /// token, or its JSON cursor record)
        #[arg(long, value_name = "CURSOR", allow_hyphen_values = true)]
        from: Option<String>,

        /// Only the last N records
        #[arg(long, value_name = "N")]
        tail: Option<usize>,

        /// Don't collapse thinking, tool input or tool output
        #[arg(long)]
        raw: bool,
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

    /// The agent's lifecycle hook `run` injects (internal)
    ///
    /// Reads one hook event (JSON) on stdin and records it in the enclosing
    /// session's `.agent`. Always exits 0 and prints nothing: a nonzero `Stop`
    /// hook changes what the agent does. Any arguments are ignored, so no
    /// command line can make it a usage error.
    #[command(name = "__hook", hide = true, disable_help_flag = true, disable_version_flag = true)]
    Hook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        _ignored: Vec<String>,
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
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid` as
        /// `status --json` reports it
        target: String,
    },

    /// Capture the agent layout now (it is also captured continuously)
    Snapshot {
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
        Command::Run {
            tag,
            parent,
            seed_file,
            cmd,
        } => wrapper::run(cmd, tag, parent, seed_file),
        Command::Spawn {
            cwd,
            tag,
            agent,
            permission_mode,
            worktree,
            args,
            prompt_file,
            prompt,
        } => spawn::run(spawn::Args {
            cwd,
            tag,
            agent,
            permission_mode,
            worktree,
            args,
            prompt_file,
            prompt,
        }),
        Command::Wait {
            target,
            after,
            what,
            timeout,
        } => wait::run(&target, after, what, timeout),
        Command::Send {
            target,
            now,
            keys,
            no_submit,
            answer_permission,
            timeout,
            prompt_file,
            text,
        } => send::send(send::SendArgs {
            target,
            now,
            keys,
            no_submit,
            answer_permission,
            timeout,
            prompt_file,
            text,
        }),
        Command::Stop { target, force } => send::stop(&target, force),
        Command::Read {
            target,
            transcript,
            json,
            from,
            tail,
            raw,
        } => read::run(read::Args {
            target,
            transcript,
            json,
            from,
            tail,
            raw,
        }),
        Command::Lead { cmd } => wrapper::lead(cmd),
        Command::Hook { .. } => hook::run(),
        Command::Status { json: true, sort } => status::print_json(sort),
        Command::Status { json: false, sort } => status::print(sort),
        Command::Focus { target } => window::focus(&target),
        Command::Snapshot { print } => snapshot::snapshot_cmd(print),
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
        let Command::Run { tag, cmd, .. } =
            parse(&["seagents", "run", "claude", "--model", "x", "--tag", "w", "-p", "--"])
        else {
            panic!("not run");
        };
        // --tag after the agent's first word belongs to the agent.
        assert_eq!(tag, None);
        assert_eq!(cmd, ["claude", "--model", "x", "--tag", "w", "-p", "--"]);

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
    fn run_takes_tag_and_parent_before_the_command_in_both_spellings() {
        for args in [
            &["seagents", "run", "--tag", "ab", "--parent", "p", "--", "claude", "--resume", "id"][..],
            &["seagents", "run", "--parent=p", "--tag=ab", "claude", "--resume", "id"][..],
        ] {
            let Command::Run { tag, parent, cmd, .. } = parse(args) else {
                panic!("not run");
            };
            assert_eq!(tag.as_deref(), Some("ab"));
            assert_eq!(parent.as_deref(), Some("p"));
            assert_eq!(cmd, ["claude", "--resume", "id"]);
        }
        let Command::Run { seed_file, cmd, .. } =
            parse(&["seagents", "run", "--tag", "t", "--seed-file", "/r/seed-t", "--", "claude"])
        else {
            panic!("not run");
        };
        assert_eq!(seed_file, Some(PathBuf::from("/r/seed-t")));
        assert_eq!(cmd, ["claude"]);
    }

    #[test]
    fn spawn_takes_agent_args_with_hyphens_and_a_prompt_or_a_file() {
        let Command::Spawn { args, prompt, agent, tag, .. } = parse(&[
            "seagents", "spawn", "--tag", "it-a", "--arg", "--model", "--arg=haiku", "--", "-say: ALPHA",
        ]) else {
            panic!("not spawn");
        };
        assert_eq!(args, ["--model", "haiku"]);
        assert_eq!(prompt.as_deref(), Some("-say: ALPHA"));
        assert_eq!((agent.as_str(), tag.as_deref()), ("claude", Some("it-a")));

        let Command::Spawn { prompt_file, prompt, worktree, .. } =
            parse(&["seagents", "spawn", "--worktree", "--prompt-file", "p.md"])
        else {
            panic!("not spawn");
        };
        assert_eq!((prompt_file, prompt, worktree), (Some(PathBuf::from("p.md")), None, true));

        for bad in [
            &["seagents", "spawn"][..],
            &["seagents", "spawn", "--prompt-file", "p", "hi"][..],
            &["seagents", "spawn", "two", "words"][..],
        ] {
            assert_eq!(Cli::try_parse_from(bad).unwrap_err().exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn wait_defaults_to_the_running_turn_for_five_minutes() {
        let Command::Wait { target, after, what, timeout } = parse(&["seagents", "wait", "@it-a"]) else {
            panic!("not wait");
        };
        assert_eq!((target.as_str(), after, what), ("@it-a", None, wait::For::Turn));
        assert_eq!(timeout, std::time::Duration::from_secs(300));
        let Command::Wait { after, what, timeout, .. } =
            parse(&["seagents", "wait", "it-a", "--after", "0", "--for=exit", "--timeout", "90s"])
        else {
            panic!("not wait");
        };
        assert_eq!((after, what, timeout.as_secs()), (Some(0), wait::For::Exit, 90));
        for bad in [&["seagents", "wait"][..], &["seagents", "wait", "x", "--timeout", "soon"][..],
                    &["seagents", "wait", "x", "--for", "done"][..]] {
            assert_eq!(Cli::try_parse_from(bad).unwrap_err().exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn send_takes_text_or_a_file_and_waits_five_minutes_by_default() {
        let Command::Send { target, now, keys, no_submit, answer_permission, timeout, prompt_file, text } =
            parse(&["seagents", "send", "it-b", "Reply with exactly: BRAVO"])
        else {
            panic!("not send");
        };
        assert_eq!((target.as_str(), text.as_deref(), prompt_file), ("it-b", Some("Reply with exactly: BRAVO"), None));
        assert_eq!((now, keys, no_submit, answer_permission), (false, false, false, false));
        assert_eq!(timeout, std::time::Duration::from_secs(300));
        let Command::Send { now, keys, no_submit, answer_permission, timeout, text, .. } = parse(&[
            "seagents", "send", "@it-b", "--now", "--keys", "--no-submit", "--answer-permission", "--timeout=30s",
            "--", "-1",
        ]) else {
            panic!("not send");
        };
        assert_eq!((now, keys, no_submit, answer_permission, timeout.as_secs()), (true, true, true, true, 30));
        assert_eq!(text.as_deref(), Some("-1"));
        for bad in [
            &["seagents", "send", "x"][..],
            &["seagents", "send", "x", "--prompt-file", "f", "text"][..],
            &["seagents", "send"][..],
        ] {
            assert_eq!(Cli::try_parse_from(bad).unwrap_err().exit_code(), 2, "{bad:?}");
        }
        let Command::Stop { target, force } = parse(&["seagents", "stop", "it-b", "--force"]) else {
            panic!("not stop");
        };
        assert_eq!((target.as_str(), force), ("it-b", true));
    }

    #[test]
    fn read_takes_a_target_or_a_transcript_never_both() {
        let Command::Read { target, transcript, json, from, tail, raw } =
            parse(&["seagents", "read", "@it-a", "--json", "--from", "0::/t.jsonl", "--tail=5", "--raw"])
        else {
            panic!("not read");
        };
        assert_eq!((target.as_deref(), transcript, json, raw), (Some("@it-a"), None, true, true));
        assert_eq!((from.as_deref(), tail), (Some("0::/t.jsonl"), Some(5)));
        // A text read's last line, pasted whole.
        let Command::Read { from, .. } = parse(&["seagents", "read", "x", "--from", "-- cursor: 0::/t.jsonl"]) else {
            panic!("not read");
        };
        assert_eq!(from.as_deref(), Some("-- cursor: 0::/t.jsonl"));
        let Command::Read { target, transcript, .. } = parse(&["seagents", "read", "--transcript", "/t.jsonl"]) else {
            panic!("not read");
        };
        assert_eq!((target, transcript), (None, Some(PathBuf::from("/t.jsonl"))));
        for bad in [
            &["seagents", "read"][..],
            &["seagents", "read", "x", "--transcript", "/t.jsonl"][..],
            &["seagents", "read", "x", "--tail", "many"][..],
        ] {
            assert_eq!(Cli::try_parse_from(bad).unwrap_err().exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn focus_takes_a_tag_or_a_pid() {
        for t in ["reviewer", "@reviewer", "1234"] {
            let Command::Focus { target } = parse(&["seagents", "focus", t]) else {
                panic!("not focus");
            };
            assert_eq!(target, t);
        }
    }

    #[test]
    fn run_and_lead_need_a_command() {
        for args in [
            &["seagents", "run"][..],
            &["seagents", "run", "--tag", "h"][..],
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
    fn hook_takes_anything_and_never_fails_to_parse() {
        for args in [
            &["seagents", "__hook"][..],
            &["seagents", "__hook", "--help", "--version", "-x", "stray"][..],
        ] {
            assert!(matches!(parse(args), Command::Hook { .. }), "{args:?}");
        }
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
            &["seagents", "snapshot", "--bogus"][..],
        ] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
    }
}
