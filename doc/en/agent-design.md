# Agent design notes

Working notes for the built-in coding agent (`crates/agent-*`, `crates/panel-agent`).
Each decision is argued from a comparison of existing agents; nothing is copied
from a single one. Facts marked *(unverified)* come from memory, not from a
checked source, and should be confirmed before they are relied on.

Compared: pi 0.85 (TypeScript), Claude Code 2.1 (TypeScript), Codex CLI 0.153
(Rust), Gemini CLI, OpenCode, Goose (Rust), Aider (Python), jcode (Rust),
DeepSeek Harness (TypeScript), Hermes Agent (Python), and Zed's Agent Client
Protocol (ACP).

## 1. Edit tool

| Agent | Format |
|---|---|
| Claude Code | `old_string`/`new_string`, exact and unique match, `replace_all`; the file must have been read first |
| pi | list of `{oldText, newText}` edits, each unique in the *original* file |
| Anthropic `text_editor`, Goose | `str_replace` with unique `old_str`, plus `insert`, `view`, `create` |
| Gemini CLI | `replace` with `expected_replacements`; a model-based corrector repairs a mismatched `old_string` *(unverified)* |
| OpenCode, Cline | `oldString`/`newString` with tolerant matching: exact, then whitespace-normalised, then indentation-flexible, then block anchors *(unverified)* |
| Aider | `<<<<<<< SEARCH / ======= / >>>>>>> REPLACE` blocks in plain text; its benchmark ranks this format best for most models |
| Codex CLI | `apply_patch` grammar (`*** Begin Patch`, `*** Update File:`, `@@` hunks); OpenAI models are trained on it |

Decision: **search/replace with a unique anchor**, one edit per call, `replace_all`
flag. It is the consensus format and works for every model family; `apply_patch`
is tied to one vendor. Add a **tolerant matcher** (exact, then trailing-whitespace
and indentation tolerant) because local models are the first target and they
misquote whitespace most often. Return a unified diff in `details` for the UI.
Claude Code removed its multi-edit tool; pi kept one. We start with single edits
and revisit if transcripts show many sequential edits to one file.

## 2. Read tool

Claude Code, OpenCode and Anthropic's `text_editor` return numbered lines; pi
returns raw text; Codex reads through the shell. Decision: **numbered lines**
(`cat -n` style), `offset`/`limit`, default cap 2000 lines, byte cap, and an
explicit continuation note when truncated. Numbers give the model stable anchors
for `edit` and for `OpenFileAt` links in the panel.

## 3. Shell tool

Every agent has one. Sandboxing exists natively only in Codex (Seatbelt,
Landlock) and optionally in Claude Code and Gemini CLI. Output limits: pi keeps
the last 2000 lines / 50 KB, Claude Code about 30 000 characters, Codex keeps
head and tail *(unverified)*.

Decision: `bash` with a timeout, cooperative cancel that kills the process
group, **head-and-tail truncation** (the command echo and the final error both
survive) and the full output saved to a file whose path is returned. No sandbox
in the first version; the tool takes its environment from `ToolContext`, so a
sandbox can be added later as a separate module without changing the contract.

## 4. Permissions

| Agent | Model |
|---|---|
| Claude Code | modes (`default`, `acceptEdits`, `plan`, `auto`, `bypassPermissions`, `dontAsk`) plus `allow`/`ask`/`deny` rules such as `Bash(git commit *)`, evaluated deny → ask → allow; "always allow" is persisted to `.claude/settings.local.json` |
| Codex CLI | approval policy (`untrusted`, `on-failure`, `on-request`, `never`) × sandbox mode; answers `Approved`, `ApprovedForSession`, `Denied`, `Abort` *(unverified)* |
| OpenCode | `permission` config: `edit`, `webfetch`, and `bash` pattern → `allow`/`ask`/`deny` *(unverified)* |
| Gemini CLI | `default`, `auto_edit`, `yolo` plus a policy engine *(unverified)* |
| Goose | `auto`, `approve`, `smart_approve`, `chat` *(unverified)* |
| pi | none; a `tool_call` hook in an extension may block |
| Hermes | allow-list patterns, approval for dangerous commands |
| ACP | `session/request_permission` with `allow_once`, `allow_always`, `reject_once`, `reject_always` options |

Decision: **rules plus a mode**. Rules are `deny`, `ask`, `allow` lists keyed by
tool and an argument pattern (`bash: "git push *"`, `edit: "src/**"`), evaluated
deny → ask → allow. The mode decides unresolved calls: `ask` (default),
`accept-edits` (edits inside the project pass, shell asks), `auto` (everything
passes, for containers). Prompt answers: allow once, allow for the session,
allow always (writes a rule to the project `.termide` config), deny. This is the
Claude Code / OpenCode shape with ACP's answer set; it stays small and is
data-driven, so the panel and a future ACP client render the same prompt.

Chosen TOML shape (OpenCode-style tables, so "allow always" appends one key):

```toml
[agent.permissions]
mode = "ask"        # ask | accept-edits | auto

[agent.permissions.bash]
"git status*" = "allow"
"git push*"   = "ask"
"rm -rf *"    = "deny"

[agent.permissions.edit]
"src/**" = "allow"
".env"   = "deny"
```

Implementation (`crates/agent-core/src/permissions.rs`): `*` spans any text
including slashes, a leading `**/` is optional. Subjects are the command for
`bash` and the project-relative path for file tools. Shell commands are split
on `&&`, `||`, `;`, `|` and newlines with quote awareness, as Claude Code
does: every part needs its own allow, a deny or ask on any part wins, and
command substitution is never auto-allowed. A built-in read-only command list
(`ls`, `cat`, `rg`, `git status`, `find` without `-delete`/`-exec`, ...)
skips the prompt in every mode, the way Claude Code's read-only Bash set and
Codex's `is_safe_command` do; redirections disqualify. Reads inside the
project never prompt. Suggested rule for "allow always": `git push *` style
for shell (first two words for `git`, `cargo`, `npm`, ...), the exact path for
files.

## 5. Hooks and extension mechanism

| Mechanism | Who uses it |
|---|---|
| MCP servers for tools | Claude Code, Codex, Gemini, Goose, OpenCode, Hermes, jcode |
| `SKILL.md` skills (agentskills.io) | Claude Code, Codex, pi, Hermes, OpenCode |
| Context files (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`) | all |
| External command hooks, JSON on stdin/stdout, exit-code semantics | Claude Code (also `http`, `prompt`, `agent` hook types), Codex `hooks.json` *(unverified)* |
| In-process scripting | pi (TypeScript via jiti), OpenCode (TypeScript plugins), DeepSeek Harness (everything is a Cordis plugin) |

Every Rust agent (Codex, Goose, jcode) extends through MCP and data files, none
embeds a scripting language. Decision: **levels 0–2 only** for now: data files,
external processes (MCP for tools, command hooks with a JSON protocol modelled on
Claude Code's `PreToolUse`/`PostToolUse` shape: `permissionDecision`,
`updatedInput`, `additionalContext`, exit code 2 = block), and Rust traits.
Embedded Lua stays a documented option, not a plan.

## 5a. Instruction files and system prompt

`AGENTS.md` is the cross-vendor convention (Codex, pi, Gemini CLI, Cursor,
Zed, Amp); Claude Code reads `CLAUDE.md`. pi and Claude Code walk every
ancestor of the working directory from the root down, Codex starts at the
repository root; Codex caps a file at 32 KiB, Claude Code at 4 MiB. Claude
Code and Gemini support `@file` imports, the others do not.

Decision (`crates/agent-core/src/context.rs`): the project root's file when
the panel works outside the project, then per ancestor from the root down to
the working directory
`AGENTS.md`, falling back to `CLAUDE.md` in the same directory; 32 KiB cap; no
imports. Files are appended under a `# Project instructions` heading with
their path as a sub-heading (Markdown, not pi's XML wrapper, because small
local models follow Markdown more reliably).

Where the agent's own files live:

| Agent | Directory | Contents |
|---|---|---|
| Claude Code | `~/.claude/`, `.claude/` | `CLAUDE.md`, `agents/*.md` (body = system prompt), `skills/*/SKILL.md`, `commands/*.md` |
| Codex CLI | `~/.codex/` | `config.toml`, `AGENTS.md`, `prompts/*.md`, `skills/` |
| pi | `~/.pi/agent/`, `.pi/` | `AGENTS.md`, `SYSTEM.md`, `APPEND_SYSTEM.md`, `skills/`, `prompts/`, `extensions/` |
| OpenCode | `~/.config/opencode/`, `.opencode/` | `AGENTS.md`, `agent/*.md`, `command/*.md` |
| cross-agent | `.agents/` | `skills/*/SKILL.md` (agentskills.io) |

Decision (`crates/agent-core/src/layers.rs`): three roots — `.termide/ai/`
in the panel's working directory, the same in the termide project root, and
`ai/` in the configuration directory — highest first. A single file comes
from the first root that has it; a directory of named entries is the union
with higher names hiding lower ones, which is how agents, skills and prompts
will merge. Session logs go to `<config>/ai/sessions/<panel directory>/`,
keyed by the directory the panel works in: the user's decision, the shape pi
and Claude Code use (sessions under the tool's own directory), taken over the
XDG data/config split. The configuration level is laid out on first use —
`AGENTS.md` seeded from the shipped data file, empty `agents/`, `skills/` and
`prompts/` — and files present are never touched again.

Agent definitions: Claude Code's `agents/*.md` and OpenCode's `agent/*.md`
carry the settings (`description`, `model`, `tools`, `permissionMode` /
`permission`) as YAML front matter above the prompt body. Decision: the
prompt stays a plain Markdown file (`SOUL.md`) and the settings go beside it
in `agent.toml` — termide is TOML throughout, and a prompt without front
matter can be copied from and to any other tool. `default` exists without
files; switching agents goes through an `AgentCatalog` trait the app
implements over the directories, so the panel chooses among definitions
without knowing how they are stored. The switch is one `AgentRuntime::update`
closure applied between runs (prompt, tools, model); the mode goes through
the shared `ModeHandle`. Model and mode change only when the definition names
them, so a user's runtime choice survives switching to an agent that has no
opinion.

Skills (`skills/<name>/SKILL.md`, agentskills.io front matter) are a
cross-agent format; how they reach the model differs:

| Agent | In the prompt | Loading the body |
|---|---|---|
| Claude Code | names and descriptions | a `Skill` tool pulls the body into the context |
| Codex CLI | names, descriptions and paths | the model reads the file with its read tool |
| pi | names, descriptions and paths | the same, through `read` |
| OpenCode | explicitly enabled skills in full | none |

Decision: names and descriptions under `{{skills}}`, one line each, and a
`skill` tool that takes the name. Per request the two options cost the same
— a list line per skill, cached with the prompt prefix — and the tool's
schema adds a few dozen tokens, also cached. The difference is on the load:
a name is two or three tokens and an `enum` in the schema, where a path is
thirty and a thing a local model mistypes; the body comes back verbatim,
without `read`'s line-number prefixes (a few tokens per line), together with
the skill's companion files, which `read` would need a second call to
discover; and a skill in the configuration directory lies outside the
project, where `read` would have to ask permission — `skill` never does.
Skills are found in each level's `ai/skills` and in `.agents/skills`, the
shared directory, so nothing has to be copied to work with termide. The tool
exists only when a skill does, and an agent's `tools` list does not remove
it: skills are instructions, not a capability.

The prompt is a template with `{{tools}}`, `{{guidelines}}`,
`{{environment}}` and `{{project_instructions}}` placeholders: the `ai`
directory's root `AGENTS.md` for the default agent (the user's decision — the
root file of the directory is the default prompt, and a global instruction
file would only duplicate what one can write into it), `agents/<name>/SOUL.md`
for a custom agent, which falls back to the root file when it has none. No
prompt text is code: the seed is the data file
`crates/agent-core/assets/AGENTS.md` (the former fixed prompt, base
guidelines included), copied to the configuration on first use; code only
fills the placeholders from tool metadata, the environment and the instruction
files. A file replaces the template whole rather than layering
`identity`/`append` overrides, so what the user reads is what the model gets;
the panel's **Show system prompt** writes the assembled text next to the
session logs and opens it. The name is termide's own: only pi calls the file
`SYSTEM.md`, Claude Code and OpenCode keep the prompt in the agent's Markdown
body, and Codex has no such file, so there is no convention to follow.

## 6. Sessions

pi: JSONL tree with `parentId`. Claude Code: JSONL with `parentUuid`,
sidechains, per-block assistant records, file-history snapshots. Codex: rollout
JSONL with typed items *(unverified)*. OpenCode, Goose, Hermes: SQLite.

Decision: **append-only JSONL, one file per session** under the termide data
dir, every entry with `id` and `parent_id` so branching needs no migration.
Search across sessions is out of scope; if it comes, termide's `db` crate exists.

## 6a. Compaction

pi compacts when the context exceeds `window - reserve`, inside the run,
keeping a `retainedTail` of recent messages verbatim and retrying after an
overflow error. Claude Code compacts near the window with a summary that
preserves requests, decisions, files, errors and pending work, then re-reads
recently touched files. Codex has `model_auto_compact_token_limit` and a
summary prompt.

Decision (`crates/agent-core/src/compaction.rs`): threshold check before every
model call using the last reported usage plus a characters-over-four estimate
for later messages; the reserve (default 16 K tokens) is capped at a quarter of
the window so short-context local models still work. The summary is produced
by the same model with a fixed six-point prompt (Claude Code's list); the most
recent messages within a token budget (`keep_recent_tokens`, default 4 K,
capped at a quarter of the window) stay verbatim, as pi's retained tail does,
and a tool call is never split from its results. A summary shorter than 40
characters is rejected as degenerate and the transcript stays untouched: a
live run showed a small model answering `{}` when asked to summarise a lone
prompt. The summary message ends with an instruction to continue the task. An
overflow error from the provider triggers one compaction and one retry. The transcript becomes `[summary user message] +
tail`; the session log records a `compaction` entry with `keep_last`, so
reopening rebuilds the same context. Re-reading touched files is left to the
model.

## 6b. The panel

Layout follows pi, Claude Code and Codex: transcript above, multi-line input
below. `Enter` sends, `Shift+Enter` adds a line, `Esc` aborts a run and then
clears the input. A message typed while the agent works is queued as a
steering message rather than starting a second run.

Tool calls collapse to one line (`▸ bash ls -la ✓`) and expand on click or
`Ctrl+O`, as in pi and Claude Code; the transcript renders through
`crates/richtext` so answers get real Markdown with syntax-highlighted code.
Lines are cached per item, so a streaming token re-renders one message rather
than the whole history.

Permission prompts use termide's own selection modal instead of an in-transcript
card (Zed's ACP style): the modal is app-global and cannot be missed in an
unfocused panel. Crossing the thread boundary needs care — the agent thread
blocks inside `before_tool_call` while the UI thread owns the modal — so the
prompter sends the request over a channel and waits with a timeout, checking
the shared `CancelToken` so an aborted run never hangs on an unanswered
prompt. The answer travels back through a new `PanelCommand::SelectionMade`,
which also makes `SelectAction::Custom` work for any panel (it was a no-op
before).

The title is the session's name when it has one, else the first prompt,
else the working directory, so stacked agent panels stay apart; renaming goes
through the panel's `[≡]` menu and appends a `session_name` entry to the log,
the way pi's `/rename` and Claude Code's `-n` name a session. That prompt
needed the input twin of the selection round-trip:
`InputAction::Custom` → `PanelCommand::InputSubmitted`.

Resuming: the panel keeps everything its agent was built from (provider,
tools, model, prompt, rules, compaction policy), so switching sessions is a
rebuild rather than a new panel; `persist_rule` is a plain `fn` pointer rather
than a closure so it survives that rebuild. The `[≡]` menu offers **New
session** and **Open session** (a picker over `Session::list`, newest first,
the current one marked), mirroring pi's `/resume` and Claude Code's
`--resume`. A switch is refused while a run is in flight: simpler than
draining the old worker, and it never leaves a half-finished turn in a log.

Session logs live in `<config>/ai/sessions/<panel directory>/`; the
picker lists the sessions of the directory the panel works in.

Switching model and mode at runtime:

| Agent | Model | Permission mode |
|---|---|---|
| Claude Code | `/model` picker over a fixed list plus a typed id | `Shift+Tab` cycles default → accept-edits → plan; not persisted |
| Codex CLI | `/model` picker | `/approvals` picker |
| pi | `/model` picker over its registry, `Ctrl+P` cycles | none (no modes) |
| OpenCode | `Ctrl+X M` list from models.dev | `Tab` toggles the build/plan agents |
| Aider | `/model <id>` typed | none |

Decisions: the two status chips are buttons, mirrored in the `[≡]` menu, and
`Shift+Tab` cycles the mode as in Claude Code. The model list comes from the
endpoint's own `GET /models` (every OpenAI-compatible server answers it),
fetched on a helper thread and shown when it arrives, with a typed-id entry
last and as the whole picker when the endpoint cannot list; a config-side
model list would be a second place to keep in sync with the server. vLLM and
omlx put `max_model_len` on each entry, and the panel takes it as the context
window of the model it switches to, since the configured figure belongs to
the configured model. The mode
is a `ModeHandle` — an atomic shared with the hooks on the agent thread, the
way `CancelToken` is — so a switch applies to the next tool call of a run in
flight; the model goes to the worker as a `SetModel` command and is refused
while a run is active, since the worker reads commands only between runs. A
switch is recorded as a `model_change` entry in the session log and a new
session records its starting model, so resume continues on the session's model
(pi's behaviour) rather than the config's. Neither switch is written to the
config: the chips are per-panel state, as Claude Code's `Shift+Tab` is.

Edits and open editors: VS Code, Zed and JetBrains reload a clean buffer
when its file changes on disk and keep the cursor; a dirty buffer keeps its
work and shows a conflict. termide's editor now does the same for every
on-disk change, so the agent needs no special path into the editor. It only
speeds the reload up: a successful `edit` or `write` result carries the path
in its details, and the panel raises `PanelEvent::FileChangedOnDisk`, which
the app fans out like a watcher batch — at once, and also for paths the
watcher drops under `.gitignore`.

Persisted in a termide project layout as `PanelState::Agent { cwd, session }`:
the working directory and the path of the session log. That is enough because
the log carries the model and the rest (endpoint, rules, prompt) is
configuration, which the layout-restore constructor now receives as
`AgentSettings` alongside the editor config. Zed's ACP threads and JetBrains'
tool windows restore the same way — a reference to the conversation, not its
content — and Claude Code's `--continue` is the CLI shape of it. A missing log
starts a fresh session in the same project; no configured model skips the
panel, as an unavailable image backend skips an image panel.

## 7. Panel ↔ agent boundary

ACP is becoming the editor-side standard: Gemini CLI speaks it natively, Claude
Code and Codex have adapters, Zed, Neovim and JetBrains are clients. Decision:
shape the panel's contract after ACP (`prompt`, `session/update` with
`tool_call` status `pending`/`in_progress`/`completed`/`failed`,
`request_permission` with the four answers). The built-in agent is the first
backend; an ACP client over stdio can be a second one later and reuse the same
panel. No dependency on the `agent-client-protocol` crate for now.

## 8. Provider

One wire format first: OpenAI-compatible streaming chat completions
(`crates/agent-providers`), which covers llama.cpp, Ollama, vLLM, omlx,
OpenRouter and most gateways. Vendor differences are data in `Compat`
(`max_tokens_field`, `reasoning_effort`, `send_reasoning`, `extra_body`), the
way pi's per-model `compat` table works, instead of one code path per vendor.

Reasoning between turns: Anthropic requires thinking blocks to be echoed with
their signature; DeepSeek rejects an echoed `reasoning_content`; vLLM and Qwen
accept it for the current turn. Decision: keep thinking in the transcript for
the UI and the session, do not send it back by default, `send_reasoning`
opts in. An Anthropic provider will need a signature on the thinking block.

Retries: pi retries at the session level (3 attempts, 2 s base), Claude Code and
Codex inside the client. Decision: inside the provider, only while no content
has arrived (a half-streamed answer is returned as an error, not replayed),
exponential backoff, surfaced as `StreamEvent::Retry` so the panel can show
the wait. Transient: transport errors, 408, 409, 425, 429, 5xx.

Verified against the local omlx server (Qwen3.8 27B): `reasoning_content`
deltas, keepalive chunks with empty content, `tool_calls` deltas with an index
and complete arguments in one chunk, `finish_reason: "tool_calls"`, `[DONE]`.
That transcript is a replay test in `sse.rs`.

## 9. Loop

Kept from the first increment (`crates/agent-core`): one turn = one assistant
message plus its tool batch; the provider stream never fails; steering at turn
boundaries, follow-up when the agent would stop; `before_tool_call` may block.
Added after the comparison: an `after_tool_call` hook (Claude Code
`PostToolUse`, pi `afterToolCall`) so an external hook can rewrite a result, and
an `updated_input` field on allow so a hook can rewrite arguments.
