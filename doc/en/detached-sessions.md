# Detached Sessions

A detached session keeps running after the terminal that started it is gone.
Close the SSH connection, come back hours later, attach again — the editors,
shells, LSP servers and long-running jobs are exactly where you left them.

This is the same guarantee `tmux` and `screen` give, without a second
multiplexer between you and TermIDE: there is no prefix key competing with
TermIDE's own bindings, and no second layer to configure for colours or mouse
reporting.

> Unix only (Linux, macOS, BSD). Windows has no `fork`/`setsid`, and its
> ConPTY model needs a different host, so these flags are not available there.

## Quick start

```bash
termide --detached            # start a session, print its id
termide --list-sessions       # see what is running
termide --attach              # attach to the most recent session
termide --attach my-project   # attach to a specific one
```

`Alt+J` detaches again, leaving everything running — as does **Options →
Detach session** in the menu. That entry is shown only in a detachable
session; in an ordinary one there is nothing to detach from, so it is left
out rather than shown and refused.

A session is named after the project directory it was started in, so starting
one in `~/src/my-project` gives you `my-project`. Start a second session in the
same directory and it becomes `my-project-2`.

## Making every session detachable

Remembering `--detached` at launch is the whole catch: a termide started
normally cannot be detached later. A running process is bound to its
terminal's PTY — file descriptors are open, children inherited them, the
controlling terminal is assigned — and nothing can move it into another one.
This is the same reason `tmux` cannot adopt a program that is already running.

If you work this way most of the time, turn it on permanently:

```toml
[general]
always_detachable = true
```

or tick **Always detachable (Unix)** in Settings (`Alt+P`) → General. Every
`termide` then starts in a host of its own, and `Alt+J` works everywhere.

Worth knowing before you enable it:

- **Closing a terminal stops meaning "stop termide".** The session survives,
  and so do its LSP servers, watchers and shells. That is the point over SSH,
  and a surprise locally — check `--list-sessions` occasionally.
- **`$EDITOR` launches are exempt.** With file arguments (`EDITOR=termide git
  commit`) the option is ignored: git waits for the editor to exit, and a
  detach would tell it the edit finished when it had not.
- The extra PTY costs a little throughput on heavy output, the same way tmux
  does.

## A typical remote workflow

```bash
ssh server
cd ~/src/my-project
termide --detached
termide --attach
# … work, start a build, run an agent in a terminal panel …
# press Alt+J, or just close the SSH connection
```

Later, from any machine:

```bash
ssh server
termide --attach my-project
```

Closing the SSH connection without detaching is safe. The session notices the
client is gone and carries on; the next `--attach` picks it up.

## What survives, and why

Everything. The session is not saved and restored — it never stops.

`termide --detached` starts a small host process that owns a PTY and runs an
ordinary TermIDE inside it. Your shells, LSP servers, watchers and background
jobs are children of that TermIDE, so they are untouched by a client coming and
going. Attaching connects a terminal to the host; detaching disconnects it.

This is a different thing from the session layout in
`~/.local/share/termide/sessions/`, which records which panels were open so a
*new* TermIDE can reopen them. That still works as before, and still applies
when you start a session for the first time.

## Reattaching from a different terminal

You can attach from a terminal that is nothing like the one you started in — a
different size, a different emulator, a different `TERM`. On attach, TermIDE
re-negotiates the alternate screen, mouse reporting, bracketed paste and
keyboard protocol against the terminal that is now looking at it, re-detects
colour support from the client's `TERM`, and repaints in full.

Resizing the terminal while attached works normally; the layout redistributes
the way it does in a local session.

## Commands

| Command | What it does |
|---------|--------------|
| `termide --detached` | Start a detached session and print its id |
| `termide --detached file.rs` | Same, opening files as usual |
| `termide --attach` | Attach to the most recent session |
| `termide --attach <ID>` | Attach to a named session |
| `termide --list-sessions` | List sessions: id, pid, uptime, state, project |

`--list-sessions` also cleans up after sessions whose host process is gone, so
a crash never leaves a phantom entry behind.

## Detaching

| Way | When to use it |
|-----|----------------|
| `Alt+J` | The normal way. Rebind it as `detach_session` in the `[general.keybindings]` section. |
| Close the terminal | Safe. The session notices and keeps running. |
| `Ctrl+Z` | Does **not** work, and cannot: termide reads keys in raw mode, so the key never reaches the tty line discipline to become a SIGTSTP. `Alt+J` is the binding that does what you meant. |
| `Ctrl+\` three times | Emergency only — if TermIDE itself has stopped responding. Handled by the client, so it works even when the app does not. |

Ending a session is the same as ending any TermIDE: quit it (`Alt+Q`) while
attached. That stops the host process too, and removes the session.

## Only one client at a time

A second `--attach` to the same session is refused while another client is
attached, rather than mirroring the screen to both. Detach the first client (or
close its terminal) and the next attach succeeds immediately.

## Where the session state lives

Sockets live in `$XDG_RUNTIME_DIR/termide/` on Linux and BSD, and in
`~/Library/Application Support/termide/run/` on macOS, which has no
`XDG_RUNTIME_DIR`. The directory is owner-only (`0700`), so no other account on
the machine can attach to your sessions.

Nothing there needs cleaning up by hand: a socket outlives its host only until
the next `--list-sessions` or `--detached`, which prunes it.
