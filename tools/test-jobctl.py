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

    def drain(timeout: float) -> None:
        nonlocal out
        end = time.time() + timeout
        while time.time() < end:
            try:
                data = os.read(fd, 4096)
                if data:
                    out += data
                else:
                    time.sleep(0.05)
            except BlockingIOError:
                time.sleep(0.05)
            except OSError:
                break

    def send(s: str) -> None:
        os.write(fd, s.encode())

    def fail(msg: str) -> int:
        print(f"FAIL: {msg}", file=sys.stderr)
        print("--- transcript ---", file=sys.stderr)
        print(out.decode(errors="replace"), file=sys.stderr)
        return 1

    # Markers are quote-split on the command line so the terminal's echo of the
    # command itself can never satisfy the assertions.
    drain(1.0)
    send(f"{binary} run sh -c 'echo ST''ART; kill -TSTP $$; echo RES''UMED'\n")
    drain(2.0)
    text = out.decode(errors="replace")
    if "START" not in text:
        return fail("inner command never ran")
    if "RESUMED" in text:
        return fail("inner command resumed before fg — stop was not delivered")
    if not re.search(r"Stopped", text):
        return fail("shell never reported the job Stopped — suspend did not propagate")
    print("OK: suspend propagated to the shell")

    send("fg\n")
    drain(2.0)
    if "RESUMED" not in out.decode(errors="replace"):
        return fail("fg did not resume the inner command")
    print("OK: fg resumed the inner command")

    send("echo EXIT_$?\n")
    drain(1.0)
    m = re.search(r"EXIT_(\d+)", out.decode(errors="replace"))
    if not m or m.group(1) != "0":
        return fail(f"wrapper exit code was {m.group(1) if m else 'missing'}, expected 0")
    print("OK: wrapper exited 0 — full suspend/resume round trip works")

    send("exit\n")
    drain(0.5)
    return 0

if __name__ == "__main__":
    sys.exit(main())
