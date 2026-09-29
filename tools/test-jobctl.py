#!/usr/bin/env python3
"""Job-control regression harness for `seagents run`.

The wrapper -> __lead anchor -> agent suspend chain is the subtlest code in the
repo and its regression mode is silent (Ctrl+Z "does nothing"). This exercises
the full round trip headlessly by driving a real interactive bash inside a pty:

  1. run an inner command that suspends itself (what Claude does on Ctrl+Z)
  2. assert the *shell* reports the job Stopped (stop propagated both hops up)
  3. `fg` and assert the inner command resumed and the wrapper exited 0

Usage: python3 tools/test-jobctl.py [path-to-seagents]
       (default: target/release/seagents, falling back to `seagents` on PATH)

Exits 0 on success, 1 with a transcript dump on failure. Needs a Linux box with
bash; safe to run anywhere — it only touches its own pty.
"""
import os
import pty
import re
import shutil
import sys
import time

def resolve_binary() -> str:
    if len(sys.argv) > 1:
        return sys.argv[1]
    repo_default = os.path.join(os.path.dirname(__file__), "..", "target", "release", "seagents")
    if os.access(repo_default, os.X_OK):
        return os.path.abspath(repo_default)
    found = shutil.which("seagents")
    if found:
        return found
    sys.exit("no seagents binary: build with `cargo build --release` or pass a path")

def main() -> int:
    binary = resolve_binary()
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["PS1"] = "SHELL$ "
        os.execvp("bash", ["bash", "--norc", "-i"])
    os.set_blocking(fd, False)

    out = b""

    def read_until(pattern: str, timeout: float) -> bool:
        """Read until `pattern` matches the transcript so far, or `timeout`.
        Waiting on the event instead of a fixed window keeps a slow box from
        failing and a fast one from checking too early."""
        nonlocal out
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pattern, out.decode(errors="replace")):
                return True
            try:
                data = os.read(fd, 4096)
            except BlockingIOError:
                time.sleep(0.02)
                continue
            except OSError:
                break
            if not data:
                break
            out += data
        return bool(re.search(pattern, out.decode(errors="replace")))

    def send(s: str) -> None:
        os.write(fd, s.encode())

    def fail(msg: str) -> int:
        print(f"FAIL: {msg}", file=sys.stderr)
        print("--- transcript ---", file=sys.stderr)
        print(out.decode(errors="replace"), file=sys.stderr)
        return 1

    # Markers are quote-split on the command line so the terminal's echo of the
    # command itself can never satisfy the assertions.
    if not read_until(r"SHELL\$ ", 5.0):
        return fail("bash never showed its prompt")
    send(f"{binary} run sh -c 'echo ST''ART; kill -TSTP $$; echo RES''UMED'\n")
    # bash reports the job only once the wrapper has stopped itself, and the
    # wrapper stops only after relaying what the agent printed before its
    # stop — so START must already be here when Stopped is. (It used to race:
    # the wrapper froze its own output pump with START still on the pty.)
    if not read_until(r"Stopped", 5.0):
        if "START" not in out.decode(errors="replace"):
            return fail("inner command never ran")
        return fail("shell never reported the job Stopped — suspend did not propagate")
    text = out.decode(errors="replace")
    if "START" not in text:
        return fail("inner command never ran — or its output was not relayed before the stop")
    if "RESUMED" in text:
        return fail("inner command resumed before fg — stop was not delivered")
    print("OK: suspend propagated to the shell")

    send("fg\n")
    if not read_until(r"RESUMED", 5.0):
        return fail("fg did not resume the inner command")
    print("OK: fg resumed the inner command")

    # Until the wrapper has exited, its input pump owns the keyboard: anything
    # typed now goes to the finished agent's pty, not to bash. Wait for bash.
    if not read_until(r"RESUMED[\s\S]*SHELL\$ ", 5.0):
        return fail("the wrapper never exited back to the shell")

    send("echo EXIT_$?\n")
    read_until(r"EXIT_\d+", 5.0)
    m = re.search(r"EXIT_(\d+)", out.decode(errors="replace"))
    if not m or m.group(1) != "0":
        return fail(f"wrapper exit code was {m.group(1) if m else 'missing'}, expected 0")
    print("OK: wrapper exited 0 — full suspend/resume round trip works")

    send("exit\n")
    os.waitpid(pid, 0)
    return 0

if __name__ == "__main__":
    sys.exit(main())
