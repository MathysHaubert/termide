# Coding Agent

TermIDE has a built-in coding agent: a panel where you describe a task in
plain language and a language model carries it out by reading files, editing
them and running shell commands in your project. Every action it takes that
could change something asks for your permission first.

Open it with `Alt+A`, from **Windows → Agent**, or from the command palette
(**Open Agent**).

## Configuring a model

The agent talks to any OpenAI-compatible endpoint, or to Anthropic's Messages
API. The OpenAI protocol covers local servers (llama.cpp, Ollama, vLLM, omlx)
and most hosted gateways, OpenAI and OpenRouter among them. Without a
configured model the panel refuses to open and says so.

```toml
[agent]
provider = "openai"          # "openai" (the default) or "anthropic"
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

For a hosted OpenAI-compatible endpoint, keep `provider = "openai"` and point
`base_url` and `api_key_env` at it, for example OpenAI itself
(`https://api.openai.com/v1`, `OPENAI_API_KEY`) or OpenRouter
(`https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY`). For an Anthropic
subscription set `provider = "anthropic"`, drop `base_url` (the API root is
built in; set it only for a gateway) and point `api_key_env` at your
`ANTHROPIC_API_KEY`; `reasoning = true` then turns on extended thinking. The
**Model** chip lists the endpoint's models for each.

## Using the panel

The session fills the panel, the input box sits at the bottom. Like a new
terminal, the agent works in the directory of the panel that had focus when
you opened it (a file manager's directory, an editor's file), or in the project
root. The panel title is your first request, so several agent panels stay
apart at a glance; before you ask anything it shows the working directory
instead. Give a session a name of your own through the panel's `[≡]` menu →
**Rename session**, and the title shows that name from then on.

The same menu has **New session**, which starts an empty one, and **Open
session**, which lists the sessions of the directory the panel works in,
newest first (the current one marked `●`), so you can pick up where you left
off. Switching waits for the
current task: stop it with `Esc` first if the agent is still working.

| Key | Action |
|---|---|
| `Enter` | Send. While the agent works, the text is queued for the next turn instead |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | New line in the input |
| `Esc` | Stop the running task; with nothing running, clear the input |
| `Ctrl+O` | Expand or collapse every tool call |
| `Shift+Tab` | Cycle the permission mode: ask → accept-edits → auto → plan |
| `/name args` + `Enter` | Send the prompt template `name` with `args` filled in, or run the command script `name`; `/compact [focus]` summarises the session, `/undo` takes the last request back |
| `↑` / `↓` | On the first or last line of the input: recall an earlier request of this session, or come back to what you were typing |
| `Tab` | Complete the highlighted `/command` while the list is open |
| `Ctrl+↑` / `Ctrl+↓`, `PageUp` / `PageDown` | Scroll the session |
| `Ctrl+Home` / `Ctrl+End` | Jump to the start, or back to following the newest output |

Each tool call is one line: the tool, what it acted on, and whether it
succeeded. Click it or press `Ctrl+O` to see the full output. The panel
follows the newest output until you scroll up, and resumes following when you
scroll back to the bottom.

The status chips show the permission mode, the model with the endpoint it is
served from (`Qwen3.8-Flash… @ 127.0.0.1:10000`), the agent, the context
window with how much of it the last answer used (`Context: 12% of 32k`),
whether the agent is working and how many messages are queued. Mode, model
and agent are buttons, and the same entries sit in the `[≡]` menu.

Typing `/` opens a list of the matching prompt templates above the input;
`↑`/`↓` move in it, `Tab` or `Enter` complete the highlighted one, and `Enter`
on a name typed in full sends it.

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

### Undoing a request

`/undo`, or **Undo last request** in the `[≡]` menu, takes back the last
request that changed files: a card in the panel names the files, and on
confirmation each is put back as it was before that request (a file the
request created is removed) and the conversation is rewound to just before
it, so the agent no longer remembers doing it either. Open editors reload the
restored files. Repeat it to step back through earlier requests; a request
that changed nothing is skipped. It is a step back, not a redo: the undone
messages stay in the session log on a dead branch, and the files' newer
content is gone.

Before `edit` or `write` runs, the panel keeps a copy of the target under the
session's directory (`ai/sessions/<path>/checkpoints/<session id>/`), one
folder per request, deleted again when the request is undone. Shell commands
are not covered: what `bash` changes, git or your own backups have to hold.

## Tools

The agent has four built-in tools, plus those its MCP servers provide (see
[MCP servers](#mcp-servers)).

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
wants to do something that is not already allowed, a card appears in the
panel above the input with six rows: allow once, allow for this session,
allow always, deny, **deny and tell the agent why** (a sentence you type,
returned to the model as the reason, so it can take another way), and
**stop the run**. `↑`/`↓` and `Enter`, the digits `1`–`6`, or a click answer
it; `Esc` stops the run. The status line announces the question too, so a
panel that is not in focus does not ask unseen. "Allow always" appends a rule
to `.termide/config.toml` in the project.

Rules live per tool. Among the rules that match, the strictest wins, so a
`deny` always beats an `allow`:

```toml
[agent.permissions]
mode = "ask"        # ask | accept-edits | auto | plan

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
- **plan** allows nothing that changes anything: the agent reads, searches
  and runs look-only commands, then answers with a plan. See below.

Two things never ask in any mode: reading a file inside the project, and a
short list of commands that only look at things (`ls`, `cat`, `rg`,
`git status`, `find` without `-delete` or `-exec`, and similar). A redirection
in the command disqualifies it.

### Plan mode

For a task you want to see thought through before a line changes, switch to
**plan** (the chip, `Shift+Tab`, or an agent whose `agent.toml` says
`mode = "plan"`). While it is on, the instructions from `system/plan.md` are
added to the system prompt, and every tool call that could change something
is refused with a message the model reads, whatever the rules, the session
grants or a hook's approval say: `edit`, `write`, MCP tools, and any shell
command that is not on the look-only list. Reading and `skill` stay as in
`ask`.

When the agent answers, a card asks what to do with the plan:

```
┌ Plan mode: carry the plan out? ────┐
│ 1. Yes, accepting edits            │
│ 2. Yes, asking before each change  │
│ 3. Keep planning                   │
└────────────────────────────────────┘
```

The first two leave plan mode for accept-edits or ask and send the request
named in the front matter of `system/plan.md` (`request:`), so the same
session goes on to carry the plan out with it in context; the third (or
`Esc`) keeps plan mode, and whatever you type next refines the plan. The
plan is the agent's answer in the session, nothing is written to a file; the
`/undo` checkpoints cover the changes that follow.

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
entries (agents, skills and prompt templates) is the union of all levels, and a
name defined higher hides the same name below.

```
ai/
  AGENTS.md                the system prompt template of the default agent
  agents/<name>/SOUL.md    the template of a custom agent (optional)
  agents/<name>/agent.toml what else sets the agent apart (optional)
  skills/<name>/SKILL.md   skills, see below
  prompts/<name>.md        prompt templates, typed as /name
  commands/<name>          command scripts, typed as /name, see below
  mcp.toml                 MCP servers, see below
  hooks.toml               command hooks, see below
  system/compact.md        how the agent summarises a long session
  system/compacted.md      how the summary is worded in the context
  system/plan.md           what plan mode tells the agent, and what accepting a plan sends
```

The first time the panel opens, the configuration level is laid out:
`AGENTS.md` and the two `system/` files receive the shipped texts, `agents/`,
`skills/`, `prompts/` and `commands/` are created empty. Nothing there is ever overwritten; delete
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
mode = "accept-edits"                            # ask | accept-edits | auto | plan
tools = ["read", "bash"]                         # a subset of the built-in tools
```

Switching agents mid-session swaps the prompt and the tools for the next
request; the model and the mode change only when the definition names them,
and the session log records the switch, as it does for the **Model** chip.
A reopened session comes back as the agent it last ran as, and a saved layout
remembers it too.

### Subagents

When there is more than one agent, each built-in-loop agent gets a `task`
tool that hands a self-contained job to another agent. The delegate runs its
own loop to the end — its own prompt, its own tools, its own model — and its
final answer comes back as the tool's result; the steps and the files it read
along the way stay out of the main conversation. It is how a terse reviewer
or a focused searcher does its work without filling the session, the way
Claude Code's `Task` tool and OpenCode's sub-sessions do.

The delegate does not see the conversation, so the calling agent must put
everything into the prompt. It runs with no one to prompt, so it can only do
what the permission rules and the current mode already allow: anything that
would otherwise ask is refused with a reason it reads. External (`[acp]`)
agents cannot be delegates, and a subagent gets no `task` tool of its own, so
delegation does not nest. A run that will not stop is cut off after fifty
model calls.

### External agents

An agent may be another program altogether: put an `[acp]` table in its
`agent.toml` and the panel drives it over the
[Agent Client Protocol](https://agentclientprotocol.com) instead of running
the built-in loop. Claude Code, Codex and Gemini CLI have ACP adapters or
speak it natively:

```toml
description = "Claude Code through its ACP adapter"

[acp]
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]
env = { ANTHROPIC_API_KEY = "$ANTHROPIC_API_KEY" }
```

The program starts in the background when you switch to the agent; the first
request waits for it. Its answers, thoughts and tool calls appear in the
session like the built-in agent's, its permission requests use the same
card, and it reads and writes files through TermIDE, so an open editor
follows its edits. The **Model** and **Mode** chips disappear while an
external agent is active: it has its own. Skills, prompt templates and MCP
servers are the agent's own affair too; `model`, `mode` and `tools` in
`agent.toml` do not apply. Switching agents rebuilds the conversation on the
same session log: earlier messages stay on screen but the external agent
does not know them, and the panel says so.

### The system prompt

The prompt the model receives is assembled from files: the template
`ai/AGENTS.md` with placeholders the agent fills in. No prompt text is
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

# Skills
When a task matches one of these, load it with the `skill` tool before starting.
{{skills}}

# Environment
{{environment}}

{{project_instructions}}
```

`{{tools}}` is the tool list with a line per tool, `{{guidelines}}` the rules
the tools themselves contribute, `{{skills}}` the skills by name and
description, `{{environment}}` the working directory, platform, date and
whether it is a git repository, and `{{project_instructions}}` the
instruction files described below. Reword the file, drop a section or add
your own; a placeholder you leave out is simply not sent. **Show system
prompt** in the panel's `[≡]` menu opens the assembled result, so you can see
exactly what the model gets.

### Service prompts

TermIDE's own prompts are files too, under `system/`, at any of the three
levels and seeded on first use like `AGENTS.md`. Compaction, the summary that
replaces the older part of a long session, uses two: `compact.md` is the
system prompt of the summarising call, with the closing user turn in its
front matter (`request:`) and `{{focus}}` where the words given to `/compact`
go; `compacted.md` is the message the summary becomes in the context, with
`{{summary}}` for the model's text. Edit them to change what a summary keeps
or how it is introduced.

```
/compact              summarise now
/compact the API      summarise now, concentrating on the API
```

`/compact` is built in and sits in the `/` list beside your templates; it
waits for a running task like every other switch. Automatic compaction, when
the session approaches the context window, uses the same files.

[Plan mode](#plan-mode) uses `plan.md`: its body is appended to the system
prompt while the mode is on, and `request:` in its front matter is the
message sent when you accept the plan. Reword the body to change what a plan
must contain, or the request to change how the agent is told to go ahead.

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

### Command scripts

Where a template is fixed text, a command script builds the request: an
executable in `commands/<name>`, run as `/name args` with the arguments as
its argv and the panel's directory as its working directory, whose standard
output is sent to the model. A `/review` that gathers `git diff --staged`, a
`/failing` that runs the tests and pastes what broke, an `/issue 123` that
fetches the ticket. Any language will do; the header comments describe it:

```sh
#!/bin/sh
# description: Review the staged changes
# argument-hint: [focus]
# timeout: 30
printf 'Review this diff%s:\n\n' "${1:+ with attention to $1}"
git diff --staged
```

The output appears in the session as your request, so what the model got is
visible. A script that exits with an error, prints nothing or exceeds its
timeout (60 s by default) sends nothing and reports why. Scripts from the
configuration level are your own and run at once; one that came with the
project or the directory asks first, in a card like a permission: run once,
for this session, always (a rule `[agent.permissions.command]` is written)
or not at all. Templates and scripts share the `/` names; when both exist
at the same level, the template wins.

### MCP servers

Tools from [MCP](https://modelcontextprotocol.io) servers join the built-in
ones. A server is a table in `mcp.toml`, at any of the three levels; the same
name higher up replaces the table below, and `enabled = false` there switches
a server off for a project or a directory.

```toml
[github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "$GITHUB_TOKEN" }   # $NAME comes from your environment
tools = ["search_issues", "get_issue", "create_issue"]      # optional: a subset
timeout_secs = 60                                            # startup and one call
```

Servers speak over stdio: TermIDE starts the process when the panel opens,
in the background, and reports in the session when it is connected or why it
is not. Its tools appear as `<server>__<tool>` (for example
`github__search_issues`), the shape OpenAI-compatible endpoints accept; they
are not listed in the system prompt, the model sees their schemas directly.
Every schema travels with every request, so a server with dozens of tools is
worth narrowing with `tools`; the log says so when a server has more than
twenty and no such list.

An MCP tool asks for permission like any other tool that no rule covers,
except in `auto` mode. "Allow always" writes a rule for the tool name:

```toml
[agent.permissions.github__search_issues]
"*" = "allow"
```

### Hooks

A hook is a program TermIDE runs around a tool call, declared in
`hooks.toml` at any of the three levels (same merging as MCP servers). It
gets the event as JSON on standard input and answers with JSON on standard
output, the shape Claude Code, Gemini CLI and Cursor share:

```toml
[no-force-push]
event = "before_tool_call"      # or after_tool_call
tools = ["bash"]                # patterns; every tool when absent
command = "scripts/guard.sh"    # run in the panel's directory
timeout_secs = 30
```

Before a call the input is `{"event","hook","cwd","tool","arguments"}`. The
program may print `{"decision": "block", "reason": "…"}` to skip the call
(the reason goes to the model), `{"decision": "allow"}` to run it without a
permission prompt, or `{"arguments": {…}}` to run it with other arguments;
no decision, or `"ask"`, leaves the permission rules to decide. Exiting with
code 2 blocks too, with standard error as the reason. After a call the input
also carries `"result": {"text", "is_error"}`, and `{"text": "…"}` rewrites
what the model sees; exit code 2 turns the result into an error with
standard error as its text. Hooks run in name order before the permission
rules; a hook that fails in any other way, or exceeds its timeout, is logged
and ignored, so a broken hook never stops the agent.

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
the model and as the agent it last used. When a session approaches the
model's context window, the agent replaces the older part with a summary it
writes itself and keeps the recent messages verbatim; the panel says when this
happens, and `/compact` does it on request (see
[Service prompts](#service-prompts)). `/undo` writes a `rewind` entry: the
log keeps every message, but the branch continues from before the undone
request, on reopening too.

When TermIDE reopens a saved layout, the agent panel comes back with it and
continues the session it was in, on that session's model and as its agent. If
the log has been
deleted the panel starts a fresh session; if no model is configured any more
the panel is left out of the layout.
