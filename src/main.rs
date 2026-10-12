//! seagents — your coding agents in herdr, each shown in its own i3 window.
//!
//! This file is the command line and nothing else: the clap definition and a
//! dispatch to the module that owns each command.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod adapter;
mod clock;
mod fsutil;
mod herdr;
mod hook;
mod hud;
mod i3;
mod launch;
mod procfs;
mod read;
mod registry;
mod run;
mod screen;
mod send;
mod snapshot;
mod spawn;
mod status;
mod transcript;
mod turn;
mod wait;
mod window;

/// Run coding agents in herdr, each in its own i3 window: see which is
/// blocked, done or working, jump to it, and restore the layout after a
/// reboot.
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

    /// Start an agent in herdr and show it in this window (how tracking happens)
    ///
    /// The agent runs in a herdr server (started if none is), in a tab of
    /// the workspace for this directory; this window shows it alone, through
    /// `herdr terminal attach`. Closing the window stops the agent; the agent
    /// exiting ends `run`. Ctrl+B is herdr's attach prefix (Ctrl+B Ctrl+B
    /// sends one); a detach (Ctrl+B q) is undone at once. From another
    /// machine, `herdr --remote ssh <this one>` shows the same agents.
    /// Everything after the command's first word is passed to it untouched,
    /// flags included: `seagents run claude --model opus`. The command must
    /// be an agent herdr can start (claude, codex, gemini, opencode, …; exit
    /// 2 otherwise). Inside a herdr pane, `run` just runs the command.
    Run {
        /// The agent's name ([a-z][a-z0-9_-]{0,31}), also its herdr agent
        /// name; minted as `<cwd basename>-<4 hex>` when omitted. Refused
        /// (exit 2) while a live agent holds it. A terminal seagents opened
        /// carries the handle `seagents-<TAG>`, which is how `run` finds its
        /// own window.
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,

        /// A prompt to start the agent on (as `spawn` writes it): read,
        /// deleted, and submitted once the agent is idle — by reference, as a
        /// prompt file in its working directory
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
    /// Prints `{"tag","pid","pane_id","cwd","stops"}` once the agent has
    /// started in herdr (not once it has started working: follow with
    /// `seagents wait <tag> --after 0`). The prompt is submitted once the
    /// agent is idle — after any dialog it opened on the way up, which is
    /// answered in its window. The window closes when the agent exits. Exit
    /// 1, with the reason on stderr, on any refusal.
    Spawn {
        /// Directory to run the agent in (default: this one)
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,

        /// The new agent's name (minted as `<dir>-<4 hex>` when omitted);
        /// refused while live or recorded in the saved layout
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,

        /// Which agent to launch
        #[arg(long, value_name = "AGENT", default_value = "claude")]
        agent: String,

        /// Permission mode for the agent: plan, default, acceptEdits or
        /// bypassPermissions (Codex gets the matching -a/-s pair); passed
        /// only if given
        #[arg(long, value_name = "MODE")]
        permission_mode: Option<String>,

        /// Run in a new git worktree, <repo>/.seagents/worktrees/<TAG> on
        /// branch seagents/<TAG> — without any of the caller's uncommitted
        /// changes, and with a cold build directory
        #[arg(long)]
        worktree: bool,

        /// One extra argument for the agent, repeatable:
        /// `--arg --model --arg haiku`
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
    /// (waiting on a human; `rule` names the dialog), `exit` 4 (also for an
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
    /// until the agent is idle (out of its turn, herdr reading it idle), so
    /// the next turn to end is the one it opened (`paired: true`). A blocked
    /// agent (a permission prompt, a trust dialog) is refused at once, with
    /// the dialog's `rule`. herdr submits it (`herdr agent prompt`: a paste
    /// the way the agent takes one, then Enter). Text an agent would take as
    /// quoted data rather than as your words (Claude: over 700 characters or
    /// 2 lines) is written to `<its cwd>/.seagents/prompts/` instead, and it
    /// is sent one line telling it to read that file. A paste lands after
    /// whatever is in the agent's input box, so that box must be empty or
    /// hold a prompt seagents sent that the agent put back (Esc'd before its
    /// first response), which is cleared first; anything else is someone's
    /// draft and is refused, --now too. Refusals print
    /// `{"ok":false,"reason","error"[,"rule"]}` and exit: 3 blocked, 2
    /// busy_timeout (still busy at --timeout; nothing was sent), 4 exit, 6
    /// draft (text someone typed is in the input box; nothing was sent), 1
    /// anything else.
    Send {
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid`
        target: String,

        /// Submit at once, even mid-turn (the agent may fold it into the
        /// running turn: `paired: false`). Never past a dialog
        #[arg(long)]
        now: bool,

        /// TEXT is key presses, by herdr's key names, space-separated:
        /// `--keys 'down enter'`, `--keys esc`, `--keys 1`, `--keys ctrl+u`
        #[arg(long)]
        keys: bool,

        /// Paste the text without pressing Enter
        #[arg(long)]
        no_submit: bool,

        /// Let --keys go into an agent blocked at a dialog — to answer it
        /// (`--keys 1` picks "Yes" on Claude's permission menu)
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

    /// Make an agent exit: its exit command, then closing its pane, then SIGKILL
    ///
    /// Types the agent's exit command (Claude: `/exit`) and gives it 5 s;
    /// then closes its herdr pane (which hangs it up), then SIGKILLs its
    /// process group, 5 s apart. A blocked agent skips the exit command
    /// (typed into a permission menu, its Enter would pick an answer), and
    /// so does one whose screen shows a dialog herdr does not read as one
    /// (Codex's folder-trust question over an update notice), and one whose
    /// input box holds a draft someone typed (the command would be submitted
    /// as part of it); a prompt seagents sent that the agent put back is
    /// cleared first. Its window closes with it. Prints `{"ok":true,"tag"}`
    /// once the agent is gone. Exit 1 on any failure — including an agent
    /// that had already exited (no live agent by that name).
    Stop {
        /// The agent: its tag (`reviewer` or `@reviewer`), or its `pid`
        target: String,

        /// Skip the exit command: close the pane at once
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

    /// The agent's lifecycle hook `run` injects (internal)
    ///
    /// Reads one hook event (JSON) on stdin and records it in the `.agent` of
    /// the session of the herdr pane it runs in. Always exits 0 and prints nothing: a nonzero `Stop`
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
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(dispatch(cli.command.unwrap_or(Command::Hud)));
}

fn dispatch(command: Command) -> i32 {
    match command {
        Command::Hud => hud::run(),
        Command::Run { tag, seed_file, cmd } => run::run(cmd, tag, seed_file),
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
    }
}
