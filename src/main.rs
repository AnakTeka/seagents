//! seagents — observe coding agents running in your own terminal windows.
//!
//! Milestone 1: `seagents run <cmd...>` — a transparent pty-teeing launcher.
//! It spawns <cmd> on an inner PTY, relays stdin/output byte-for-byte between
//! your real terminal and the child, and tees a copy of the output stream to a
//! file so a future daemon can scrape it for agent status. Nothing here parses
//! or rewrites the stream: transparency comes from relaying, not interpreting.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

mod detect;
mod focus;
mod hud;
mod status;

fn main() {
    let mut args = std::env::args();
    let _bin = args.next();
    match args.next().as_deref() {
        Some("run") => {
            let cmd: Vec<String> = args.collect();
            if cmd.is_empty() {
                eprintln!("usage: seagents run <cmd> [args...]");
                std::process::exit(2);
            }
            std::process::exit(run(cmd));
        }
        Some("status") => std::process::exit(status::print()),
        Some("focus") => {
            let pid = args.next().and_then(|s| s.parse::<u64>().ok());
            match pid {
                Some(p) => std::process::exit(focus::focus(p)),
                None => {
                    eprintln!("usage: seagents focus <child_pid>");
                    std::process::exit(2);
                }
            }
        }
        // Dev tool: render a .raw file and show the detector's verdict, for
        // tuning detection against real captures.
        Some("debug-screen") => {
            let file = args.next().unwrap_or_default();
            let cols: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(80);
            let rows: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(24);
            let data = std::fs::read(&file).unwrap_or_default();
            let screen = detect::render(&data, rows, cols);
            println!("{screen}");
            println!("\n---\nis_blocked = {}", detect::is_blocked(&screen));
            std::process::exit(0);
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
    eprintln!("  seagents status                show every tracked agent's state");
    eprintln!("  seagents focus <child_pid>     focus that agent's i3 window");
}

/// Query the *outer* terminal's real size, including pixel dimensions, so the
/// inner PTY matches it exactly (apps like image/graphics protocols care).
fn terminal_size() -> PtySize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        // Best-effort: on failure we fall back to a sane 80x24.
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col != 0 {
            PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            }
        } else {
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }
        }
    }
}

/// Directory holding this machine's seagents session files.
fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/seagents"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/seagents"));
    let _ = fs::create_dir_all(&base);
    base
}

/// A session is keyed by the wrapper's pid; `.raw` holds the teed output and
/// `.meta` holds the identity the daemon needs (cwd, argv, child pid).
fn session_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let base = cache_dir();
    let id = std::process::id();
    (
        base.join(format!("{id}.raw")),
        base.join(format!("{id}.meta")),
    )
}

/// Record who this agent is so the daemon can label it and locate its process /
/// i3 window. cwd here is the launch cwd; the daemon can also read the live cwd
/// from /proc/<child_pid>/cwd.
fn write_meta(
    path: &std::path::Path,
    child_pid: Option<u32>,
    cwd: &std::path::Path,
    argv: &[String],
    size: &PtySize,
    con_id: Option<u64>,
) {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = serde_json::json!({
        "wrapper_pid": std::process::id(),
        "child_pid": child_pid,
        "cwd": cwd.to_string_lossy(),
        "argv": argv,
        "started": started,
        // Geometry so the daemon can re-render the raw stream at the right size.
        "cols": size.cols,
        "rows": size.rows,
        // The i3 window this agent runs in, captured at launch — the reliable
        // handle for focus (titles can collide between idle agents).
        "con_id": con_id,
    });
    if let Ok(s) = serde_json::to_string_pretty(&meta) {
        let _ = fs::write(path, s);
    }
}

fn run(cmd: Vec<String>) -> i32 {
    // Capture the launching terminal's i3 window up front, while it's still the
    // focused window (the user just typed the command into it).
    let con_id = focus::focused_terminal_con_id();

    let size = terminal_size();

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(size) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("seagents: openpty failed: {e}");
            return 1;
        }
    };

    // Build the child command, inheriting cwd and environment.
    let mut builder = CommandBuilder::new(&cmd[0]);
    for arg in &cmd[1..] {
        builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }

    let mut child = match pair.slave.spawn_command(builder) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("seagents: failed to spawn {}: {e}", cmd[0]);
            return 1;
        }
    };
    // Drop the slave so the master sees EOF once the child exits.
    drop(pair.slave);

    // Split the master into a reader (child output) and a writer (child input)
    // before sharing it for resize handling.
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("seagents: clone reader failed: {e}");
            return 1;
        }
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("seagents: take writer failed: {e}");
            return 1;
        }
    };
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    // Open the tee target and write the identity sidecar. Both best-effort.
    let (raw_path, meta_path) = session_paths();
    let tee = File::create(&raw_path).ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    write_meta(&meta_path, child.process_id(), &cwd, &cmd, &size, con_id);
    if std::env::var("SEAGENTS_DEBUG").is_ok() {
        eprintln!("seagents: teeing to {}", raw_path.display());
    }

    // Put the outer terminal into raw mode so keystrokes and control bytes flow
    // through untouched (the inner PTY's line discipline does the cooking).
    let raw = crossterm::terminal::enable_raw_mode().is_ok();

    // Output pump: master -> stdout (+ tee). This owns the "T-junction".
    let reader_handle = thread::spawn(move || {
        let mut out = std::io::stdout();
        let mut tee = tee;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = &buf[..n];
                    if out.write_all(data).is_err() {
                        break;
                    }
                    let _ = out.flush();
                    if let Some(f) = tee.as_mut() {
                        let _ = f.write_all(data);
                        let _ = f.flush();
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // Input pump: stdin -> master. Detached; the process exit reaps it.
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // Resize pump: on SIGWINCH, re-read the outer size and push it to the inner
    // PTY so the child's TUI reflows.
    {
        let master = Arc::clone(&master);
        if let Ok(mut signals) =
            signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
        {
            thread::spawn(move || {
                for _ in &mut signals {
                    let size = terminal_size();
                    if let Ok(m) = master.lock() {
                        let _ = m.resize(size);
                    }
                }
            });
        }
    }

    // Block until the child exits, then restore the terminal.
    let code = match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(_) => 1,
    };

    // Let the output pump drain anything buffered before we tear down.
    let _ = reader_handle.join();
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    code
}
