# Coding Agent

TermIDE has a built-in coding agent: a panel where you describe a task in
plain language and a language model carries it out by reading files, editing
them and running shell commands in your project. Every action it takes that
could change something asks for your permission first.

Open it with `Alt+A`, from **Windows → Agent**, or from the command palette
(**Open Agent**).

## Configuring a model

The agent talks to any OpenAI-compatible endpoint. That covers local servers
(llama.cpp, Ollama, vLLM, omlx) and most hosted gateways. Without a configured
model the panel refuses to open and says so.

```toml
[agent]
base_url = "http://127.0.0.1:10000/v1"
model = "Qwen3.8-Flash-Next-oQ4e-mtp"
context_window = 32000
max_tokens = 4096
reasoning = false            # send reasoning_effort to models that support it
api_key_env = "OPENAI_API_KEY"   # name of the variable, never the key itself
```

The API key is read from the environment variable named by `api_key_env`, so
the configuration file never holds a secret. Local servers usually need no key
at all; leave the variable unset.

## Using the panel

The session fills the panel, the input box sits at the bottom. The panel title
is your first request, so several agent panels stay apart at a glance; before
you ask anything it shows the working directory instead. Give a session a name
of your own through the panel's `[≡]` menu → **Rename session**, and the title
shows that name from then on.

The same menu has **New session**, which starts an empty one, and **Open
session**, which lists this project's sessions newest first (the current one
marked `●`) so you can pick up where you left off. Switching waits for the
current task: stop it with `Esc` first if the agent is still working.

| Key | Action |
|---|---|
| `Enter` | Send. While the agent works, the text is queued for the next turn instead |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | New line in the input |
| `Esc` | Stop the running task; with nothing running, clear the input |
| `Ctrl+O` | Expand or collapse every tool call |
| `Shift+Tab` | Cycle the permission mode: ask → accept-edits → auto |
| `Ctrl+↑` / `Ctrl+↓`, `PageUp` / `PageDown` | Scroll the session |
| `Ctrl+Home` / `Ctrl+End` | Jump to the start, or back to following the newest output |

Each tool call is one line: the tool, what it acted on, and whether it
succeeded. Click it or press `Ctrl+O` to see the full output. The panel
follows the newest output until you scroll up, and resumes following when you
scroll back to the bottom.

The status chips show the permission mode, the model, how much of the context
window is used, whether the agent is working and how many messages are queued.
The first two are buttons, and the same two entries sit in the `[≡]` menu.

**Model** asks the endpoint for the models it serves and lists them, the
current one marked `●`; the last entry lets you type an id instead, which is
also what you get when the endpoint cannot list its models. The switch takes
effect on your next request and stays with the session: it is written to the
session log, so reopening that session brings its model back, and a new
session starts on whatever model the panel is on. When the endpoint reports
a model's context window (vLLM and omlx do), the panel adopts it; otherwise
the window and the token limit stay as configured. Switching waits for the
current task, like switching sessions.

**Mode** offers the three permission modes described below. `Shift+Tab` cycles
through them without the picker. A change applies at the agent's next tool
call, so you can loosen the mode while a long task is running instead of
answering the same prompt again and again. Neither switch touches the
configuration file; the panel starts from `[agent]` again the next time.

## Tools

The agent has four tools.

- **read** returns a file with line numbers, paged with an offset when a file
  is long.
- **edit** replaces a unique piece of text in a file and reports a diff of
  what changed.
- **write** creates a file or replaces its whole content.
- **bash** runs a shell command in the project directory, streaming its output.
  Long output keeps its beginning and end, and the complete log is written to
  a file the agent can read.

Searching is done through `bash` with the tools you already have (`rg`,
`find`), rather than through a separate search tool.

## Permissions

Nothing that changes your project happens without your say-so. When the agent
wants to do something that is not already allowed, a dialog offers four
answers: allow once, allow for this session, allow always, or deny. "Allow
always" appends a rule to `.termide/config.toml` in the project.

Rules live per tool. Among the rules that match, the strictest wins, so a
`deny` always beats an `allow`:

```toml
[agent.permissions]
mode = "ask"        # ask | accept-edits | auto

[agent.permissions.bash]
"cargo *"     = "allow"
"git status*" = "allow"
"git push*"   = "ask"
"rm -rf *"    = "deny"

[agent.permissions.edit]
"src/**" = "allow"
".env"   = "deny"

[agent.permissions.read]
"**/.env*" = "deny"
```

In a pattern, `*` stands for any text and a leading `**/` is optional, so
`**/.env*` also matches `.env` in the project root. Shell commands are matched
per part: `cargo build && rm -rf target` needs both halves allowed, and a deny
on either half stops the whole command. Command substitution (`$(…)`, backticks)
is never allowed automatically.

The mode decides what happens to anything no rule covers. `mode` in the
configuration is the starting point; the panel's **Mode** chip and `Shift+Tab`
change it for the current panel.

- **ask** (the default) asks before every change and every command.
- **accept-edits** also lets the agent edit and create files inside the project
  without asking; shell commands still ask.
- **auto** allows everything. Use it only where a mistake costs nothing, such
  as a container or a scratch checkout.

Two things never ask in any mode: reading a file inside the project, and a
short list of commands that only look at things (`ls`, `cat`, `rg`,
`git status`, `find` without `-delete` or `-exec`, and similar). A redirection
in the command disqualifies it.

## Project instructions

The agent reads `AGENTS.md` (or `CLAUDE.md` in the same directory) from every
directory between the filesystem root and your working directory, most
specific last, plus a global `AGENTS.md` in the TermIDE configuration
directory. Put your project's conventions there and the agent follows them.
Files over 32 KiB are skipped.

## Session history

Every session is written to a log in JSON Lines, one file per session, in an
`agent` folder beside the project's saved layout under the TermIDE data
directory. The log records the model the session started on and every switch,
so a reopened session continues on the model it last used. When a session
approaches the model's context window, the agent replaces the older part with
a summary it writes itself and keeps the recent messages verbatim; the panel
says when this happens.

When TermIDE reopens a saved layout, the agent panel comes back with it and
continues the session it was in, on that session's model. If the log has been
deleted the panel starts a fresh session; if no model is configured any more
the panel is left out of the layout.
