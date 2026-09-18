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

The session fills the panel, the input box sits at the bottom. Like a new
terminal, the agent works in the directory of the panel that had focus when
you opened it (a file manager's directory, an editor's file), or in the project
root. The panel title is your first request, so several agent panels stay
apart at a glance; before you ask anything it shows the working directory
instead. Give a session a name
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
| `/name args` + `Enter` | Send the prompt template `name` with `args` filled in |
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

A fifth tool, **skill**, appears when skills are defined; see
[Skills](#skills).

A file the agent edits while it is open in an editor is reloaded there at
once, cursor and scroll position kept, unless that editor has unsaved changes;
then the editor keeps them and marks the conflict, as with any change on disk
(see [Changes on disk](editor.md#changes-on-disk)).

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

## The agent directory

The agent's own files live in an `ai` directory that exists at three
levels, highest priority first:

1. `.termide/ai/` in the directory the panel works in;
2. `.termide/ai/` in the TermIDE project root, when that is another
   directory;
3. `ai/` in the TermIDE configuration directory
   (`~/.config/termide/ai/` on Linux,
   `~/Library/Application Support/termide/ai/` on macOS).

A single file is taken from the first level that has it. A directory of named
entries (agents, later skills and prompts) is the union of all levels, and a
name defined higher hides the same name below.

```
ai/
  AGENTS.md                the system prompt template of the default agent
  agents/<name>/SOUL.md    the template of a custom agent (optional)
  agents/<name>/agent.toml what else sets the agent apart (optional)
  skills/<name>/SKILL.md   skills, see below
  prompts/<name>.md        prompt templates, typed as /name
```

The first time the panel opens, the configuration level is laid out:
`AGENTS.md` receives the shipped template, `agents/`, `skills/` and
`prompts/` are created empty. Nothing there is ever overwritten; delete
`AGENTS.md` to get the shipped template back.

### Agents

An agent is a directory under `agents/`. `default` is the one the panel
starts as; it has no directory and speaks with `ai/AGENTS.md`. Any directory
defines an agent you can switch to from the **Agent** status chip or **Change
agent…** in the `[≡]` menu; the picker shows each agent's description. Its
`SOUL.md` is the agent's own template; without one it uses `ai/AGENTS.md`
too. Beside it an `agent.toml` may set, every field optional:

```toml
description = "Reviews diffs and points at risks"
model = "Qwen3.8-27B-MTPLX-Optimized-Quality"   # at the configured endpoint
mode = "accept-edits"                            # ask | accept-edits | auto
tools = ["read", "bash"]                         # a subset of the built-in tools
```

Switching agents mid-session swaps the prompt and the tools for the next
request; the model and the mode change only when the definition names them,
and the session log records the switch, as it does for the **Model** chip.
A reopened session comes back as the agent it last ran as, and a saved layout
remembers it too.

### The system prompt

The prompt the model receives is assembled from files: the template
`ai/AGENTS.md` with four placeholders the agent fills in. No prompt text is
built into TermIDE; the template below ships as a data file
(`crates/agent-core/assets/AGENTS.md`) and is written to the
configuration level on first use, and from then on the file is what counts. A
project or the panel's directory may carry its own `.termide/ai/AGENTS.md`,
which then replaces it:

```markdown
You are a coding agent working inside termide, a terminal IDE. You help with software tasks in the current project: you read code, make targeted edits, run commands and report what you did and what you found.

# Tools
{{tools}}

# Guidelines
- Read a file before you change it, and keep edits small and targeted.
- Name file paths clearly when you talk about files.
- Be concise.
{{guidelines}}

# Environment
{{environment}}

{{project_instructions}}
```

`{{tools}}` is the tool list with a line per tool, `{{guidelines}}` the rules
the tools themselves contribute, `{{environment}}` the working directory,
platform, date and whether it is a git repository, and
`{{project_instructions}}` the instruction files described next. Reword the
file, drop a section or add your own; a placeholder you leave out is simply
not sent. **Show system prompt** in the
panel's `[≡]` menu opens the assembled result, so you can see exactly what
the model gets.

### Skills

A skill is a directory with a `SKILL.md` in the [agentskills.io](https://agentskills.io)
shape: YAML front matter with `name` and `description`, then the
instructions, plus any files the instructions refer to (scripts, checklists,
examples):

```markdown
---
name: release
description: Cut a release: version bump, changelog, tag, packages
---
# Release

1. Run `scripts/check.sh` …
```

Skills are read from `skills/` at the three levels of the `ai` directory and
also from `.agents/skills/` in the panel's directory and in the project root,
the directory other agents share, so a skill written for Claude Code, Codex
or pi works unchanged. The same name at a higher level hides the lower one.

Only the names and descriptions go into the prompt, one line per skill under
`{{skills}}`; the instructions themselves enter the conversation when the
model loads the skill with the `skill` tool, which returns the text of
`SKILL.md` and lists the files beside it for the model to `read`. Loading a
skill never asks for permission. The tool exists only when at least one skill
does, and it is not subject to an agent's `tools` list.

### Prompt templates

A prompt template is a Markdown file `prompts/<name>.md`, at any of the three
levels, that you send as `/name` followed by arguments. The front matter is
optional: `description` for the picker and `argument-hint` for what to type
after the name. In the body `$ARGUMENTS` stands for everything after the
name and `$1`…`$9` for its words; a body without placeholders gets the
arguments appended on a line of their own.

```markdown
---
description: Review a file for bugs and risks
argument-hint: <path>
---
Review $1. Point at bugs first, style last, and quote the lines you mean.
```

`/review src/parser.rs` then sends the expanded text, which appears in the
session as what the model actually received. **Insert prompt…** in the `[≡]`
menu lists the templates and puts the chosen `/name ` into the input. A
message starting with `/` that names no template is not sent; a path such as
`/usr/bin/ls` is plain text.

### Project instructions

The agent reads `AGENTS.md` (or `CLAUDE.md` in the same directory) from every
directory between the filesystem root and the panel's working directory,
most specific last, so the panel directory's file outranks the project's. The
project root's file is included even when the panel works outside it. Put
your conventions there and the agent follows them; global rules go into the
template `ai/AGENTS.md` itself. Files over 32 KiB are skipped.

## Session history

Every session is written to a log in JSON Lines, one file per session, under
`ai/sessions/<path of the panel's directory>/` in the TermIDE configuration
directory, beside the agents. The log records the model and the agent the
session started with and every switch, so a reopened session continues on
the model and as the agent it last used. When a session
approaches the model's context window, the agent replaces the older part with
a summary it writes itself and keeps the recent messages verbatim; the panel
says when this happens.

When TermIDE reopens a saved layout, the agent panel comes back with it and
continues the session it was in, on that session's model. If the log has been
deleted the panel starts a fresh session; if no model is configured any more
the panel is left out of the layout.
