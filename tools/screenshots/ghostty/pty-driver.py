#!/usr/bin/env python3
"""Play a key script into a program on a pty, passing its output through.

Runs inside the recording container under a real terminal (Ghostty on the
host), so image protocols reach that terminal untouched. At each `shot`
line it writes out/.ghostty/<name>.ready and waits for <name>.done, which the
host writes after capturing the window.

Script lines: `sleep <s>`, `key <name>`, `type <text>`, `shot <name>`.
Usage: pty-driver.py <script> -- <command> [args...]
"""

import fcntl, os, pty, select, signal, struct, sys, termios, time, tty

KEYS = {
    "enter": b"\r", "esc": b"\x1b", "up": b"\x1b[A", "down": b"\x1b[B",
    "right": b"\x1b[C", "left": b"\x1b[D", "backspace": b"\x7f", "tab": b"\t",
}
SIGNALS = "/opt/shots/out/.ghostty"


def keybytes(name):
    if name in KEYS:
        return KEYS[name]
    if name.startswith("alt+"):
        return b"\x1b" + name[4:].encode()
    if name.startswith("ctrl+"):
        return bytes([ord(name[5:].lower()) & 0x1F])
    return name.encode()


def main():
    split = sys.argv.index("--")
    steps = [line.split(" ", 1) for line in open(sys.argv[1]).read().splitlines()
             if line.strip() and not line.startswith("#")]
    command = sys.argv[split + 1:]
    os.makedirs(SIGNALS, exist_ok=True)

    pid, master = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)

    stdin_tty = os.isatty(0)

    def resize(*_):
        if stdin_tty:
            size = fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8)
            fcntl.ioctl(master, termios.TIOCSWINSZ, size)

    resize()
    signal.signal(signal.SIGWINCH, resize)
    saved = termios.tcgetattr(0) if stdin_tty else None
    if stdin_tty:
        tty.setraw(0)

    def pump(seconds):
        # Pass bytes both ways for `seconds`: the program's output to the
        # terminal and the terminal's replies (e.g. capability queries) back.
        end = time.monotonic() + seconds
        while True:
            left = end - time.monotonic()
            if left <= 0:
                return True
            fds = [master] + ([0] if stdin_tty else [])
            ready, _, _ = select.select(fds, [], [], left)
            if master in ready:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    return False
                if not data:
                    return False
                os.write(1, data)
            if 0 in ready:
                os.write(master, os.read(0, 1024))

    try:
        for step in steps:
            verb, arg = step[0], (step[1] if len(step) > 1 else "")
            if verb == "sleep":
                pump(float(arg))
            elif verb == "key":
                os.write(master, keybytes(arg))
                pump(0.25)
            elif verb == "type":
                for ch in arg:
                    os.write(master, ch.encode())
                    pump(0.03)
            elif verb == "shot":
                done = os.path.join(SIGNALS, arg + ".done")
                open(os.path.join(SIGNALS, arg + ".ready"), "w").close()
                deadline = time.monotonic() + 60
                while not os.path.exists(done) and time.monotonic() < deadline:
                    pump(0.2)
        os.write(master, keybytes("alt+q"))
        pump(1.5)
    finally:
        if saved:
            termios.tcsetattr(0, termios.TCSADRAIN, saved)
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass


if __name__ == "__main__":
    main()
