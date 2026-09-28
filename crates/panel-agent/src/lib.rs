//! The coding agent panel: a transcript above a multi-line input, tool calls
//! collapsed to one line each, permission prompts routed through termide's
//! selection modal.
//!
//! The panel owns an [`AgentRuntime`] and mirrors its events into a
//! [`Transcript`] from `tick()`, so it never blocks the UI thread. Every
//! transcript change also goes to the JSONL [`Session`] when one is attached.

mod events;
mod pending;
mod select;
mod slash;
mod toolset;
mod transcript;

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use termide_agent_core::{
    civil_date, now_millis, permission_channel, question_channel, Agent, AgentRuntime, Backend,
    BackendModel, BackendSetup, CancelToken, ChainedHooks, CheckpointHooks, CheckpointStore,
    CommandScript, CompactionPolicy, CompactionPrompts, Decision, EntryKind, GoalPrompt,
    HandoffPrompt, Hooks, HostTools, LateTools, LoggedMessage, Message, Mode, ModeHandle,
    ModelInfo, ModelSpec, PermissionEnvelope, PermissionHooks, PermissionRules, PersistRule,
    PersistScope, PlanGuard, PlanPrompt, PromptError, PromptTemplate, Provider, QuestionEnvelope,
    Session, SessionSummary, SkillInfo, Timing, Tool, ToolRegistry, ToolResultMessage, UserMessage,
    DEFAULT_AGENT,
};
use termide_config::Config;
use termide_core::{
    CommandResult, ConfirmAction, InputAction, KeyChord, Panel, PanelCommand, PanelEvent,
    RenderContext, ScrollAxis, ScrollBars, SegmentKind, SelectAction, StatusSegment, ThemeColors,
    WidthPreference,
};
use termide_theme::Theme;
use termide_ui::textarea::TextArea;
use termide_ui::{
    ChoiceAction, ChoiceForm, ClickTracker, CompletionAction, CompletionItem, CompletionList,
    FieldEdit, InputBar, ScrollBar,
};

use crate::pending::Pending;
use crate::toolset::{Blocked, ToolsetGuard, TOOLSET_ACTION};

pub use transcript::{FoldMode, Item, NoticeKind, Transcript};

/// A paste past either bound is held as a short placeholder rather than
/// inlined, so a big block does not swamp the prompt box.
const PASTE_MAX_CHARS: usize = 2000;
const PASTE_MAX_LINES: usize = 5;
/// A duration in whole milliseconds, saturated to fit a `u32`.
fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX)) as u32
}

/// Context-menu action that renames the session.
const RENAME_ACTION: &str = "agent_rename";
/// Context-menu action that deletes the session (behind a confirmation).
const DELETE_SESSION_ACTION: &str = "agent_delete_session";
/// Selection action for the F4 checkpoint-rollback picker.
const ROLLBACK_ACTION: &str = "agent_rollback";
/// Context-menu action that starts a fresh session.
const NEW_SESSION_ACTION: &str = "agent_new_session";
/// Context-menu action that opens the session picker.
const RESUME_ACTION: &str = "agent_resume";
/// Status chip and context-menu action that opens the model picker.
const MODEL_ACTION: &str = "agent_model";
/// Status/banner action that switches the connection.
const CONNECTION_ACTION: &str = "agent_connection";
/// Input action carrying a model id typed by hand.
const MODEL_INPUT_ACTION: &str = "agent_model_input";
/// Status chip and context-menu action that opens the permission-mode picker.
const MODE_ACTION: &str = "agent_mode";
/// Status chip that toggles whether the model is asked to reason.
const REASONING_ACTION: &str = "agent_reasoning";
/// Context-menu action that opens the assembled system prompt in a viewer.
const SHOW_PROMPT_ACTION: &str = "agent_show_prompt";
/// Context-menu action that opens the session-info modal (also F3, `/usage`).
const SESSION_INFO_ACTION: &str = "agent_session_info";
/// Status chip and context-menu action that opens the agent picker.
const AGENT_ACTION: &str = "agent_agent";
/// Context-menu action that opens the prompt-template picker.
const PROMPTS_ACTION: &str = "agent_prompts";
/// The built-in `/compact [focus]` command.
const COMPACT_COMMAND: &str = "compact";
/// The built-in `/undo` command.
const UNDO_COMMAND: &str = "undo";
/// The built-in `/new` command: start a fresh session, keeping the current one
/// in the list.
const NEW_COMMAND: &str = "new";
/// The built-in `/clear` command: discard the current session and start a fresh
/// one in its place.
const CLEAR_COMMAND: &str = "clear";
/// The built-in `/rename` and `/name` commands: rename the session, either from
/// an argument or through the same prompt as the menu.
const RENAME_COMMAND: &str = "rename";
const NAME_COMMAND: &str = "name";
/// The built-in `/pause` and `/continue` commands: stop the run gracefully
/// after the current step, and resume it.
const PAUSE_COMMAND: &str = "pause";
const CONTINUE_COMMAND: &str = "continue";
/// The built-in `/loop` command: re-run a prompt on an interval or back-to-back.
const LOOP_COMMAND: &str = "loop";
/// A `/loop` stops itself after this many iterations, so it cannot run away.
const LOOP_MAX_ITERATIONS: usize = 100;
/// The built-in `/goal` command: work autonomously toward a goal, a judge
/// deciding after each turn whether it is reached.
const GOAL_COMMAND: &str = "goal";
/// A `/goal` stops itself after this many work turns, so it cannot run away.
const GOAL_MAX_ITERATIONS: usize = 50;
/// The built-in `/handoff` command: distil the unfinished work into a brief for
/// a fresh session or another agent.
const HANDOFF_COMMAND: &str = "handoff";
/// The built-in `/usage` command: open the session-info modal (same as F3).
const USAGE_COMMAND: &str = "usage";
/// The built-in `/prompt` command: open the assembled system prompt in a viewer.
const PROMPT_COMMAND: &str = "prompt";
/// Every built-in `/name`, whatever the state; a template, script or skill
/// of the same name never runs under it (see `slash`).
const BUILTIN_COMMANDS: [&str; 13] = [
    UNDO_COMMAND,
    COMPACT_COMMAND,
    NEW_COMMAND,
    CLEAR_COMMAND,
    RENAME_COMMAND,
    NAME_COMMAND,
    PAUSE_COMMAND,
    CONTINUE_COMMAND,
    LOOP_COMMAND,
    GOAL_COMMAND,
    HANDOFF_COMMAND,
    USAGE_COMMAND,
    PROMPT_COMMAND,
];
/// Welcome-banner action that explains the `/name`s defined more than once.
const SLASH_CONFLICTS_ACTION: &str = "agent_slash_conflicts";
/// Context-menu action that undoes the last request.
const UNDO_ACTION: &str = "agent_undo";

/// Everything the app resolves from configuration before opening the panel.
///
/// The panel keeps these so it can rebuild its agent when the user switches
/// to another session.
pub struct AgentPanelSetup {
    pub cwd: PathBuf,
    /// Name of the agent definition in use.
    pub agent: String,
    /// Resolves agent definitions when the user switches agents.
    pub catalog: Arc<dyn AgentCatalog>,
    /// Tools that arrive after the start (MCP servers connecting).
    pub late_tools: Option<Receiver<LateTools>>,
    /// Builds the hooks that run before the permission rules (command hooks);
    /// a factory, since every session switch spawns a fresh agent.
    pub hooks: Option<HooksFactory>,
    /// An external agent to drive instead of the built-in loop.
    pub backend: Option<BackendFactory>,
    /// The CLI agent (Claude Code, Codex) the connection drives over
    /// ACP, if it names one; it wins over an agent definition's own backend.
    pub provider_backend: Option<BackendFactory>,
    /// The connections a session can switch to; `None` offers none.
    pub connections: Option<Arc<dyn ConnectionCatalog>>,
    /// The connection in use.
    pub connection: String,
    pub provider: Arc<dyn Provider>,
    /// The provider's wire-protocol type (e.g. `openai_compatible`), recorded
    /// in the session log so a resume can rebuild the right provider.
    pub provider_kind: String,
    pub model: ModelSpec,
    pub tools: ToolRegistry,
    pub rules: PermissionRules,
    pub system_prompt: String,
    pub compaction: CompactionPolicy,
    /// The texts of a compaction, from the agent directory's `system/` files.
    pub compaction_prompts: CompactionPrompts,
    /// Plan mode's instructions and the request that carries a plan out.
    pub plan_prompt: PlanPrompt,
    /// The goal-judge texts, from the agent directory's `system/goal.md`.
    pub goal_prompt: GoalPrompt,
    /// The handoff-brief texts, from the agent directory's `system/handoff.md`.
    pub handoff_prompt: HandoffPrompt,
    /// Where "allow always" rules go; a plain function so it survives a
    /// session switch. `None` keeps such rules in memory only.
    pub persist_rule: Option<PersistFn>,
    /// Directory holding this project's session logs; `None` runs without
    /// persistence and without the session picker.
    pub session_dir: Option<PathBuf>,
    /// Session to start in; `None` creates one in `session_dir`.
    pub session: Option<Session>,
    /// When reasoning and tool calls fold to their headline (the answer
    /// always shows).
    pub fold: FoldMode,
}

/// Records an "allow always" rule outside the panel, in the project's or the
/// global configuration.
pub type PersistFn = fn(&str, &str, Decision, PersistScope);

/// Makes the extra hooks of one agent (command hooks from `hooks.toml`).
pub type HooksFactory = Arc<dyn Fn() -> Box<dyn Hooks> + Send + Sync>;

/// Starts an external agent (ACP) in place of the built-in loop.
pub type BackendFactory =
    Arc<dyn Fn(BackendSetup) -> Result<Box<dyn Backend>, String> + Send + Sync>;

/// One connection the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionEntry {
    pub name: String,
    /// Its wire protocol or CLI agent, as `[ai] provider` spells it.
    pub kind: String,
    pub model: String,
}

/// A connection made ready for an agent to run on.
pub struct ConnectionChoice {
    pub name: String,
    pub kind: String,
    pub provider: Arc<dyn Provider>,
    /// The connection's model and context window; the rest of the spec (output
    /// bound, reasoning) is the panel's own.
    pub model: String,
    pub context_window: u64,
    /// The CLI agent it drives over ACP instead of the built-in loop.
    pub backend: Option<BackendFactory>,
}

/// The app's connections (`[ai.connections.<name>]`); the
/// panel only chooses among them.
pub trait ConnectionCatalog: Send + Sync {
    fn list(&self) -> Vec<ConnectionEntry>;
    /// Connection `name` built for `agent`; `None` when it does not exist.
    fn build(&self, name: &str, agent: &str) -> Option<ConnectionChoice>;
    /// The panel switched to `choice`: what it hands off (a delegated task)
    /// follows. The default hands nothing off.
    fn activate(&self, _choice: &ConnectionChoice) {}
}

/// One agent the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    pub name: String,
    pub description: String,
}

/// What an agent definition changes about the panel's agent. `None` keeps
/// the current model or mode; the prompt and the tools always come from the
/// definition.
pub struct AgentProfile {
    pub system_prompt: String,
    pub tools: ToolRegistry,
    pub model: Option<String>,
    pub mode: Option<Mode>,
    /// Tools still connecting (MCP servers); they join `tools` as they come.
    pub late_tools: Option<Receiver<LateTools>>,
    /// An external agent to drive instead of the built-in loop.
    pub backend: Option<BackendFactory>,
    /// Every tool the agent has before a session switches any off, by name,
    /// in registry order.
    pub offered: Vec<String>,
    /// Every skill the agent has, by name.
    pub skills: Vec<String>,
}

/// The app's view of the agent definitions (`agents/<name>/` across the
/// agent directories); the panel only chooses among them.
pub trait AgentCatalog: Send + Sync {
    fn list(&self) -> Vec<AgentEntry>;
    fn resolve(&self, name: &str) -> Option<AgentProfile>;
    /// `name`'s profile with what a session switched off (`off`: tool names,
    /// `skill:<name>`) left out of its registry and its prompt. The default
    /// can only drop tools from the registry; a catalog that builds the
    /// prompt rebuilds it without them.
    fn resolve_without(&self, name: &str, off: &BTreeSet<String>) -> Option<AgentProfile> {
        let mut profile = self.resolve(name)?;
        let offered: Vec<String> = profile
            .tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        for tool in off {
            profile.tools.remove(tool);
        }
        if profile.offered.is_empty() {
            profile.offered = offered;
        }
        Some(profile)
    }
    /// Prompt templates (`prompts/<name>.md`), for `/<name>` in the input.
    fn prompts(&self) -> Vec<PromptTemplate> {
        Vec::new()
    }
    /// Command scripts (`commands/<name>`), for `/<name>` in the input.
    fn commands(&self) -> Vec<CommandScript> {
        Vec::new()
    }
    /// Skills, for `/<name>` and `/skill:<name>` in the input.
    fn skills(&self) -> Vec<SkillInfo> {
        Vec::new()
    }
    /// The session's permission mode is now `mode`: what the catalog runs
    /// on the session's behalf (a delegated task) follows. The default runs
    /// nothing.
    fn set_mode(&self, _mode: Mode) {}
}

/// What the agent is doing right now, for the live activity indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the model's first token.
    Prefill,
    /// Streaming the model's answer.
    Generating,
    /// A tool is running.
    Tool,
    /// The conversation is being compacted.
    Compact,
}

/// A run control on the prompt box's top border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunButton {
    /// `[‖]`: pause at the next step, like `/pause`.
    Pause,
    /// `[▶]`: resume a paused run, or withdraw a pause not reached yet, like
    /// `/continue`.
    Continue,
    /// `[■]`: stop the run, like `Esc`.
    Stop,
}

/// Live state of the current run: the phase, when it started, and enough to
/// estimate the generation speed until the authoritative `Usage` arrives.
#[derive(Debug, Clone, Copy)]
struct Activity {
    phase: Phase,
    /// When the current phase started (for its ticking elapsed time).
    since: Instant,
    /// Characters streamed in the current generation, for a rough live token
    /// count and speed (reconciled to `Usage` at `MessageEnd`).
    gen_chars: usize,
    /// When the current model message began (`MessageStart`), for the block's
    /// prefill/generation split in its cost footer.
    msg_start: Instant,
    /// When the first token of the current message arrived.
    first_token: Option<Instant>,
}

impl Activity {
    fn new(phase: Phase) -> Self {
        let now = Instant::now();
        Self {
            phase,
            since: now,
            gen_chars: 0,
            msg_start: now,
            first_token: None,
        }
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.since = Instant::now();
        self.gen_chars = 0;
    }

    /// Rough live token count from streamed characters (~4 chars per token).
    fn est_tokens(&self) -> u64 {
        (self.gen_chars / 4) as u64
    }

    /// The finished turn's cost from the phase timings and token `usage`:
    /// prefill (start→first token) and generation (first token→now).
    fn cost(&self, input: u64, output: u64) -> transcript::Cost {
        let ms = |d: Duration| d.as_millis() as u32;
        let prefill_ms = self.first_token.map_or(0, |ft| ms(ft - self.msg_start));
        let gen_ms = self
            .first_token
            .map_or(0, |ft| ms(ft.elapsed()))
            .min(ms(self.msg_start.elapsed()));
        transcript::Cost {
            prefill_ms,
            gen_ms,
            input,
            output,
        }
    }
}

/// A large paste kept out of the input: `placeholder` stands in the prompt
/// box, `text` is the full content spliced back in on submit.
struct Paste {
    placeholder: String,
    text: String,
}

/// A running `/loop`: re-submit `prompt` after each run finishes — on
/// `interval`, or back-to-back when `None` — until stopped or the cap is hit.
struct LoopTask {
    prompt: String,
    interval: Option<Duration>,
    /// When the next iteration is due; `None` while a run is in flight.
    next_at: Option<Instant>,
    iterations: usize,
}

/// A running `/goal`: work autonomously toward `goal`. After each work turn a
/// judge decides whether the goal is reached; if not, the panel sends the next
/// continuation turn. Stops when the judge says done, on an error, or at the
/// iteration cap.
struct GoalTask {
    goal: String,
    /// Work turns sent so far.
    iterations: usize,
    /// When the judge call is due (a work turn has finished); `None` while a
    /// work turn or the judge call is in flight.
    judge_at: Option<Instant>,
    /// A judge call is in flight; its verdict arrives as a `GoalJudged` event.
    judging: bool,
}

pub struct AgentPanel {
    runtime: Box<dyn Backend>,
    /// The runtime is an external agent: model and mode are not ours to set.
    external: bool,
    permission_rx: Receiver<PermissionEnvelope>,
    /// The model's questions to the user, from the `question` tool.
    question_rx: Receiver<QuestionEnvelope>,
    /// The question a card in the panel is asking, if any.
    pending: Option<Pending>,
    /// Command scripts the user let run for this session, by name.
    allowed_commands: HashSet<String>,
    /// A command script running on a thread, with the command as typed; its
    /// output becomes a request.
    command_run: Option<Receiver<(String, Result<String, String>)>>,
    /// What the files the agent changes looked like before each request,
    /// for `/undo`; shared with the hook that records them.
    checkpoints: Option<Arc<Mutex<CheckpointStore>>>,
    session: Option<Session>,
    /// The provider's wire-protocol type, recorded on model changes.
    provider_kind: String,
    session_dir: Option<PathBuf>,
    /// Sessions offered by the last picker, in the order they were shown.
    session_choices: Vec<SessionSummary>,
    cwd: PathBuf,
    agent: String,
    catalog: Arc<dyn AgentCatalog>,
    /// Agents offered by the last picker, in the order they were shown.
    agent_choices: Vec<String>,
    /// Prompt templates offered by the last picker, in the order shown.
    prompt_choices: Vec<PromptTemplate>,
    /// Tools still connecting, and those that arrived while a run was in
    /// flight and wait for the worker to be free.
    late_tools: Option<Receiver<LateTools>>,
    waiting_tools: Vec<Arc<dyn Tool>>,
    /// What the session switched off: tool names and `skill:<name>`.
    toolset_off: BTreeSet<String>,
    /// What the running profile (its prompt and registry) was built without.
    /// Switched off but not in it means still in the model's context, so
    /// refused rather than gone.
    context_off: BTreeSet<String>,
    /// `toolset_off` less `context_off`: what the guard refuses.
    blocked: Blocked,
    /// A compaction invalidated the prompt cache: the next moment between
    /// runs rebuilds the context without what is refused.
    context_stale: bool,
    /// Every tool and skill the agent offers, for the checklist.
    offered_tools: Vec<String>,
    offered_skills: Vec<String>,
    /// Every MCP tool that arrived, with its server, switched off or not.
    mcp_arrived: Vec<(String, Arc<dyn Tool>)>,
    model: ModelSpec,
    /// The model from the configuration: the base every session's model is
    /// built on, since the log records only an id and a context window.
    configured_model: ModelSpec,
    /// Live permission mode, shared with the hooks on the agent thread.
    mode: ModeHandle,
    /// Models offered by the last picker, in the order they were shown.
    model_choices: Vec<ModelInfo>,
    /// The external (ACP) agent's models, filled while its picker is open.
    acp_models: Vec<BackendModel>,
    /// Whether the external agent advertised any models (so a Model chip and
    /// picker are worth showing); latched once known.
    acp_has_models: bool,
    /// A model to pre-select on an external CLI agent (from `[ai].model` for a
    /// `claude_code`/`codex` provider), applied once its models are known.
    /// Taken (set to `None`) after the one-shot attempt.
    pending_preferred_model: Option<String>,
    /// Background `list_models` call, polled from `tick()`.
    model_fetch: Option<Receiver<Result<Vec<ModelInfo>, String>>>,
    /// A silent `list_models` call started at construction to adopt the active
    /// model's real context window; polled and cleared in `tick()`. The
    /// configured window is only a fallback until this resolves (or when the
    /// provider reports none).
    context_probe: Option<Receiver<Result<Vec<ModelInfo>, String>>>,
    /// Events produced by a command handler, delivered on the next tick.
    pending_events: Vec<PanelEvent>,

    // Kept to rebuild the agent when switching sessions.
    hooks: Option<HooksFactory>,
    backend: Option<BackendFactory>,
    /// The connection's CLI agent, kept apart from `backend` so a
    /// rebuilt agent profile does not drop it.
    provider_backend: Option<BackendFactory>,
    connections: Option<Arc<dyn ConnectionCatalog>>,
    /// The connection in use.
    connection: String,
    /// The connections the picker last offered, in its order.
    connection_choices: Vec<ConnectionEntry>,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    rules: PermissionRules,
    /// "Allow for this session" grants, held for the panel's lifetime so a
    /// rebuild of the agent (undo, a model or agent switch) keeps them. They
    /// are never written to the configuration; "allow always" goes to `rules`.
    session_rules: PermissionRules,
    system_prompt: String,
    compaction: CompactionPolicy,
    compaction_prompts: CompactionPrompts,
    plan_prompt: PlanPrompt,
    /// The goal-judge texts, kept so `/goal` can start a judge call; passed to
    /// the agent so the judge prompt comes from the `system/` files.
    goal_prompt: GoalPrompt,
    /// The handoff-brief texts, passed to the agent for `/handoff`.
    handoff_prompt: HandoffPrompt,
    /// When blocks fold; passed to each transcript.
    fold: FoldMode,
    /// The worker still has the prompt of the other plan-ness: a mode
    /// switch during a run could not update it, `AgentEnd` retries.
    prompt_stale: bool,
    /// The system prompt last shown as a `#` block, so a new one is surfaced
    /// (before the next message) only when it actually changed.
    shown_system: String,
    persist_rule: Option<PersistFn>,

    transcript: Transcript,
    /// The prompt box: one multi-line [`InputBar`] field, no border or
    /// controls — the panel draws its own separator above it.
    input: InputBar,
    /// Which earlier request the input shows while browsing history with
    /// the arrow keys; `None` while typing.
    history_pos: Option<usize>,
    /// What was being typed when browsing started, restored on the way back.
    draft: String,
    /// The `/command` completion list, while the input is a lone `/word`.
    completion: Option<CompletionList>,
    /// When the open completion is an `@`-file mention, the span it replaces;
    /// `None` for a `/`-command completion, which replaces the whole input.
    completion_span: Option<MentionSpan>,
    /// Consecutive clicks on a form row, so a single click selects and a
    /// double click confirms; keyed by the row index.
    form_clicks: ClickTracker<usize>,
    /// Where the left button went down in the transcript, until it is
    /// released: a release without a drag is a click on the block there.
    press: Option<select::Cell>,
    /// Text selected in the transcript with the mouse, copied by `Ctrl+C`.
    text_selection: Option<select::TextSelection>,
    /// Large pastes held out of the input as a short placeholder, expanded back
    /// inline on submit, so a big block does not swamp the prompt box.
    pastes: Vec<Paste>,
    /// Serial number for the next paste placeholder.
    paste_seq: usize,
    /// Keyboard focus is in the chat, not the input: `Tab` toggles it, then
    /// the arrows pick a block and Space/Enter fold it.
    chat_focus: bool,
    /// The block the chat focus is on, an index into the transcript items.
    selected: usize,
    /// First visible transcript line.
    top: usize,
    /// Keep the view pinned to the newest line while true.
    follow: bool,
    busy: bool,
    /// A run stopped early on `/pause` with work still pending, so `/continue`
    /// can resume it.
    paused: bool,
    /// An active `/loop`: re-runs its prompt after each turn until stopped.
    loop_task: Option<LoopTask>,
    /// A running `/goal`: autonomous work toward a goal with a judge; `None`
    /// when no goal is active.
    goal_task: Option<GoalTask>,
    /// The current goal work turn ended in an error, so the goal loop stops
    /// instead of judging and retrying. Reset at the start of each work turn.
    goal_errored: bool,
    /// When the current run started (`AgentStart`), for its closing line.
    run_start: Option<Instant>,
    /// The current run hit an error or was aborted, so its closing line is `✗`.
    run_failed: bool,
    /// A run ended or a question arrived since the panel was last rendered
    /// focused; its header is highlighted while it is unfocused.
    attention: bool,
    /// The current run stopped at a `/pause` (its closing line says so).
    run_paused: bool,
    /// A `/pause` was asked for and the run has not reached a step boundary
    /// yet; shown in the state strip.
    pause_requested: bool,
    /// A stop was asked for and the run has not ended yet; the stop control
    /// is red until it does.
    stop_requested: bool,
    /// When the current pause began, while the run is paused: its closing
    /// line ticks the pause's length until `/continue`.
    pause_start: Option<Instant>,
    /// A `/continue` resumed the paused run, so the next `AgentStart` keeps
    /// the run's start and its clock goes on from the request.
    resuming: bool,
    /// When the current permission question (or the model's question to the
    /// user) went up, and how long the
    /// running call had already waited before it: the wait is a pause of
    /// its own, shown on the call and kept out of its duration.
    permission_wait: Option<(Instant, u32)>,
    /// The screen row of the state strip's pause line, a click target that
    /// continues the run.
    pause_row: Option<u16>,
    /// The run controls last put on the prompt's border, in order, so a
    /// click maps back to one.
    run_buttons: Vec<RunButton>,
    /// Texts of the steering messages sent while the agent works, oldest
    /// first, shown in the state strip until the agent takes them. Kept in
    /// step with the runtime's steering count (`QueueUpdate`).
    queued_texts: VecDeque<String>,
    queued: (usize, usize),
    /// Tokens of the last reported context, for the status chip.
    context_tokens: u64,
    /// What the agent is doing right now; `None` when idle.
    activity: Option<Activity>,
    /// Session token totals from `Usage`: input (prefill) and output.
    session_input: u64,
    /// Prompt tokens the cache served this session.
    session_cached: u64,
    session_output: u64,
    /// Bytes of shell output before and after cleaning, summed over the
    /// session, for the "output cleaned" diagnostic in the summary.
    clean_raw_bytes: u64,
    clean_out_bytes: u64,
    /// When each running tool started, to report how long it took (`🕒`).
    tool_starts: HashMap<String, Instant>,
    /// Throttles the animation redraws requested while busy.
    last_anim: Instant,

    colors: ThemeColors,
    is_light: bool,
    transcript_area: Rect,
    input_area: Rect,
    scrollbars: ScrollBars,
    /// Clickable fields drawn in the welcome banner, each with the status
    /// action a click on it triggers (re-pick the model, the agent). Rebuilt
    /// every render; empty once the session has content and the banner is gone.
    banner_hits: Vec<(Rect, &'static str)>,
    /// The `/name`s more than one kind defined when the panel opened, for
    /// the welcome banner; a click there explains them.
    shadowed: Vec<String>,
}

impl AgentPanel {
    #[must_use]
    pub fn new(mut setup: AgentPanelSetup) -> Self {
        let session = setup.session.or_else(|| {
            start_session(
                setup.session_dir.as_deref(),
                &setup.cwd,
                &setup.connection,
                &setup.provider_kind,
                &setup.model,
                &setup.agent,
            )
        });
        let model = session_model(&setup.model, session.as_ref());
        let checkpoints = checkpoint_store(setup.session_dir.as_deref(), session.as_ref());
        let (mut agent, mut system_prompt, mut tools, mut late_tools, mut backend) = (
            setup.agent,
            setup.system_prompt,
            setup.tools,
            setup.late_tools,
            setup.backend,
        );
        let (toolset_off, resolved) = session_agent(
            setup.catalog.as_ref(),
            &agent,
            &BTreeSet::new(),
            session.as_ref(),
        );
        // What the running profile was built without: the session's set when
        // it was rebuilt for it, nothing when the set-up one runs.
        let context_off = if resolved.is_some() {
            toolset_off.clone()
        } else {
            BTreeSet::new()
        };
        let mut offered = None;
        if let Some((name, profile)) = resolved {
            agent = name;
            system_prompt = profile.system_prompt;
            tools = profile.tools;
            late_tools = profile.late_tools;
            backend = setup.provider_backend.clone().or(profile.backend);
            offered = Some((profile.offered, profile.skills));
            if let Some(mode) = profile.mode {
                setup.rules.mode = mode;
            }
        }
        // The full lists the checklist offers, the set-up profile's too.
        let (offered_tools, offered_skills) = offered.unwrap_or_else(|| {
            setup
                .catalog
                .resolve_without(&agent, &BTreeSet::new())
                .map(|profile| (profile.offered, profile.skills))
                .unwrap_or_default()
        });
        let blocked: Blocked = Arc::new(RwLock::new(
            toolset_off.difference(&context_off).cloned().collect(),
        ));
        let Spawned {
            runtime,
            permission_rx,
            question_rx,
            transcript,
            mode,
            external,
        } = spawn_runtime(
            &setup.provider,
            &tools,
            &model,
            &setup.cwd,
            &system_prompt,
            setup.rules.clone(),
            setup.compaction,
            &setup.compaction_prompts,
            &setup.plan_prompt,
            &setup.goal_prompt,
            &setup.handoff_prompt,
            setup.persist_rule,
            setup.hooks.as_ref(),
            backend.as_ref(),
            checkpoints.clone(),
            setup.fold,
            session.as_ref(),
            &blocked,
        );
        // Learn the context window from the provider in the background and
        // adopt the active model's real `max_model_len`; the configured window
        // is only a fallback (an external agent has no such endpoint).
        let context_probe = (!external).then(|| spawn_model_list(Arc::clone(&setup.provider)));
        // A CLI provider carries the model to pre-select on its agent in
        // `[ai].model`; a wire-protocol or generic external agent does not.
        let pending_preferred_model = (external
            && termide_config::is_cli_provider(&setup.provider_kind)
            && !model.id.is_empty())
        .then(|| model.id.clone());
        setup.catalog.set_mode(mode.get());
        let mut panel = Self {
            runtime,
            external,
            permission_rx,
            question_rx,
            pending: None,
            allowed_commands: HashSet::new(),
            command_run: None,
            checkpoints,
            session,
            session_dir: setup.session_dir,
            session_choices: Vec::new(),
            cwd: setup.cwd,
            model,
            configured_model: setup.model,
            agent,
            catalog: setup.catalog,
            agent_choices: Vec::new(),
            prompt_choices: Vec::new(),
            late_tools,
            waiting_tools: Vec::new(),
            toolset_off,
            context_off,
            blocked,
            context_stale: false,
            offered_tools,
            offered_skills,
            mcp_arrived: Vec::new(),
            mode,
            model_choices: Vec::new(),
            acp_models: Vec::new(),
            acp_has_models: false,
            pending_preferred_model,
            model_fetch: None,
            context_probe,
            pending_events: Vec::new(),
            hooks: setup.hooks,
            backend,
            provider_backend: setup.provider_backend.clone(),
            connections: setup.connections.clone(),
            connection: setup.connection.clone(),
            connection_choices: Vec::new(),
            provider: setup.provider,
            provider_kind: setup.provider_kind,
            tools,
            rules: setup.rules,
            session_rules: PermissionRules::default(),
            system_prompt,
            compaction: setup.compaction,
            compaction_prompts: setup.compaction_prompts,
            plan_prompt: setup.plan_prompt,
            goal_prompt: setup.goal_prompt,
            handoff_prompt: setup.handoff_prompt,
            fold: setup.fold,
            prompt_stale: false,
            shown_system: String::new(),
            persist_rule: setup.persist_rule,
            transcript,
            input: InputBar::new(vec![])
                .with_multiline_field("")
                .with_placeholder(termide_i18n::t().agent_input_placeholder())
                .with_border(String::new(), String::new()),
            history_pos: None,
            draft: String::new(),
            completion: None,
            completion_span: None,
            form_clicks: ClickTracker::new(),
            press: None,
            text_selection: None,
            pastes: Vec::new(),
            paste_seq: 0,
            chat_focus: false,
            selected: 0,
            top: 0,
            follow: true,
            busy: false,
            paused: false,
            loop_task: None,
            goal_task: None,
            goal_errored: false,
            run_start: None,
            run_failed: false,
            attention: false,
            run_paused: false,
            pause_requested: false,
            stop_requested: false,
            pause_start: None,
            resuming: false,
            permission_wait: None,
            pause_row: None,
            run_buttons: Vec::new(),
            queued_texts: VecDeque::new(),
            queued: (0, 0),
            context_tokens: 0,
            activity: None,
            session_input: 0,
            session_cached: 0,
            session_output: 0,
            clean_raw_bytes: 0,
            clean_out_bytes: 0,
            tool_starts: HashMap::new(),
            last_anim: Instant::now(),
            colors: ThemeColors::default(),
            is_light: false,
            transcript_area: Rect::default(),
            input_area: Rect::default(),
            scrollbars: ScrollBars::default(),
            banner_hits: Vec::new(),
            shadowed: Vec::new(),
        };
        // Names defined twice are reported once, when the panel opens: on the
        // welcome banner of a fresh session (a notice would replace it), as
        // notices under a resumed one.
        if panel.transcript.items().is_empty() {
            panel.shadowed = panel
                .slash_conflicts()
                .into_iter()
                .map(|conflict| conflict.name)
                .collect();
        } else {
            panel.notice_slash_conflicts();
        }
        panel
    }

    /// Replace the running agent with one continuing `session` (or a fresh
    /// one when `None`). Refuses while a run is in flight.
    pub fn switch_session(&mut self, session: Option<Session>) -> bool {
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return false;
        }
        let session = session.or_else(|| {
            start_session(
                self.session_dir.as_deref(),
                &self.cwd,
                &self.connection,
                &self.provider_kind,
                &self.model,
                &self.agent,
            )
        });
        let model = session_model(&self.configured_model, session.as_ref());
        self.checkpoints = checkpoint_store(self.session_dir.as_deref(), session.as_ref());
        let (mut agent, mut system_prompt, mut tools) = (
            self.agent.clone(),
            self.system_prompt.clone(),
            self.tools.clone(),
        );
        let (toolset_off, resolved) = session_agent(
            self.catalog.as_ref(),
            &agent,
            &self.context_off,
            session.as_ref(),
        );
        if let Some((name, profile)) = resolved {
            agent = name;
            system_prompt = profile.system_prompt;
            tools = profile.tools;
            self.late_tools = profile.late_tools;
            self.backend = self.provider_backend.clone().or(profile.backend);
            self.waiting_tools.clear();
            self.mcp_arrived.clear();
            self.offered_tools = profile.offered;
            self.offered_skills = profile.skills;
            self.context_off = toolset_off.clone();
            if let Some(mode) = profile.mode {
                self.rules.mode = mode;
            }
        }
        self.toolset_off = toolset_off;
        self.sync_blocked();
        let blocked = Arc::clone(&self.blocked);
        let Spawned {
            runtime,
            permission_rx,
            question_rx,
            transcript,
            mode,
            external,
        } = spawn_runtime(
            &self.provider,
            &tools,
            &model,
            &self.cwd,
            &system_prompt,
            self.effective_rules(),
            self.compaction,
            &self.compaction_prompts,
            &self.plan_prompt,
            &self.goal_prompt,
            &self.handoff_prompt,
            self.persist_rule,
            self.hooks.as_ref(),
            self.backend.as_ref(),
            self.checkpoints.clone(),
            self.fold,
            session.as_ref(),
            &blocked,
        );
        // Dropping the old runtime cancels it and asks its worker to stop.
        self.runtime = runtime;
        self.external = external;
        self.permission_rx = permission_rx;
        self.question_rx = question_rx;
        self.pending = None;
        self.transcript = transcript;
        // Leaving the current session: if it was never used, delete it so an
        // empty session does not clutter the list or the disk. On a
        // same-session rebuild (switch agent, undo) the caller has already
        // taken the session out, so there is nothing to leave here.
        if let Some(old) = self.session.take() {
            discard_if_empty(old);
        }
        self.session = session;
        self.model = model;
        self.agent = agent;
        self.system_prompt = system_prompt;
        self.tools = tools;
        self.catalog.set_mode(mode.get());
        self.mode = mode;
        self.model_choices.clear();
        self.model_fetch = None;
        self.clear_input();
        self.history_pos = None;
        self.draft.clear();
        self.completion = None;
        self.top = 0;
        self.follow = true;
        self.queued = (0, 0);
        self.queued_texts.clear();
        self.pause_requested = false;
        self.stop_requested = false;
        self.context_tokens = 0;
        true
    }

    /// Sessions of this project, newest first.
    #[must_use]
    pub fn session_list(&self) -> Vec<SessionSummary> {
        self.session_dir
            .as_ref()
            .and_then(|dir| Session::list(dir).ok())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy || self.runtime.is_busy()
    }

    #[must_use]
    pub fn input_text(&self) -> String {
        self.input_area().text()
    }

    /// The prompt box's text area. The input bar holds exactly one multi-line
    /// field, so both accessors always resolve.
    fn input_area(&self) -> &TextArea {
        self.input
            .multiline(0)
            .expect("agent input is a multiline field")
    }

    fn input_area_mut(&mut self) -> &mut TextArea {
        self.input
            .multiline_mut(0)
            .expect("agent input is a multiline field")
    }

    /// Clear the prompt box.
    fn clear_input(&mut self) {
        self.input.set_field_text(0, "");
        self.pastes.clear();
        self.paste_seq = 0;
    }

    /// Insert pasted `text` at the cursor: a small paste inline, a large one as
    /// a short `[#n pasted …]` placeholder whose full content is spliced back
    /// in on [`AgentPanel::submit`].
    fn paste(&mut self, text: &str) {
        let lines = text.lines().count();
        let large = text.chars().count() > PASTE_MAX_CHARS || lines > PASTE_MAX_LINES;
        if !large {
            self.input_area_mut().insert_str(text);
            return;
        }
        // Pasting the same block again unmasks it: the placeholder the first
        // paste left gives way to the full text, to read or edit in place.
        let input = self.input_text();
        if let Some(last) = self.pastes.last() {
            if last.text == text && input.contains(&last.placeholder) {
                let unmasked = input.replacen(&last.placeholder, text, 1);
                self.pastes.pop();
                self.set_input(&unmasked);
                return;
            }
        }
        self.paste_seq += 1;
        let t = termide_i18n::t();
        let label = if lines > 1 {
            t.agent_paste_lines_fmt(lines)
        } else {
            t.agent_paste_chars_fmt(text.chars().count())
        };
        let placeholder = t.agent_paste_placeholder_fmt(self.paste_seq, &label);
        self.input_area_mut().insert_str(&placeholder);
        self.pastes.push(Paste {
            placeholder,
            text: text.to_string(),
        });
    }

    /// Copy the prompt's selection to the clipboard. Returns whether there was
    /// one to copy, so the caller knows whether the key was the prompt's.
    fn copy_input_selection(&mut self) -> bool {
        match self.input_area().selected_text() {
            Some(text) => {
                self.copy_text(&text);
                true
            }
            None => false,
        }
    }

    /// The transcript cell under screen position (`column`, `row`), clamped
    /// into the transcript.
    fn cell_at(&self, column: u16, row: u16) -> select::Cell {
        let area = self.transcript_area;
        let row = row.clamp(area.y, (area.y + area.height).saturating_sub(1));
        let col = column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(2));
        select::Cell {
            line: self.top + (row - area.y) as usize,
            col: col as usize,
        }
    }

    /// A click on transcript line `line`: focus the chat and select the block
    /// there; a second click on the block already selected folds/unfolds it.
    fn click_line(&mut self, line: usize) -> Vec<PanelEvent> {
        // A pause's ticking line resumes the run.
        if self.paused && self.pause_start.is_some() && self.transcript.is_live_pause_line(line) {
            self.resume();
            return vec![PanelEvent::NeedsRedraw];
        }
        // A click on a run's closing line selects the block above it.
        let Some(index) = self
            .transcript
            .item_at_line(line)
            .and_then(|index| self.transcript.selectable_near(index))
        else {
            return vec![PanelEvent::NeedsRedraw];
        };
        if self.chat_focus && self.selected == index {
            self.transcript.toggle_expanded(index);
        } else {
            self.chat_focus = true;
            self.selected = index;
        }
        vec![PanelEvent::NeedsRedraw]
    }

    /// Copy the text selected in the transcript with the mouse. Returns
    /// whether there was any.
    fn copy_text_selection(&mut self) -> bool {
        let Some(selection) = self.text_selection else {
            return false;
        };
        let width = self.transcript_area.width.saturating_sub(1).max(1) as usize;
        let text = selection.text(self.transcript.rendered(), width);
        if text.trim().is_empty() {
            return false;
        }
        self.copy_text(&text);
        true
    }

    /// Copy the prompt's selection and delete it.
    fn cut_input_selection(&mut self) -> bool {
        match self.input_area().selected_text() {
            Some(text) => {
                self.copy_text(&text);
                self.input_area_mut().delete_selection();
                true
            }
            None => false,
        }
    }

    /// Paste the clipboard into the prompt; a large paste is held as a
    /// placeholder rather than flooding the input.
    fn paste_clipboard(&mut self) -> bool {
        match termide_ui::clipboard::paste() {
            Some(text) => {
                self.paste(&text);
                true
            }
            None => false,
        }
    }

    /// Put `text` on the clipboard, telling the user when the clipboard refused.
    fn copy_text(&mut self, text: &str) {
        if let Err(error) = termide_ui::clipboard::copy(text) {
            log::warn!("agent copy failed: {error}");
            self.notice(
                termide_i18n::t().agent_notice_clipboard_failed(),
                NoticeKind::Warn,
            );
        }
    }

    /// Splice every held paste's full content back in place of its placeholder.
    fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for paste in &self.pastes {
            out = out.replace(&paste.placeholder, &paste.text);
        }
        out
    }

    #[must_use]
    pub fn session_path(&self) -> Option<&std::path::Path> {
        self.session.as_ref().map(Session::path)
    }

    /// Send the input box: a new run when idle, a steering message while
    /// the agent works.
    pub fn submit(&mut self) -> Vec<PanelEvent> {
        // Held pastes are spliced back in before anything reads the message, so
        // the full content is what a command, a template or the model sees.
        let text = self.expand_pastes(&self.input_area().text());
        let text = text.trim().to_string();
        if text.is_empty() {
            return vec![];
        }
        self.completion = None;
        self.history_pos = None;
        self.draft.clear();
        // What reaches `send` through a slash arm was expanded from this.
        let command = slash_command(&text).map(|_| text.clone());
        let text = match slash_command(&text) {
            Some((UNDO_COMMAND, _)) => {
                self.clear_input();
                return self.ask_undo();
            }
            Some((COMPACT_COMMAND, focus)) => {
                // Built in: summarise the older part of the session now.
                let focus = (!focus.is_empty()).then(|| focus.to_string());
                match self.runtime.compact(focus) {
                    Ok(()) => self.clear_input(),
                    Err(PromptError::Busy) => {
                        self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn)
                    }
                    Err(error) => self.notice(error.to_string(), NoticeKind::Warn),
                }
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((NEW_COMMAND, _)) => {
                // Start fresh, leaving the current session in the list (empty
                // ones are still dropped by `switch_session`).
                self.clear_input();
                self.switch_session(None);
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((CLEAR_COMMAND, _)) => {
                // Like `/new`, but the current session is deleted rather than
                // kept, so there is nothing to resume back to.
                if self.is_busy() {
                    self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
                    return vec![PanelEvent::NeedsRedraw];
                }
                self.clear_input();
                if let Some(old) = self.session.take() {
                    discard(old);
                }
                self.switch_session(None);
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((RENAME_COMMAND | NAME_COMMAND, args)) => {
                // With a name, rename now; without one, open the same prompt as
                // the `[≡]` menu and F2.
                self.clear_input();
                if args.is_empty() {
                    return self.handle_status_action(RENAME_ACTION);
                }
                if !self.rename_session(args) {
                    self.notice(
                        termide_i18n::t().agent_notice_no_log_to_name(),
                        NoticeKind::Warn,
                    );
                }
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((PAUSE_COMMAND, _)) => {
                self.clear_input();
                if !self.runtime.can_pause() {
                    self.notice(PromptError::Unsupported.to_string(), NoticeKind::Info);
                } else if !self.request_pause() {
                    self.notice(
                        termide_i18n::t().agent_notice_nothing_to_pause(),
                        NoticeKind::Info,
                    );
                }
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((CONTINUE_COMMAND, _)) => {
                self.clear_input();
                if self.paused {
                    self.resume();
                } else if self.pause_requested {
                    // The run has not reached the pause yet: withdraw it.
                    self.cancel_pause();
                } else if self.is_busy() {
                    self.notice(
                        termide_i18n::t().agent_notice_already_running(),
                        NoticeKind::Info,
                    );
                } else {
                    self.notice(
                        termide_i18n::t().agent_notice_nothing_to_continue(),
                        NoticeKind::Info,
                    );
                }
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((LOOP_COMMAND, args)) => {
                self.clear_input();
                let args = args.trim();
                if args.is_empty() || args == "stop" || args == "off" {
                    if self.loop_task.take().is_some() {
                        self.notice(
                            termide_i18n::t().agent_notice_loop_stopped(),
                            NoticeKind::Info,
                        );
                    } else {
                        self.notice(
                            termide_i18n::t().agent_notice_loop_usage(),
                            NoticeKind::Info,
                        );
                    }
                    return vec![PanelEvent::NeedsRedraw];
                }
                let (interval, prompt) = parse_loop_args(args);
                if prompt.is_empty() {
                    self.notice(
                        termide_i18n::t().agent_notice_loop_usage(),
                        NoticeKind::Info,
                    );
                    return vec![PanelEvent::NeedsRedraw];
                }
                let t = termide_i18n::t();
                self.notice(
                    match interval {
                        Some(d) => t.agent_notice_looping_every_fmt(&fmt_secs(d.as_secs())),
                        None => t.agent_notice_looping().to_string(),
                    },
                    NoticeKind::Info,
                );
                self.loop_task = Some(LoopTask {
                    prompt: prompt.to_string(),
                    interval,
                    next_at: None,
                    iterations: 0,
                });
                return self.loop_step();
            }
            Some((GOAL_COMMAND, args)) => {
                self.clear_input();
                let args = args.trim();
                if args.is_empty() || args == "stop" || args == "off" {
                    if self.goal_task.take().is_some() {
                        self.notice(
                            termide_i18n::t().agent_notice_goal_stopped(),
                            NoticeKind::Info,
                        );
                    } else {
                        self.notice(
                            termide_i18n::t().agent_notice_goal_usage(),
                            NoticeKind::Info,
                        );
                    }
                    return vec![PanelEvent::NeedsRedraw];
                }
                if self.is_busy() {
                    self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
                    return vec![PanelEvent::NeedsRedraw];
                }
                return self.start_goal(args.to_string());
            }
            Some((HANDOFF_COMMAND, _)) => {
                self.clear_input();
                return self.start_handoff();
            }
            Some((USAGE_COMMAND, _)) => {
                self.clear_input();
                return self.session_summary();
            }
            Some((PROMPT_COMMAND, _)) => {
                self.clear_input();
                return self.handle_status_action(SHOW_PROMPT_ACTION);
            }
            Some((name, args)) => match slash::resolve(
                name,
                self.catalog.prompts(),
                self.catalog.commands(),
                self.catalog.skills(),
            ) {
                Some(slash::SlashTarget::Template(template)) => template.expand(args),
                Some(slash::SlashTarget::Script(script)) => {
                    // A command script: its output becomes the request, once
                    // it has run (and, for a project's script, been allowed).
                    self.clear_input();
                    self.run_command(script, args.to_string());
                    return vec![PanelEvent::NeedsRedraw];
                }
                // A skill switched off in the toolset still runs by hand: that
                // keeps it out of the model's context, not away from the user.
                Some(slash::SlashTarget::Skill(skill)) => match skill.load(args) {
                    Ok(loaded) => loaded.text,
                    Err(error) => {
                        self.notice(error, NoticeKind::Warn);
                        return vec![PanelEvent::NeedsRedraw];
                    }
                },
                None => {
                    let mut names: Vec<String> =
                        self.catalog.prompts().into_iter().map(|p| p.name).collect();
                    names.extend(self.catalog.commands().into_iter().map(|c| c.name));
                    names.extend(self.slash_skills().into_iter().map(|(name, _)| name));
                    names.push(COMPACT_COMMAND.to_string());
                    if self.session_dir.is_some() {
                        names.push(NEW_COMMAND.to_string());
                        names.push(CLEAR_COMMAND.to_string());
                        names.push(RENAME_COMMAND.to_string());
                        names.push(NAME_COMMAND.to_string());
                    }
                    if self.is_busy() {
                        names.push(PAUSE_COMMAND.to_string());
                    }
                    if self.paused || self.pause_requested {
                        names.push(CONTINUE_COMMAND.to_string());
                    }
                    names.push(LOOP_COMMAND.to_string());
                    names.push(GOAL_COMMAND.to_string());
                    names.push(HANDOFF_COMMAND.to_string());
                    names.push(USAGE_COMMAND.to_string());
                    names.push(PROMPT_COMMAND.to_string());
                    self.notice(
                        termide_i18n::t().agent_notice_no_command_fmt(name, &names.join(", ")),
                        NoticeKind::Warn,
                    );
                    return vec![PanelEvent::NeedsRedraw];
                }
            },
            None => text,
        };
        // A model left to the provider is known once its list arrives; until
        // then there is nothing to send to, and the text stays to send later.
        if !self.external && self.model.id.is_empty() {
            self.notice(
                termide_i18n::t().agent_notice_model_pending(),
                NoticeKind::Warn,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        self.clear_input();
        self.send_as(text, command)
    }

    /// Send `text` as the next request: a new run when idle, a steering
    /// message while the agent works.
    fn send(&mut self, text: String) -> Vec<PanelEvent> {
        self.send_as(text, None)
    }

    /// [`Self::send`] for text a typed `/name args` produced: the transcript
    /// and the input history show the command, the model gets the text.
    fn send_as(&mut self, text: String, command: Option<String>) -> Vec<PanelEvent> {
        self.follow = true;
        let message = UserMessage::text(text).with_command(command);
        if self.is_busy() {
            // The message waits in the state strip until the agent takes it,
            // then shows as a user block.
            self.queued_texts.push_back(message.typed());
            self.runtime.steer(message);
            self.set_queued(self.runtime.queue_lens());
        } else {
            // A fresh turn starts: surface the system prompt as a folded `#`
            // block when it is new or has changed since it was last shown, so it
            // sits just above this message.
            let system = self.effective_system_prompt();
            if system != self.shown_system {
                self.transcript.push(Item::System {
                    text: system.clone(),
                });
                self.shown_system = system;
            }
            // A request starts: from here on the files it touches are kept
            // for /undo, together with where the conversation stood.
            if let Some(store) = &self.checkpoints {
                let leaf = self
                    .session
                    .as_ref()
                    .and_then(Session::leaf_id)
                    .map(str::to_string);
                store.lock().unwrap().begin_run(leaf);
            }
            match self.runtime.prompt(message) {
                Ok(()) => self.busy = true,
                Err(error) => self.notice(
                    termide_i18n::t().agent_notice_cannot_start_fmt(&error.to_string()),
                    NoticeKind::Error,
                ),
            }
        }
        vec![PanelEvent::NeedsRedraw]
    }

    /// Run the next `/loop` iteration: send the loop's prompt as a fresh run,
    /// unless the safety cap has been reached.
    fn loop_step(&mut self) -> Vec<PanelEvent> {
        if self
            .loop_task
            .as_ref()
            .is_some_and(|t| t.iterations >= LOOP_MAX_ITERATIONS)
        {
            self.loop_task = None;
            self.notice(
                termide_i18n::t().agent_notice_loop_stopped_max_fmt(LOOP_MAX_ITERATIONS),
                NoticeKind::Warn,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        let Some(task) = self.loop_task.as_mut() else {
            return vec![PanelEvent::NeedsRedraw];
        };
        task.iterations += 1;
        task.next_at = None;
        let prompt = task.prompt.clone();
        self.send(prompt)
    }

    /// Start a `/goal`: work autonomously toward `goal`, a judge deciding after
    /// each turn whether it is reached. The first work turn is the goal itself.
    fn start_goal(&mut self, goal: String) -> Vec<PanelEvent> {
        self.notice(
            termide_i18n::t().agent_notice_goal_working_fmt(&goal),
            NoticeKind::Info,
        );
        self.goal_task = Some(GoalTask {
            goal: goal.clone(),
            iterations: 0,
            judge_at: None,
            judging: false,
        });
        self.send_goal_turn(goal)
    }

    /// Send one work turn of the active goal as a fresh run and count it;
    /// stops the goal when the safety cap is reached.
    fn send_goal_turn(&mut self, prompt: String) -> Vec<PanelEvent> {
        let over_cap = match self.goal_task.as_mut() {
            Some(task) => {
                task.iterations += 1;
                task.judge_at = None;
                task.iterations > GOAL_MAX_ITERATIONS
            }
            None => return vec![PanelEvent::NeedsRedraw],
        };
        if over_cap {
            self.goal_task = None;
            self.notice(
                termide_i18n::t().agent_notice_goal_stopped_max_fmt(GOAL_MAX_ITERATIONS),
                NoticeKind::Warn,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        self.goal_errored = false;
        self.send(prompt)
    }

    /// Ask the judge whether the active goal is reached; the verdict arrives as
    /// a `GoalJudged` event, applied in [`AgentPanel::on_goal_verdict`].
    fn run_goal_judge(&mut self) -> Vec<PanelEvent> {
        let goal = match self.goal_task.as_mut() {
            Some(task) => {
                task.judge_at = None;
                task.judging = true;
                task.goal.clone()
            }
            None => return vec![PanelEvent::NeedsRedraw],
        };
        match self.runtime.judge(goal) {
            Ok(()) => self.notice(
                termide_i18n::t().agent_notice_goal_checking(),
                NoticeKind::Info,
            ),
            Err(error) => {
                self.goal_task = None;
                self.notice(
                    termide_i18n::t().agent_notice_cannot_check_goal_fmt(&error.to_string()),
                    NoticeKind::Warn,
                );
            }
        }
        vec![PanelEvent::NeedsRedraw]
    }

    /// Apply the judge's verdict: finish when the goal is reached, otherwise
    /// send the next work turn with what is still missing.
    fn on_goal_verdict(&mut self, done: bool, reason: &str) {
        let goal = match self.goal_task.as_mut() {
            Some(task) => {
                task.judging = false;
                task.goal.clone()
            }
            None => return,
        };
        if done {
            self.goal_task = None;
            let reason = reason.trim();
            let t = termide_i18n::t();
            let msg = if reason.is_empty() {
                t.agent_notice_goal_reached().to_string()
            } else {
                t.agent_notice_goal_reached_reason_fmt(reason)
            };
            self.notice(msg, NoticeKind::Info);
            return;
        }
        let prompt = goal_continuation(&goal, reason);
        let _ = self.send_goal_turn(prompt);
    }

    /// Start a `/handoff`: a read-only model call that distils the unfinished
    /// work into a brief; the verdict arrives as an `AgentEvent::Handoff`.
    fn start_handoff(&mut self) -> Vec<PanelEvent> {
        match self.runtime.handoff() {
            Ok(()) => self.notice(
                termide_i18n::t().agent_notice_handoff_preparing(),
                NoticeKind::Info,
            ),
            Err(PromptError::Busy) => {
                self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn)
            }
            Err(error) => self.notice(
                termide_i18n::t().agent_notice_cannot_handoff_fmt(&error.to_string()),
                NoticeKind::Warn,
            ),
        }
        vec![PanelEvent::NeedsRedraw]
    }

    /// Write the handoff brief to `HANDOFF.md` in the panel's working directory,
    /// where a fresh session or another agent (including an external one that
    /// reads files) can pick it up.
    fn save_handoff(&mut self, brief: &str) {
        let path = self.cwd.join("HANDOFF.md");
        match std::fs::write(&path, brief) {
            Ok(()) => {
                self.notice(
                    termide_i18n::t().agent_notice_handoff_written_fmt(&path.display().to_string()),
                    NoticeKind::Info,
                );
                self.pending_events
                    .push(PanelEvent::FileChangedOnDisk(path));
            }
            Err(error) => self.notice(
                termide_i18n::t().agent_notice_cannot_write_handoff_fmt(&error.to_string()),
                NoticeKind::Warn,
            ),
        }
    }

    /// Discard the current session and start a fresh one seeded with the
    /// handoff brief as its first request, so work continues from it.
    fn handoff_to_new_session(&mut self, brief: String) -> Vec<PanelEvent> {
        if let Some(old) = self.session.take() {
            discard(old);
        }
        self.switch_session(None);
        self.send(format!(
            "Continue the work described in this handoff brief:\n\n{brief}"
        ))
    }

    pub fn abort(&mut self) {
        // Stopping also ends any running loop or goal.
        self.loop_task = None;
        self.goal_task = None;
        // A stop already under way needs no second request or notice.
        if self.is_busy() && !self.stop_requested {
            self.runtime.abort();
            self.stop_requested = true;
            self.notice(termide_i18n::t().agent_notice_stopping(), NoticeKind::Warn);
        }
    }

    /// Record the runtime's queue lengths and drop the steering texts the
    /// agent has taken (it takes them oldest first).
    fn set_queued(&mut self, queued: (usize, usize)) {
        self.queued = queued;
        while self.queued_texts.len() > queued.0 {
            self.queued_texts.pop_front();
        }
    }

    /// Ask the running agent to pause at its next step boundary. Returns
    /// whether a run was there to pause.
    fn request_pause(&mut self) -> bool {
        if !self.is_busy() || !self.runtime.can_pause() {
            return false;
        }
        // The state strip shows the pending pause until the run reaches a
        // step boundary.
        self.runtime.pause();
        self.pause_requested = true;
        true
    }

    /// Withdraw a pause asked for that the run has not reached yet.
    fn cancel_pause(&mut self) {
        self.runtime.cancel_pause();
        self.pause_requested = false;
    }

    /// Resume the paused run. Its clock goes on from the request, and the
    /// pause's line keeps how long the pause lasted.
    fn resume(&mut self) {
        match self.runtime.resume() {
            Ok(()) => {
                self.busy = true;
                self.paused = false;
                self.resuming = true;
                self.end_pause();
            }
            Err(error) => self.notice(
                termide_i18n::t().agent_notice_cannot_continue_fmt(&error.to_string()),
                NoticeKind::Warn,
            ),
        }
    }

    /// Give up a paused run: nothing is running, so there is nothing to
    /// abort; the pause ends where it stood, and so do a loop or goal it was
    /// part of. The calls it left unrun are closed by the next request.
    fn stop_paused(&mut self) {
        self.paused = false;
        self.run_start = None;
        self.loop_task = None;
        self.goal_task = None;
        self.end_pause();
    }

    /// Freeze the pause's line at the pause's length, once it is over.
    fn end_pause(&mut self) {
        if let Some(start) = self.pause_start.take() {
            self.transcript.finish_pause(millis(start.elapsed()));
        }
    }

    /// The state strip above the input: what holds right now rather than what
    /// happened — a pause asked for but not reached yet, and the queued
    /// messages. Empty when there is nothing to show. A pause that took
    /// effect is the transcript's `‖` line, not a line here.
    fn state_lines(&self, width: u16) -> Vec<Line<'static>> {
        let pause = self
            .pause_requested
            .then(|| termide_i18n::t().agent_notice_will_pause());
        state_strip(
            self.queued_texts.iter().map(String::as_str),
            pause,
            width,
            &self.colors,
        )
    }

    /// Each skill as `/` reaches it: by its own name, or as `skill:<name>`
    /// when a built-in command, a template or a script takes the name.
    fn slash_skills(&self) -> Vec<(String, SkillInfo)> {
        let prompts = self.catalog.prompts();
        let commands = self.catalog.commands();
        self.catalog
            .skills()
            .into_iter()
            .map(|skill| {
                let name = skill.name.as_str();
                let taken = BUILTIN_COMMANDS.contains(&name)
                    || prompts.iter().any(|p| p.name == name)
                    || commands.iter().any(|c| c.name == name);
                let reach = if taken {
                    format!("{}{name}", slash::SKILL_PREFIX)
                } else {
                    name.to_string()
                };
                (reach, skill)
            })
            .collect()
    }

    /// The `/name`s more than one kind defines, see [`slash::conflicts`].
    fn slash_conflicts(&self) -> Vec<slash::Conflict> {
        slash::conflicts(
            &BUILTIN_COMMANDS,
            &self.catalog.prompts(),
            &self.catalog.commands(),
            &self.catalog.skills(),
        )
    }

    /// One warning per `/name` more than one kind defines.
    fn notice_slash_conflicts(&mut self) {
        for conflict in self.slash_conflicts() {
            self.notice(conflict.describe(), NoticeKind::Warn);
        }
    }

    fn notice(&mut self, text: impl Into<String>, kind: NoticeKind) {
        self.transcript.push(Item::Notice {
            text: text.into(),
            kind,
        });
    }

    /// Whether the session has no conversation yet — no request sent, so the
    /// welcome banner is still showing. A model/agent switch then updates the
    /// banner's values in place instead of pushing a notice that would replace
    /// the banner with a near-empty transcript.
    fn is_fresh(&self) -> bool {
        !self
            .transcript
            .items()
            .iter()
            .any(|item| matches!(item, Item::User { .. } | Item::Assistant { .. }))
    }

    /// The streaming block's live meta line: the same right-aligned zone a
    /// finished block shows, but with the block glyph, the ticking elapsed time
    /// (and a live token estimate while generating) and, in place of the status
    /// check, an animated spinner. Sits after the last block, animating on the
    /// panel's ~10 fps redraw while busy; `None` when idle.
    fn live_footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(activity) = self.activity.as_ref() else {
            return Vec::new();
        };
        let dim = Style::default().fg(self.colors.disabled);
        let mut lines: Vec<Line<'static>> = Vec::new();
        // While tokens stream (an answer or reasoning), a generation line in the
        // same shape as a finished block's `✍️` meta, with the live estimate.
        // Input tokens are only known once the turn ends, so the `⏫` prefill
        // line waits for the finished block. Only while tokens stream: once a
        // tool runs the message's first token is still known (the cost needs
        // it), but nothing is being generated.
        // An external agent sends its text in bursts and reports no tokens, so
        // there is no generation to time: none is shown for it.
        if let (Phase::Generating, Some(first_token), false) =
            (activity.phase, activity.first_token, self.external)
        {
            let gen_ms = first_token.elapsed().as_millis() as u32;
            let tokens = activity.est_tokens();
            lines.push(transcript::right_meta(
                width,
                vec![Span::styled(
                    format!(
                        "✍\u{fe0f} {} (↓{}, {})",
                        transcript::fmt_dur(gen_ms),
                        format_tokens(tokens),
                        transcript::fmt_speed(tokens, gen_ms)
                    ),
                    dim,
                )],
            ));
        }
        // The run clock: an animated glyph and the time since the request.
        // Its own glyph keeps it apart from a block's `🕒`, and when the run
        // ends it freezes on the answer (or on the run's closing line).
        let elapsed = self
            .run_start
            .map_or_else(|| activity.msg_start.elapsed(), |start| start.elapsed());
        let frames = transcript::RUN_FRAMES;
        let frame = (elapsed.as_millis() / 120) as usize % frames.len();
        lines.push(transcript::right_meta(
            width,
            vec![
                Span::styled(
                    format!("{} ", frames[frame]),
                    Style::default()
                        .fg(self.colors.info)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(transcript::fmt_dur(elapsed.as_millis() as u32), dim),
            ],
        ));
        lines
    }

    /// Name the conversation, so the panel title shows it instead of the
    /// first prompt. `false` when there is no session log to record it in.
    pub fn rename_session(&mut self, name: &str) -> bool {
        let Some(session) = &mut self.session else {
            return false;
        };
        match session.set_name(name) {
            Ok(_) => true,
            Err(error) => {
                log::warn!("cannot rename the agent conversation: {error}");
                false
            }
        }
    }

    /// Open the session the picker offered at `index`.
    fn resume_choice(&mut self, index: usize) -> bool {
        let Some(summary) = self.session_choices.get(index).cloned() else {
            return false;
        };
        self.session_choices.clear();
        if self.session.as_ref().map(Session::path) == Some(summary.path.as_path()) {
            return true; // already open
        }
        match Session::open_exclusive(&summary.path) {
            Ok(session) => {
                self.switch_session(Some(session));
            }
            Err(error) => {
                log::warn!("cannot open {}: {error}", summary.path.display());
                self.notice(
                    termide_i18n::t().agent_notice_cannot_open_session_fmt(&error.to_string()),
                    NoticeKind::Error,
                );
            }
        }
        true
    }

    /// The rules the agent runs under: the configured and "always" rules, and
    /// apart from them this session's answers, which count in modes the
    /// configured rules do not. Rebuilt into every agent the panel spawns so
    /// an answer survives a rebuild.
    fn effective_rules(&self) -> PermissionRules {
        let mut rules = self.rules.clone();
        rules.session = self.session_rules.tools.clone();
        rules
    }

    /// Offer the endpoint's models. The list is fetched off the UI thread
    /// and the picker opens from `tick()` when it arrives; an endpoint that
    /// cannot list models falls back to a typed id.
    fn request_model_list(&mut self) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return vec![PanelEvent::NeedsRedraw];
        }
        if self.model_fetch.is_some() {
            return vec![];
        }
        self.model_fetch = Some(spawn_model_list(Arc::clone(&self.provider)));
        vec![PanelEvent::SetStatusMessage {
            message: termide_i18n::t().agent_models_loading().to_string(),
            is_error: false,
        }]
    }

    fn model_picker(&mut self, result: Result<Vec<ModelInfo>, String>) -> PanelEvent {
        let t = termide_i18n::t();
        let mut models = match result {
            Ok(models) => models,
            Err(error) => {
                self.notice(
                    termide_i18n::t().agent_notice_model_list_unavailable_fmt(&error.to_string()),
                    NoticeKind::Info,
                );
                Vec::new()
            }
        };
        if models.is_empty() {
            return self.model_input();
        }
        if !models.iter().any(|m| m.id == self.model.id) {
            models.insert(
                0,
                ModelInfo {
                    id: self.model.id.clone(),
                    context_window: None,
                },
            );
        }
        let mut options: Vec<String> = models
            .iter()
            .map(|m| format!("{}{}", current_mark(m.id == self.model.id), m.id))
            .collect();
        options.push(format!("  {}", t.agent_model_other()));
        self.model_choices = models;
        PanelEvent::ShowSelect {
            title: t.agent_change_model().to_string(),
            options,
            on_select: SelectAction::Custom(MODEL_ACTION.to_string()),
        }
    }

    /// The model picker for an external (ACP) agent: the models it advertised,
    /// current marked, switched over ACP rather than through the built-in loop.
    fn acp_model_picker(&mut self) -> Vec<PanelEvent> {
        let t = termide_i18n::t();
        let models = self.runtime.available_models();
        if models.is_empty() {
            self.notice(
                termide_i18n::t().agent_notice_no_model_choices(),
                NoticeKind::Info,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        let current = self.runtime.current_model();
        let options = models
            .iter()
            .map(|m| {
                let mark = current_mark(current.as_deref() == Some(m.id.as_str()));
                if m.name == m.id {
                    format!("{mark}{}", m.id)
                } else {
                    format!("{mark}{} · {}", m.name, m.id)
                }
            })
            .collect();
        self.acp_models = models;
        vec![PanelEvent::ShowSelect {
            title: t.agent_change_model().to_string(),
            options,
            on_select: SelectAction::Custom(MODEL_ACTION.to_string()),
        }]
    }

    fn model_input(&self) -> PanelEvent {
        PanelEvent::ShowInput {
            prompt: termide_i18n::t().agent_model_prompt().to_string(),
            initial_value: self.model.id.clone(),
            on_submit: InputAction::Custom(MODEL_INPUT_ACTION.to_string()),
        }
    }

    fn mode_picker(&self) -> PanelEvent {
        let t = termide_i18n::t();
        let current = self.mode.get();
        let options = Mode::ALL
            .iter()
            .map(|mode| {
                let text = match mode {
                    Mode::Ask => t.agent_mode_ask(),
                    Mode::Plan => t.agent_mode_plan(),
                    Mode::Edit => t.agent_mode_edit(),
                    Mode::Configured => t.agent_mode_configured(),
                    Mode::All => t.agent_mode_all(),
                };
                format!("{}{text}", current_mark(*mode == current))
            })
            .collect();
        PanelEvent::ShowSelect {
            title: t.agent_change_mode().to_string(),
            options,
            on_select: SelectAction::Custom(MODE_ACTION.to_string()),
        }
    }

    /// Switch the permission mode. The handle is shared with the hooks, so
    /// a run in flight sees the new mode at its next tool call.
    fn set_mode(&mut self, mode: Mode) -> PanelEvent {
        let was_plan = self.mode.get() == Mode::Plan;
        self.mode.set(mode);
        self.rules.mode = mode;
        self.catalog.set_mode(mode);
        self.runtime.set_mode(mode);
        if was_plan != (mode == Mode::Plan) {
            self.sync_system_prompt();
        }
        PanelEvent::SetStatusMessage {
            message: format!(
                "{}: {}",
                termide_i18n::t().agent_change_mode(),
                mode.label()
            ),
            is_error: false,
        }
    }

    /// Offer the prompt templates; choosing one puts `/<name> ` into the
    /// input so arguments can follow.
    fn prompt_picker(&mut self) -> Vec<PanelEvent> {
        let t = termide_i18n::t();
        let prompts = self.catalog.prompts();
        if prompts.is_empty() {
            return vec![PanelEvent::SetStatusMessage {
                message: t.agent_no_prompts().to_string(),
                is_error: false,
            }];
        }
        let options = prompts
            .iter()
            .map(|p| {
                let mut line = format!("/{}", p.name);
                if !p.argument_hint.is_empty() {
                    line.push(' ');
                    line.push_str(&p.argument_hint);
                }
                if !p.description.is_empty() {
                    line.push_str(" · ");
                    line.push_str(&p.description);
                }
                line
            })
            .collect();
        self.prompt_choices = prompts;
        vec![PanelEvent::ShowSelect {
            title: t.agent_prompts().to_string(),
            options,
            on_select: SelectAction::Custom(PROMPTS_ACTION.to_string()),
        }]
    }

    fn agent_picker(&mut self) -> PanelEvent {
        let t = termide_i18n::t();
        let entries = self.catalog.list();
        let options = entries
            .iter()
            .map(|entry| {
                let mark = current_mark(entry.name == self.agent);
                if entry.description.is_empty() {
                    format!("{mark}{}", entry.name)
                } else {
                    format!("{mark}{} · {}", entry.name, entry.description)
                }
            })
            .collect();
        self.agent_choices = entries.into_iter().map(|entry| entry.name).collect();
        PanelEvent::ShowSelect {
            title: t.agent_change_agent().to_string(),
            options,
            on_select: SelectAction::Custom(AGENT_ACTION.to_string()),
        }
    }

    /// Continue the session as another agent: its prompt and tools, and its
    /// model and mode when the definition names them. Refused while a run
    /// is in flight.
    fn switch_agent(&mut self, name: &str) -> bool {
        if name == self.agent {
            return true;
        }
        let Some(profile) = self.catalog.resolve_without(name, &self.toolset_off) else {
            self.notice(
                termide_i18n::t().agent_notice_no_agent_fmt(name),
                NoticeKind::Warn,
            );
            return false;
        };
        // An external agent, or leaving one: the runtime is rebuilt on the
        // same session log, which is replayed into the transcript only.
        if profile.backend.is_some() || self.external {
            if self.is_busy() {
                self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
                return false;
            }
            if let Some(session) = &mut self.session {
                if let Err(error) = session.append_agent_change(name) {
                    log::warn!("agent session write failed: {error}");
                }
            }
            self.agent = name.to_string();
            self.system_prompt = profile.system_prompt;
            self.tools = profile.tools;
            self.late_tools = profile.late_tools;
            self.backend = profile.backend;
            if let Some(mode) = profile.mode {
                self.rules.mode = mode;
            }
            if let Some(id) = profile.model {
                self.model.id = id;
            }
            let session = self.session.take();
            self.switch_session(session);
            if !self.is_fresh() {
                self.notice(
                    termide_i18n::t().agent_notice_agent_fmt(name),
                    NoticeKind::Info,
                );
            }
            return true;
        }
        let model = match profile.model {
            Some(id) if id != self.model.id => ModelSpec {
                id,
                ..self.model.clone()
            },
            _ => self.model.clone(),
        };
        let mode_after = profile.mode.unwrap_or(self.mode.get());
        let prompt = if mode_after == Mode::Plan {
            self.plan_prompt.apply(&profile.system_prompt)
        } else {
            profile.system_prompt.clone()
        };
        let tools = profile.tools.clone();
        let worker_model = model.clone();
        if let Err(error) = self.runtime.update(Box::new(move |agent| {
            agent.set_system_prompt(prompt);
            *agent.tools_mut() = tools;
            agent.set_model(worker_model);
        })) {
            self.notice(
                termide_i18n::t().agent_notice_cannot_switch_agent_fmt(&error.to_string()),
                NoticeKind::Warn,
            );
            return false;
        }
        if model.id != self.model.id {
            if let Some(session) = &mut self.session {
                if let Err(error) = session.append_model_change(
                    self.provider_kind.as_str(),
                    &model.id,
                    Some(model.context_window),
                ) {
                    log::warn!("agent session write failed: {error}");
                }
            }
        }
        self.model = model;
        self.system_prompt = profile.system_prompt;
        self.tools = profile.tools;
        self.late_tools = profile.late_tools;
        self.waiting_tools.clear();
        // The new agent's prompt is built without what the session switched
        // off (the cache is lost anyway), and it offers its own lists.
        self.mcp_arrived.clear();
        self.offered_tools = profile.offered;
        self.offered_skills = profile.skills;
        self.context_off = self.toolset_off.clone();
        self.sync_blocked();
        if let Some(session) = &mut self.session {
            if let Err(error) = session.append_agent_change(name) {
                log::warn!("agent session write failed: {error}");
            }
        }
        if let Some(mode) = profile.mode {
            self.mode.set(mode);
            self.rules.mode = mode;
            self.catalog.set_mode(mode);
        }
        self.agent = name.to_string();
        if !self.is_fresh() {
            self.notice(
                termide_i18n::t().agent_notice_agent_fmt(name),
                NoticeKind::Info,
            );
        }
        true
    }

    /// The session's token totals: `↑` the prompt tokens billed in full
    /// (uncached input and cache writes), `↻` those the cache served (when
    /// any), `↓` the output.
    fn token_totals(&self) -> String {
        let cached = if self.session_cached > 0 {
            format!(" ↻{}", format_tokens(self.session_cached))
        } else {
            String::new()
        };
        format!(
            "↑{}{cached} ↓{}",
            format_tokens(self.session_input),
            format_tokens(self.session_output)
        )
    }

    /// The model as the banner and the chip show it: `auto` while it is left
    /// to the provider and not known yet.
    fn model_display(&self) -> String {
        if self.model.id.is_empty() {
            "auto".to_string()
        } else {
            self.model.id.clone()
        }
    }

    /// The connection as the banner and the chip show it: its name beside
    /// the protocol.
    fn connection_display(&self) -> String {
        let kind = provider_label(&self.provider_kind);
        if self.connection.is_empty() {
            kind.to_string()
        } else {
            format!("{} · {kind}", self.connection)
        }
    }

    /// Run the session on connection `name`: its endpoint and its
    /// model, the rest of the session kept. The agent restarts on the same
    /// log, so a built-in loop carries the conversation over; a CLI agent
    /// does not, so switching to or from one is refused once it has begun.
    fn switch_connection(&mut self, name: &str) -> bool {
        if name == self.connection {
            return true;
        }
        let t = termide_i18n::t();
        let Some(connections) = self.connections.clone() else {
            return false;
        };
        if self.is_busy() {
            self.notice(t.agent_notice_busy(), NoticeKind::Warn);
            return false;
        }
        let Some(choice) = connections.build(name, &self.agent) else {
            self.notice(t.agent_notice_no_connection_fmt(name), NoticeKind::Warn);
            return false;
        };
        if (choice.backend.is_some() || self.external) && !self.is_fresh() {
            self.notice(t.agent_notice_connection_before_first(), NoticeKind::Warn);
            return false;
        }
        connections.activate(&choice);
        self.provider = Arc::clone(&choice.provider);
        self.provider_kind = choice.kind.clone();
        self.connection = choice.name.clone();
        // The connection's model replaces the one in use: another endpoint seldom
        // serves the same id.
        let model = ModelSpec {
            id: choice.model.clone(),
            context_window: choice.context_window,
            ..self.model.clone()
        };
        self.configured_model = model.clone();
        self.model = model;
        self.model_choices.clear();
        self.provider_backend = choice.backend.clone();
        self.backend = self.provider_backend.clone().or_else(|| {
            self.catalog
                .resolve(&self.agent)
                .and_then(|profile| profile.backend)
        });
        if let Some(session) = &mut self.session {
            let written = session.append_connection_change(name).and_then(|_| {
                session.append_model_change(
                    &choice.kind,
                    &self.model.id,
                    Some(self.model.context_window),
                )
            });
            if let Err(error) = written {
                log::warn!("agent session write failed: {error}");
            }
        }
        let fresh = self.is_fresh();
        let session = self.session.take();
        self.switch_session(session);
        // The silent window probe asks the new endpoint.
        self.context_probe = (!self.external).then(|| spawn_model_list(Arc::clone(&self.provider)));
        if !fresh {
            self.notice(t.agent_notice_connection_fmt(name), NoticeKind::Info);
        }
        true
    }

    /// Continue the session on another model of the same endpoint. The
    /// context window follows the endpoint's figure when it gave one and
    /// stays as configured otherwise; the token limit is always the
    /// configured one. Refused while a run is in flight.
    fn switch_model(&mut self, id: &str, context_window: Option<u64>) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let new_window = context_window.unwrap_or(self.model.context_window);
        let id_changed = id != self.model.id;
        // Re-selecting the same model still adopts a newly-known context window
        // (a provider's `max_model_len`); nothing to do only when both match.
        if !id_changed && new_window == self.model.context_window {
            return true;
        }
        let model = ModelSpec {
            id: id.to_string(),
            context_window: new_window,
            ..self.model.clone()
        };
        let worker_model = model.clone();
        if let Err(error) = self
            .runtime
            .update(Box::new(move |agent| agent.set_model(worker_model)))
        {
            self.notice(
                termide_i18n::t().agent_notice_cannot_switch_model_fmt(&error.to_string()),
                NoticeKind::Warn,
            );
            return false;
        }
        self.model = model;
        if let Some(session) = &mut self.session {
            if let Err(error) = session.append_model_change(
                self.provider_kind.as_str(),
                id,
                Some(self.model.context_window),
            ) {
                log::warn!("agent session write failed: {error}");
            }
        }
        // A silent window adoption (same id) leaves no notice; nor does a fresh
        // session, where the banner shows the new model instead.
        if id_changed && !self.is_fresh() {
            self.notice(
                termide_i18n::t().agent_notice_model_fmt(id),
                NoticeKind::Info,
            );
        }
        // Another model has no cache of this prompt: what is refused can
        // leave the context for free.
        if id_changed && self.toolset_off != self.context_off {
            self.refresh_context();
        }
        true
    }

    /// Adopt what a `list_models` result says: with the model left to the
    /// provider, its first model — for this session and the next ones the
    /// panel starts; otherwise the active model's real context window, when
    /// it is known and differs. Returns whether anything changed.
    fn adopt_listed_models(&mut self, models: &[ModelInfo]) -> bool {
        if self.is_busy() {
            return false;
        }
        if self.model.id.is_empty() {
            let Some(first) = models.first() else {
                return false;
            };
            let adopted = self.switch_model(&first.id, first.context_window);
            if adopted && self.configured_model.id.is_empty() {
                self.configured_model.id = self.model.id.clone();
                self.configured_model.context_window = self.model.context_window;
            }
            return adopted;
        }
        let Some(window) = models
            .iter()
            .find(|m| m.id == self.model.id)
            .and_then(|m| m.context_window)
        else {
            return false;
        };
        if window == self.model.context_window {
            return false;
        }
        let id = self.model.id.clone();
        self.switch_model(&id, Some(window))
    }

    /// Toggle whether the model is asked to reason (extended thinking /
    /// `reasoning_effort`). Applies to the next request and is remembered in
    /// the session log so a resume comes back with the same choice.
    fn toggle_reasoning(&mut self) -> bool {
        if self.external {
            return false;
        }
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return false;
        }
        let reasoning = !self.model.reasoning;
        let mut model = self.model.clone();
        model.reasoning = reasoning;
        let worker_model = model.clone();
        if let Err(error) = self
            .runtime
            .update(Box::new(move |agent| agent.set_model(worker_model)))
        {
            self.notice(
                termide_i18n::t().agent_notice_cannot_change_reasoning_fmt(&error.to_string()),
                NoticeKind::Warn,
            );
            return false;
        }
        self.model = model;
        if let Some(session) = &mut self.session {
            if let Err(error) = session.append_reasoning_change(reasoning) {
                log::warn!("agent session write failed: {error}");
            }
        }
        let t = termide_i18n::t();
        self.notice(
            if reasoning {
                t.agent_notice_reasoning_on()
            } else {
                t.agent_notice_reasoning_off()
            },
            NoticeKind::Info,
        );
        true
    }

    /// Earlier requests of this session, oldest first, repeats collapsed.
    fn history(&self) -> Vec<String> {
        let mut history: Vec<String> = Vec::new();
        for item in self.transcript.items() {
            if let Item::User { text, command, .. } = item {
                let typed = command.as_ref().unwrap_or(text);
                if history.last() != Some(typed) {
                    history.push(typed.clone());
                }
            }
        }
        history
    }

    /// Show an earlier (`older`) or later request in the input, the way a
    /// shell recalls its history; past the newest, the draft comes back.
    /// Take the messages still waiting in the queue back into the input, to
    /// edit them before they go: ahead of what is typed, as they were sent
    /// first. Returns whether there were any, so `↑` walks history only once
    /// the queue is empty.
    fn unqueue(&mut self) -> bool {
        if self.history_pos.is_some() || self.queued_texts.is_empty() {
            return false;
        }
        let taken = self.runtime.take_queued();
        self.set_queued(self.runtime.queue_lens());
        self.queued_texts.clear();
        let Some(queued) = UserMessage::merge(taken) else {
            return false;
        };
        let typed = self.input_area().text();
        let text = if typed.trim().is_empty() {
            queued.typed()
        } else {
            format!("{}\n\n{typed}", queued.typed())
        };
        self.set_input(&text);
        true
    }

    fn recall(&mut self, older: bool) -> bool {
        let history = self.history();
        let next = match (self.history_pos, older) {
            (None, true) if !history.is_empty() => {
                self.draft = self.input_area().text();
                Some(history.len() - 1)
            }
            (None, _) => return false,
            (Some(pos), true) => Some(pos.saturating_sub(1)),
            (Some(pos), false) if pos + 1 < history.len() => Some(pos + 1),
            (Some(_), false) => None,
        };
        self.history_pos = next;
        let text = match next {
            Some(pos) => history[pos].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.set_input(&text);
        true
    }

    /// Replace the input with `text`, cursor at its end.
    fn set_input(&mut self, text: &str) {
        self.input.set_field_text(0, text);
        let area = self.input_area_mut();
        while area.move_down() {}
        area.move_end();
    }

    /// Recompute the `/command` popup after the input changed: it shows
    /// while the input is a single `/word` with no space yet.
    /// The plain text of the block under the chat cursor, for `Copy`, trimmed of
    /// the stray leading/trailing blank lines models and tools produce.
    fn selected_block_text(&self) -> Option<String> {
        let text = match self.transcript.items().get(self.selected)? {
            Item::User { text, .. }
            | Item::Assistant { text, .. }
            | Item::Thinking { text, .. }
            | Item::System { text } => text.clone(),
            Item::Notice { text, .. } => text.clone(),
            Item::RunEnd { elapsed_ms, at, .. } => transcript::run_end_text(*elapsed_ms, at),
            Item::Tool {
                result, live, call, ..
            } => result
                .as_ref()
                .map(ToolResultMessage::plain_text)
                .or_else(|| live.clone())
                .unwrap_or_else(|| call.name.clone()),
        };
        Some(text.trim().to_string())
    }

    /// Open the selected block's full output as a read-only panel, for a
    /// bigger view than the inline preview. A tool with a saved raw log opens
    /// that file; anything else is written to a temporary file first. Focus
    /// stays in the chat.
    fn open_selected_in_panel(&mut self) -> Vec<PanelEvent> {
        let Some(item) = self.transcript.items().get(self.selected) else {
            return vec![];
        };
        let (content, name) = match item {
            Item::Tool {
                call, result, live, ..
            } => {
                if let Some(path) = result.as_ref().and_then(full_log_path) {
                    if path.exists() {
                        return vec![PanelEvent::ViewFile(path)];
                    }
                }
                let body = result
                    .as_ref()
                    .map(ToolResultMessage::plain_text)
                    .or_else(|| live.clone())
                    .unwrap_or_default();
                (body, format!("{}-output.txt", call.name))
            }
            Item::Assistant { text, .. } => (text.clone(), "agent-answer.md".to_string()),
            Item::Thinking { text, .. } => (text.clone(), "agent-thinking.md".to_string()),
            Item::System { text } => (text.clone(), "system-prompt.md".to_string()),
            Item::User { text, .. } => (text.clone(), "message.txt".to_string()),
            Item::Notice { .. } | Item::RunEnd { .. } => return vec![PanelEvent::NeedsRedraw],
        };
        if content.trim().is_empty() {
            self.notice(
                termide_i18n::t().agent_notice_nothing_to_open(),
                NoticeKind::Info,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let path = std::env::temp_dir().join(format!("termide-agent-{}-{safe}", now_millis()));
        match std::fs::write(&path, content) {
            Ok(()) => vec![PanelEvent::ViewFile(path), PanelEvent::NeedsRedraw],
            Err(error) => {
                self.notice(
                    termide_i18n::t().agent_notice_cannot_open_block_fmt(&error.to_string()),
                    NoticeKind::Error,
                );
                vec![PanelEvent::NeedsRedraw]
            }
        }
    }

    /// The `@`-file mention under the cursor: the span from the `@` to the
    /// cursor and the text typed after it. `@` counts only at the start of a
    /// word (line start or after whitespace), and the mention ends at the
    /// first space, so it is one path.
    fn mention_at_cursor(&self) -> Option<(MentionSpan, String)> {
        let cursor = self.input_area().cursor();
        let chars: Vec<char> = self.input_area().lines().get(cursor.row)?.chars().collect();
        if cursor.col > chars.len() {
            return None;
        }
        let mut i = cursor.col;
        while i > 0 {
            let c = chars[i - 1];
            if c == '@' {
                let starts_word = i == 1 || chars[i - 2].is_whitespace();
                if !starts_word {
                    return None;
                }
                let prefix: String = chars[i..cursor.col].iter().collect();
                return Some((
                    MentionSpan {
                        row: cursor.row,
                        start: i - 1,
                        end: cursor.col,
                    },
                    prefix,
                ));
            }
            if c.is_whitespace() {
                return None;
            }
            i -= 1;
        }
        None
    }

    fn refresh_completion(&mut self) {
        self.completion_span = None;
        let text = self.input_area().text();
        let word = text.strip_prefix('/').filter(|rest| {
            self.input_area().line_count() <= 1 && !rest.contains(char::is_whitespace)
        });
        let Some(prefix) = word else {
            return self.refresh_file_completion();
        };
        let mut items: Vec<CompletionItem> = self
            .catalog
            .prompts()
            .into_iter()
            .filter(|template| template.name.starts_with(prefix))
            .map(|template| {
                CompletionItem::new(template.name.clone())
                    .with_label(format!("/{}", template.name))
                    .with_hint(template.argument_hint)
                    .with_description(template.description)
            })
            .collect();
        let taken: Vec<String> = items.iter().map(|i| i.value.clone()).collect();
        let scripts: Vec<CommandScript> = self
            .catalog
            .commands()
            .into_iter()
            .filter(|c| c.name.starts_with(prefix) && !taken.contains(&c.name))
            .collect();
        for script in scripts {
            let description = if script.trusted {
                script.description
            } else if script.description.is_empty() {
                termide_i18n::t().agent_project_command().to_string()
            } else {
                termide_i18n::t().agent_project_command_fmt(&script.description)
            };
            items.push(
                CompletionItem::new(script.name.clone())
                    .with_label(format!("/{}", script.name))
                    .with_hint(script.argument_hint)
                    .with_description(description),
            );
        }
        // A skill whose name something else takes is offered as `skill:<name>`,
        // found by either spelling.
        for (reach, skill) in self.slash_skills() {
            if reach.starts_with(prefix) || skill.name.starts_with(prefix) {
                items.push(
                    CompletionItem::new(reach.clone())
                        .with_label(format!("/{reach}"))
                        .with_hint(skill.argument_hint)
                        .with_description(skill.description),
                );
            }
        }
        if UNDO_COMMAND.starts_with(prefix) && !self.external {
            items.push(
                CompletionItem::new(UNDO_COMMAND)
                    .with_label(format!("/{UNDO_COMMAND}"))
                    .with_description(termide_i18n::t().agent_cmd_desc_undo()),
            );
        }
        if COMPACT_COMMAND.starts_with(prefix) && !self.external {
            items.push(
                CompletionItem::new(COMPACT_COMMAND)
                    .with_label(format!("/{COMPACT_COMMAND}"))
                    .with_hint("[focus]")
                    .with_description(termide_i18n::t().agent_cmd_desc_compact()),
            );
        }
        if self.session_dir.is_some() {
            if NEW_COMMAND.starts_with(prefix) {
                items.push(
                    CompletionItem::new(NEW_COMMAND)
                        .with_label(format!("/{NEW_COMMAND}"))
                        .with_description(termide_i18n::t().agent_cmd_desc_new()),
                );
            }
            if CLEAR_COMMAND.starts_with(prefix) {
                items.push(
                    CompletionItem::new(CLEAR_COMMAND)
                        .with_label(format!("/{CLEAR_COMMAND}"))
                        .with_description(termide_i18n::t().agent_cmd_desc_clear()),
                );
            }
            if RENAME_COMMAND.starts_with(prefix) {
                items.push(
                    CompletionItem::new(RENAME_COMMAND)
                        .with_label(format!("/{RENAME_COMMAND}"))
                        .with_hint("[name]")
                        .with_description(termide_i18n::t().agent_cmd_desc_rename()),
                );
            }
            if NAME_COMMAND.starts_with(prefix) {
                items.push(
                    CompletionItem::new(NAME_COMMAND)
                        .with_label(format!("/{NAME_COMMAND}"))
                        .with_hint("[name]")
                        .with_description(termide_i18n::t().agent_cmd_desc_rename()),
                );
            }
        }
        // Run control is offered only when it applies.
        if self.is_busy() && self.runtime.can_pause() && PAUSE_COMMAND.starts_with(prefix) {
            items.push(
                CompletionItem::new(PAUSE_COMMAND)
                    .with_label(format!("/{PAUSE_COMMAND}"))
                    .with_description(termide_i18n::t().agent_cmd_desc_pause()),
            );
        }
        if (self.paused || self.pause_requested) && CONTINUE_COMMAND.starts_with(prefix) {
            items.push(
                CompletionItem::new(CONTINUE_COMMAND)
                    .with_label(format!("/{CONTINUE_COMMAND}"))
                    .with_description(termide_i18n::t().agent_cmd_desc_continue()),
            );
        }
        if LOOP_COMMAND.starts_with(prefix) {
            items.push(
                CompletionItem::new(LOOP_COMMAND)
                    .with_label(format!("/{LOOP_COMMAND}"))
                    .with_hint(termide_i18n::t().agent_hint_loop())
                    .with_description(termide_i18n::t().agent_cmd_desc_loop()),
            );
        }
        if GOAL_COMMAND.starts_with(prefix) {
            items.push(
                CompletionItem::new(GOAL_COMMAND)
                    .with_label(format!("/{GOAL_COMMAND}"))
                    .with_hint(termide_i18n::t().agent_hint_goal())
                    .with_description(termide_i18n::t().agent_cmd_desc_goal()),
            );
        }
        if HANDOFF_COMMAND.starts_with(prefix) && !self.external {
            items.push(
                CompletionItem::new(HANDOFF_COMMAND)
                    .with_label(format!("/{HANDOFF_COMMAND}"))
                    .with_description(termide_i18n::t().agent_cmd_desc_handoff()),
            );
        }
        if USAGE_COMMAND.starts_with(prefix) {
            items.push(
                CompletionItem::new(USAGE_COMMAND)
                    .with_label(format!("/{USAGE_COMMAND}"))
                    .with_description(termide_i18n::t().agent_cmd_desc_usage()),
            );
        }
        if PROMPT_COMMAND.starts_with(prefix) {
            items.push(
                CompletionItem::new(PROMPT_COMMAND)
                    .with_label(format!("/{PROMPT_COMMAND}"))
                    .with_description(termide_i18n::t().agent_cmd_desc_prompt()),
            );
        }
        if items.is_empty() {
            self.completion = None;
            return;
        }
        // Prompts, command scripts and built-ins are gathered in different
        // groups; show the whole `/` list in one alphabetical order.
        items.sort_by(|a, b| a.value.cmp(&b.value));
        match &mut self.completion {
            Some(list) => list.set_items(items),
            None => self.completion = Some(CompletionList::new(items)),
        }
    }

    /// The `@`-file popup: files and directories under the panel's directory
    /// matching the text after `@`, so a path is a few keystrokes and a
    /// selection. A directory ends with `/` and reopens the popup for its
    /// contents; a file inserts the path and a space. Reuses the same
    /// completion widget as `/`.
    fn refresh_file_completion(&mut self) {
        let Some((span, prefix)) = self.mention_at_cursor() else {
            self.completion = None;
            return;
        };
        let items = file_completions(&self.cwd, &prefix);
        if items.is_empty() {
            self.completion = None;
            return;
        }
        self.completion_span = Some(span);
        match &mut self.completion {
            Some(list) => list.set_items(items),
            None => self.completion = Some(CompletionList::new(items)),
        }
    }

    /// Put the highlighted completion into the input. A `/`-command replaces
    /// the whole input; an `@`-file mention replaces just its span.
    fn accept_completion(&mut self) -> bool {
        let Some(list) = self.completion.take() else {
            return false;
        };
        let Some(item) = list.selected_item().cloned() else {
            return false;
        };
        match self.completion_span.take() {
            None => {
                let text = format!("/{} ", item.value);
                self.set_input(&text);
            }
            Some(span) => {
                let is_dir = item.value.ends_with('/');
                // Delete the `@`+prefix typed so far.
                self.input_area_mut().set_cursor(span.row, span.end);
                for _ in span.start..span.end {
                    self.input_area_mut().backspace();
                }
                if is_dir {
                    // Keep the `@` so the popup reopens for the directory's
                    // contents and the user can drill in.
                    self.input_area_mut().insert('@');
                    self.input_area_mut().insert_str(&item.value);
                    self.refresh_completion();
                } else {
                    // A chosen file becomes a plain path the agent can read.
                    self.input_area_mut().insert_str(&item.value);
                    self.input_area_mut().insert(' ');
                }
            }
        }
        true
    }

    /// Offer to undo the last request: its files go back and the
    /// conversation is rewound to before it.
    fn ask_undo(&mut self) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return vec![PanelEvent::NeedsRedraw];
        }
        let files = self
            .checkpoints
            .as_ref()
            .map(|store| store.lock().unwrap().last_files())
            .unwrap_or_default();
        if files.is_empty() {
            self.notice(
                termide_i18n::t().agent_notice_nothing_to_undo(),
                NoticeKind::Info,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        let names: Vec<String> = files
            .iter()
            .map(|path| {
                path.strip_prefix(&self.cwd)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect();
        let t = termide_i18n::t();
        let changed = if names.len() == 1 {
            names[0].clone()
        } else {
            t.agent_undo_changed_files_fmt(names.len(), &names.join(", "))
        };
        let form = ChoiceForm::new(
            t.agent_undo_confirm_fmt(&changed),
            vec![t.agent_undo_restore().to_string()],
        )
        .with_cancel(t.agent_undo_keep());
        self.pending = Some(Pending::Undo { form });
        vec![PanelEvent::NeedsRedraw]
    }

    /// Offer to delete the current session (F8, or the panel's `[≡]` menu):
    /// a confirmation modal, since it removes the log for good. The accepted
    /// answer comes back as `PanelCommand::Confirmed(DELETE_SESSION_ACTION)`.
    fn ask_delete_session(&mut self) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return vec![PanelEvent::NeedsRedraw];
        }
        let t = termide_i18n::t();
        let name = self
            .session
            .as_ref()
            .and_then(Session::name)
            .map(str::to_string);
        let id = self
            .session
            .as_ref()
            .map(Session::path)
            .and_then(Path::file_stem)
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let label = name
            .clone()
            .unwrap_or_else(|| t.agent_delete_this_session().to_string());
        let confirm = t.agent_delete_confirm_fmt(&label);
        // The log's id under the question, after the name when it has one.
        let message = match (name, id.is_empty()) {
            (_, true) => confirm,
            (Some(name), false) => format!("{confirm}\n{name} · {id}"),
            (None, false) => format!("{confirm}\n{id}"),
        };
        vec![PanelEvent::ShowConfirm {
            message,
            on_confirm: ConfirmAction::Custom(DELETE_SESSION_ACTION.to_string()),
        }]
    }

    /// Discard the current session and open a fresh one in its place — the
    /// confirmed F8 delete, the same effect as `/clear`.
    fn perform_delete_session(&mut self) -> Vec<PanelEvent> {
        if let Some(old) = self.session.take() {
            discard(old);
        }
        self.switch_session(None);
        vec![PanelEvent::NeedsRedraw]
    }

    /// A read-only summary of the current session, shown in an info modal
    /// (F3, the `[≡]` menu's "Session info", or `/usage`).
    fn session_summary(&self) -> Vec<PanelEvent> {
        let t = termide_i18n::t();
        let name = self
            .session
            .as_ref()
            .and_then(Session::name)
            .map(str::to_string)
            .unwrap_or_else(|| t.ai_session_untitled().to_string());
        let messages = self
            .transcript
            .items()
            .iter()
            .filter(|item| matches!(item, Item::User { .. } | Item::Assistant { .. }))
            .count();
        let mut rows: Vec<(String, String)> = vec![(t.agent_info_session().into(), name)];
        if let Some(session) = self.session.as_ref() {
            if let Some(id) = session
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            {
                rows.push((t.agent_info_log().into(), id));
            }
        }
        rows.push((t.agent_info_agent().into(), self.agent.clone()));
        rows.push((t.agent_info_provider().into(), self.provider_kind.clone()));
        rows.push((t.agent_info_model().into(), self.model.id.clone()));
        rows.push((
            t.agent_info_mode().into(),
            self.mode.get().label().to_string(),
        ));
        rows.push((
            t.agent_info_directory().into(),
            shorten_path(&self.cwd, usize::MAX),
        ));
        if let Some(session) = self.session.as_ref() {
            rows.push((
                t.agent_info_created().into(),
                civil_date(session.header().created),
            ));
            if let Some(last) = session.entries().last().map(|e| e.timestamp) {
                rows.push((t.agent_info_last_active().into(), civil_date(last)));
            }
            let compactions = session
                .entries()
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::Compaction { .. }))
                .count();
            rows.push((t.agent_info_compactions().into(), compactions.to_string()));
        }
        rows.push((t.agent_info_messages().into(), messages.to_string()));
        rows.push((t.agent_info_tokens().into(), self.token_totals()));
        rows.push((
            t.agent_info_context().into(),
            format!(
                "{} / {}",
                format_tokens(self.context_tokens),
                format_tokens(self.model.context_window)
            ),
        ));
        // How much the clean mechanism has shrunk shell output this session,
        // when any ran: raw → cleaned and the percentage saved.
        if self.clean_raw_bytes > 0 {
            let saved = self.clean_raw_bytes.saturating_sub(self.clean_out_bytes);
            let percent = saved * 100 / self.clean_raw_bytes;
            rows.push((
                t.agent_info_output_cleaned().into(),
                format!(
                    "{} → {} (−{percent}%)",
                    format_bytes(self.clean_raw_bytes),
                    format_bytes(self.clean_out_bytes),
                ),
            ));
        }
        vec![PanelEvent::ShowInfo {
            title: t.agent_session_info().to_string(),
            rows,
        }]
    }

    /// Offer the undoable checkpoints (F4), newest first, to roll the session
    /// back to before a chosen change.
    fn ask_rollback(&mut self) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return vec![PanelEvent::NeedsRedraw];
        }
        let Some(store) = self.checkpoints.clone() else {
            self.notice(
                termide_i18n::t().agent_notice_nothing_to_rollback(),
                NoticeKind::Info,
            );
            return vec![PanelEvent::NeedsRedraw];
        };
        let checkpoints = store.lock().unwrap().checkpoints();
        if checkpoints.is_empty() {
            self.notice(
                termide_i18n::t().agent_notice_nothing_to_rollback(),
                NoticeKind::Info,
            );
            return vec![PanelEvent::NeedsRedraw];
        }
        let options = checkpoints
            .iter()
            .enumerate()
            .map(|(i, files)| {
                let names: Vec<String> = files
                    .iter()
                    .map(|p| p.strip_prefix(&self.cwd).unwrap_or(p).display().to_string())
                    .collect();
                let t = termide_i18n::t();
                let changed = if names.len() == 1 {
                    names[0].clone()
                } else {
                    t.agent_rollback_files_fmt(names.len(), &names.join(", "))
                };
                let step = if i == 0 {
                    t.agent_rollback_last_request().to_string()
                } else {
                    t.agent_rollback_steps_fmt(i + 1)
                };
                truncate_title(&format!("{step} — {changed}"))
            })
            .collect();
        vec![PanelEvent::ShowSelect {
            title: termide_i18n::t().agent_rollback_title().to_string(),
            options,
            on_select: SelectAction::Custom(ROLLBACK_ACTION.to_string()),
        }]
    }

    /// Undo every request from the newest down to the one the user picked
    /// (`steps_from_newest` = 0 is the last request), putting the files back and
    /// rewinding the conversation to before the oldest of them.
    fn perform_rollback(&mut self, steps_from_newest: usize) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice(termide_i18n::t().agent_notice_busy(), NoticeKind::Warn);
            return vec![PanelEvent::NeedsRedraw];
        }
        let Some(store) = self.checkpoints.clone() else {
            return vec![PanelEvent::NeedsRedraw];
        };
        let mut events = Vec::new();
        let mut restored = 0usize;
        let mut leaf = None;
        {
            let mut store = store.lock().unwrap();
            for _ in 0..=steps_from_newest {
                match store.undo_last() {
                    Ok(undone) => {
                        for path in &undone.files {
                            events.push(PanelEvent::FileChangedOnDisk(path.clone()));
                        }
                        restored += undone.files.len();
                        leaf = undone.leaf_before;
                    }
                    Err(_) => break,
                }
            }
        }
        if let Some(session) = &mut self.session {
            if let Err(error) = session.rewind_to(leaf.as_deref()) {
                log::warn!("agent session rewind failed: {error}");
            }
        }
        let session = self.session.take();
        self.switch_session(session);
        let t = termide_i18n::t();
        self.notice(
            t.agent_notice_rolled_back_fmt(restored, t.pluralize(restored, "file")),
            NoticeKind::Info,
        );
        events.push(PanelEvent::NeedsRedraw);
        events
    }

    /// Put the last request's files back, rewind the session to before it
    /// and rebuild the agent from there.
    fn perform_undo(&mut self) -> Vec<PanelEvent> {
        let Some(store) = self.checkpoints.clone() else {
            return vec![];
        };
        let undone = store.lock().unwrap().undo_last();
        let undone = match undone {
            Ok(undone) => undone,
            Err(error) => {
                self.notice(
                    termide_i18n::t().agent_notice_cannot_undo_fmt(&error.to_string()),
                    NoticeKind::Error,
                );
                return vec![PanelEvent::NeedsRedraw];
            }
        };
        let mut events: Vec<PanelEvent> = undone
            .files
            .iter()
            .map(|path| PanelEvent::FileChangedOnDisk(path.clone()))
            .collect();
        if let Some(session) = &mut self.session {
            if let Err(error) = session.rewind_to(undone.leaf_before.as_deref()) {
                log::warn!("agent session rewind failed: {error}");
            }
        }
        let count = undone.files.len();
        let session = self.session.take();
        self.switch_session(session);
        let t = termide_i18n::t();
        self.notice(
            t.agent_notice_undid_fmt(count, t.pluralize(count, "file")),
            NoticeKind::Info,
        );
        events.push(PanelEvent::NeedsRedraw);
        events
    }

    /// `/name args` names a command script: run it, or ask first when it
    /// came with the project and no rule or session grant covers it.
    fn run_command(&mut self, script: CommandScript, args: String) {
        let verdict = self.rules.evaluate("command", &script.name);
        if verdict == Some(Decision::Deny) {
            self.notice(
                termide_i18n::t().agent_notice_command_denied_fmt(&script.name),
                NoticeKind::Warn,
            );
            return;
        }
        let allowed = script.trusted
            || verdict == Some(Decision::Allow)
            || self.allowed_commands.contains(&script.name);
        if allowed {
            self.start_command(script, args);
            return;
        }
        let t = termide_i18n::t();
        let title = t.agent_command_run_title_fmt(&script.name, &script.path.display().to_string());
        let form = ChoiceForm::new(
            title.clone(),
            vec![
                t.agent_cmd_run_once().to_string(),
                t.agent_cmd_run_session().to_string(),
                t.agent_cmd_run_always().to_string(),
                t.agent_cmd_dont_run().to_string(),
            ],
        );
        self.pending_events.push(PanelEvent::SetStatusMessage {
            message: title,
            is_error: false,
        });
        self.pending = Some(Pending::Command { script, args, form });
    }

    /// Run the script on a thread; `tick` sends its output as the request.
    fn start_command(&mut self, script: CommandScript, args: String) {
        if self.command_run.is_some() {
            self.notice(
                termide_i18n::t().agent_notice_command_running(),
                NoticeKind::Warn,
            );
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cwd = self.cwd.clone();
        let name = script.name.clone();
        // The request is headed by the command as typed.
        let command = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        std::thread::spawn(move || {
            let outcome = script.run(&args, &cwd);
            let _ = tx.send((command, outcome));
        });
        self.command_run = Some(rx);
        self.pending_events.push(PanelEvent::SetStatusMessage {
            message: termide_i18n::t().agent_running_command_fmt(&name),
            is_error: false,
        });
    }

    /// Take in a finished command script: its output goes out as a request.
    fn poll_command(&mut self) -> bool {
        let outcome = self.command_run.as_ref().map(Receiver::try_recv);
        match outcome {
            Some(Ok((command, Ok(text)))) => {
                self.command_run = None;
                self.send_as(text, Some(command));
                true
            }
            Some(Ok((_, Err(error)))) => {
                self.command_run = None;
                self.notice(error, NoticeKind::Error);
                true
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.command_run = None;
                self.notice(
                    termide_i18n::t().agent_notice_command_dropped(),
                    NoticeKind::Error,
                );
                true
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => false,
        }
    }

    /// The input changed by typing: history browsing ends, the popup follows.
    fn after_edit(&mut self) {
        self.history_pos = None;
        self.refresh_completion();
    }

    fn viewport_height(&self) -> usize {
        self.transcript_area.height as usize
    }

    fn max_top(&self) -> usize {
        self.transcript
            .line_count()
            .saturating_sub(self.viewport_height())
    }

    fn scroll_by(&mut self, delta: i32) {
        let max_top = self.max_top();
        let next = if delta < 0 {
            self.top.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.top.saturating_add(delta as usize)
        };
        self.top = next.min(max_top);
        self.follow = self.top >= max_top;
    }

    /// Bring the selected block into view after the selection moves. Scrolling
    /// is otherwise free, so this runs only from block navigation, not on every
    /// frame — a block scrolled off screen stays off until the selection moves.
    /// Uses the geometry of the last render (viewport height, flat-line layout).
    fn scroll_selected_into_view(&mut self) {
        let height = self.viewport_height();
        if height == 0 {
            return;
        }
        let Some(first) = self.transcript.first_line_of(self.selected) else {
            return;
        };
        let mut last = first;
        while self.transcript.item_at_line(last + 1) == Some(self.selected) {
            last += 1;
        }
        if first < self.top {
            // The block starts above the viewport: show it from its start.
            self.top = first;
        } else if last >= self.top + height {
            // It ends below the viewport: reveal its end, or, when it is taller
            // than the viewport, its start so it reads from the top.
            self.top = if last - first < height {
                last + 1 - height
            } else {
                first
            };
        }
        self.top = self.top.min(self.max_top());
        self.follow = self.top >= self.max_top();
    }

    fn input_rows(&self, available: u16, width: u16) -> u16 {
        // Size by wrapped (visual) rows so a long prompt grows the box instead
        // of being clipped; the `› ` prompt takes two columns. It grows up to
        // half the panel, leaving the other half to the conversation, and
        // scrolls beyond that.
        let text_width = width.saturating_sub(2).max(1) as usize;
        let rows =
            termide_ui::input_bar::wrapped_row_count(&self.input_text(), text_width).max(1) as u16;
        rows.min((available / 2).max(1))
            .min(available.saturating_sub(2).max(1))
    }

    /// The welcome banner shown while the session is empty: a logo on the left
    /// and what the agent is set up with (provider, model, agent, directory) on
    /// the right, each column centred in the transcript area. On a narrow panel
    /// the logo is dropped and only the details show.
    fn render_welcome(&mut self, area: Rect, buf: &mut Buffer, colors: &ThemeColors) {
        const LOGO: [&str; 5] = [
            "╭───────╮",
            "│       │",
            "│  ›_   │",
            "│       │",
            "╰───────╯",
        ];
        self.banner_hits.clear();
        if area.width < 14 || area.height == 0 {
            return;
        }
        let logo_w = LOGO
            .iter()
            .map(|l| termide_ui::str_display_width(l))
            .max()
            .unwrap_or(0) as u16;
        let gap = 3u16;
        let show_logo = area.width >= logo_w + gap + 22;
        let info_x = area.x + 2 + if show_logo { logo_w + gap } else { 0 };
        let info_w = (area.x + area.width).saturating_sub(info_x + 1);

        let accent = Style::default()
            .fg(colors.info)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(colors.disabled);
        let fg = Style::default().fg(colors.fg);
        // A re-pickable value (model, agent, tools) is drawn bold in the
        // accent colour, so it reads as clickable; a fixed one (cwd)
        // is plain. The click itself is wired through `banner_hits` below.
        let link = Style::default()
            .fg(colors.info)
            .add_modifier(Modifier::BOLD);
        let field = |name: &str, value: String, clickable: bool| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!("{name:<12}"), dim),
                Span::styled(value, if clickable { link } else { fg }),
            ])
        };
        let cwd = shorten_path(&self.cwd, (info_w as usize).saturating_sub(12));
        // Each entry is a line and, when it names a choice that can be re-picked
        // by clicking, the status action that click triggers.
        let info: Vec<(Line<'static>, Option<&'static str>)> = vec![
            (Line::styled("termide", accent), None),
            (
                Line::styled(termide_i18n::t().agent_banner_subtitle(), dim),
                None,
            ),
            (Line::from(""), None),
            (
                field(
                    "connection",
                    self.connection_display(),
                    self.connections.is_some(),
                ),
                self.connections.is_some().then_some(CONNECTION_ACTION),
            ),
            (
                field("model", self.model_display(), true),
                Some(MODEL_ACTION),
            ),
            (field("agent", self.agent.clone(), true), Some(AGENT_ACTION)),
        ];
        // What the session may use, re-pickable before the first request,
        // when switching it off keeps it out of the context altogether.
        let mut info = info;
        if !self.external {
            let (on, all) = self.toolset_counts();
            info.push((
                field("tools", format!("{on}/{all}"), true),
                Some(TOOLSET_ACTION),
            ));
        }
        info.push((field("cwd", cwd, false), None));
        if !self.shadowed.is_empty() {
            let names: Vec<String> = self
                .shadowed
                .iter()
                .map(|name| format!("/{name}"))
                .collect();
            info.push((
                Line::from(vec![
                    Span::styled(format!("{:<12}", "shadowed"), dim),
                    Span::styled(names.join(", "), Style::default().fg(colors.warning)),
                ]),
                Some(SLASH_CONFLICTS_ACTION),
            ));
        }

        let banner_h = info.len().max(LOGO.len()) as u16;
        let bottom = area.y + area.height;
        let top = area.y + area.height.saturating_sub(banner_h) / 2;
        if show_logo {
            let logo_top = top + (banner_h - LOGO.len() as u16) / 2;
            for (i, line) in LOGO.iter().enumerate() {
                let y = logo_top + i as u16;
                if y >= bottom {
                    break;
                }
                buf.set_stringn(
                    area.x + 2,
                    y,
                    line,
                    logo_w as usize,
                    Style::default().fg(colors.info),
                );
            }
        }
        let info_top = top + (banner_h - info.len() as u16) / 2;
        for (i, (line, action)) in info.iter().enumerate() {
            let y = info_top + i as u16;
            if y >= bottom {
                break;
            }
            buf.set_line(info_x, y, line, info_w);
            // The whole field row is the click target, so the label is as good
            // as the value; an external agent still routes the click, and its
            // action answers with the "unsupported" notice.
            if let Some(action) = action {
                self.banner_hits.push((
                    Rect {
                        x: info_x,
                        y,
                        width: info_w,
                        height: 1,
                    },
                    action,
                ));
            }
        }
    }

    /// The run controls the current state offers: pause and stop while the
    /// agent works, continue in place of pause once a pause is asked for or
    /// has taken effect, stop alone while a stop is under way, none while
    /// idle.
    fn run_buttons(&self) -> Vec<RunButton> {
        let paused = self.paused && !self.is_busy();
        if self.is_busy() && self.stop_requested {
            vec![RunButton::Stop]
        } else if paused || (self.is_busy() && self.pause_requested) {
            vec![RunButton::Continue, RunButton::Stop]
        } else if self.is_busy() && self.runtime.can_pause() {
            vec![RunButton::Pause, RunButton::Stop]
        } else if self.is_busy() {
            // An external agent runs its own loop: it can be stopped, not paused.
            vec![RunButton::Stop]
        } else {
            Vec::new()
        }
    }

    /// A run control's color: neutral at rest, so a control does not look
    /// engaged just because a run is on; continue is green and stop red
    /// only while the pause or stop they stand for is under way.
    fn run_button_color(&self, button: RunButton) -> Color {
        match button {
            RunButton::Pause => self.colors.fg,
            RunButton::Continue => self.colors.success,
            RunButton::Stop if self.stop_requested => self.colors.error,
            RunButton::Stop => self.colors.fg,
        }
    }

    fn render_input(&mut self, area: Rect, buf: &mut Buffer, focused: bool) {
        let colors = self.colors;
        // The run controls sit at the right end of the top border, always in
        // view whatever the transcript's scroll.
        self.run_buttons = self.run_buttons();
        let buttons = self
            .run_buttons
            .iter()
            .map(|&button| {
                let label = match button {
                    RunButton::Pause => "[‖]",
                    RunButton::Continue => "[▶]",
                    RunButton::Stop => "[■]",
                };
                (
                    label.to_string(),
                    Style::default().fg(self.run_button_color(button)),
                )
            })
            .collect();
        self.input.set_border_buttons(buttons);
        // The bar's top border is the divider from the content above and
        // brightens while the input is focused; the agent's name lives in the
        // panel title, not here.
        self.input.render(area, buf, &colors, focused);
    }
}

impl Drop for AgentPanel {
    /// Closing the panel discards its session when it was never used, so an
    /// empty session leaves nothing behind in the list or on disk.
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            discard_if_empty(session);
        }
    }
}

/// Longest prompt shown in the panel title before it is cut.
const MAX_TITLE_CHARS: usize = 60;

/// Start a background `list_models` call, returning the receiver to poll from
/// `tick()`. Used both by the model picker and the silent context-window probe.
fn spawn_model_list(provider: Arc<dyn Provider>) -> Receiver<Result<Vec<ModelInfo>, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(provider.list_models());
    });
    rx
}

/// Local wall-clock time as `HH:MM:SS`, for a transcript block's byline.
/// Queued messages the state strip shows before folding the rest into a count.
const STATE_QUEUED_ROWS: usize = 3;

/// The state strip's lines: a dim dashed rule, then a pause row (`pause`,
/// when one is pending or active) and a row per queued message (its first
/// line, cut to the width), at most [`STATE_QUEUED_ROWS`] of them before a
/// "… N more" row. Empty when there is nothing to show.
fn state_strip<'a>(
    queued: impl ExactSizeIterator<Item = &'a str>,
    pause: Option<&str>,
    width: u16,
    colors: &ThemeColors,
) -> Vec<Line<'static>> {
    let t = termide_i18n::t();
    let dim = Style::default().fg(colors.disabled);
    let total = queued.len();
    if total == 0 && pause.is_none() {
        return Vec::new();
    }
    let width = width as usize;
    let cut = |text: &str, room: usize| termide_ui::path_utils::truncate_right(text, room);
    let mut lines = vec![transcript::separator(width as u16, colors)];
    if let Some(pause) = pause {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", transcript::PAUSED_GLYPH),
                Style::default()
                    .fg(colors.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(cut(pause, width.saturating_sub(2)), dim),
        ]));
    }
    let label = format!(" {}", t.agent_state_queued());
    let label_width = termide_ui::str_display_width(&label);
    for (i, text) in queued.take(STATE_QUEUED_ROWS).enumerate() {
        let first = text.trim().lines().next().unwrap_or("");
        // The first row carries the "queued" label at the right edge, one
        // column short of the scrollbar gutter, like a block's meta.
        let room = width.saturating_sub(3 + if i == 0 { label_width } else { 0 });
        let body = cut(first, room);
        let mut spans = vec![
            Span::styled("› ", Style::default().fg(colors.info)),
            Span::styled(body.clone(), dim),
        ];
        if i == 0 {
            let used = 2 + termide_ui::str_display_width(&body) + label_width;
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used + 1))));
            spans.push(Span::styled(label.clone(), dim));
        }
        lines.push(Line::from(spans));
    }
    if total > STATE_QUEUED_ROWS {
        lines.push(Line::styled(
            format!("  {}", t.agent_state_queued_more(total - STATE_QUEUED_ROWS)),
            dim,
        ));
    }
    lines
}

fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Upper-case the first character of `name`, leaving the rest as written
/// (so `reviewer` → `Reviewer`, `web-dev` → `Web-dev`).
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// First line of `text`, collapsed to one line and cut with an ellipsis.
fn truncate_title(text: &str) -> String {
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX_TITLE_CHARS {
        return single_line;
    }
    let cut: String = single_line.chars().take(MAX_TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The file a successful `edit` or `write` changed, from the result details,
/// so open editors can follow it without waiting for the watcher.
fn changed_file(result: &ToolResultMessage) -> Option<PathBuf> {
    if result.is_error || !matches!(result.tool_name.as_str(), "edit" | "write") {
        return None;
    }
    result
        .details
        .as_ref()?
        .get("path")?
        .as_str()
        .map(PathBuf::from)
}

/// The provider's wire protocol as the status line names it.
fn provider_label(kind: &str) -> &str {
    match kind {
        "openai_compatible" => "OpenAI Compatible",
        "anthropic_compatible" => "Anthropic Compatible",
        "claude_code" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
}

/// An eight-cell fill bar for a 0–100 percentage, e.g. `▰▰▱▱▱▱▱▱` at 20%.
fn context_bar(percent: u64) -> String {
    const CELLS: u64 = 8;
    let filled = (percent * CELLS).div_ceil(100).min(CELLS);
    let mut bar = String::with_capacity(CELLS as usize * 3);
    for i in 0..CELLS {
        bar.push(if i < filled { '▰' } else { '▱' });
    }
    bar
}

/// A path for the welcome banner: the home directory shown as `~`, and, when
/// still wider than `max` columns, cut from the left so the tail (the part that
/// tells directories apart) stays visible.
fn shorten_path(path: &Path, max: usize) -> String {
    let full = path.display().to_string();
    let display = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        .map(|rest| {
            if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            }
        })
        .unwrap_or(full);
    let count = display.chars().count();
    if max <= 1 || count <= max {
        return display;
    }
    let tail: String = display.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

/// The work turn a `/goal` sends when the judge says the goal is not yet
/// reached: the goal restated, plus the one thing the judge found still
/// missing, so the agent keeps working from where it fell short.
fn goal_continuation(goal: &str, reason: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        format!("The goal is not reached yet. Keep working toward it.\nGoal: {goal}")
    } else {
        format!(
            "The goal is not reached yet. Keep working toward it.\nGoal: {goal}\nStill missing: {reason}"
        )
    }
}

/// Split `/loop` arguments into an optional interval and the prompt. When the
/// first word is a duration (`30s`, `5m`, `2h`, or a bare count of seconds) it
/// is the interval and the rest is the prompt; otherwise the whole thing is the
/// prompt (a self-paced loop).
fn parse_loop_args(args: &str) -> (Option<Duration>, &str) {
    match args.split_once(char::is_whitespace) {
        // A duration as the first word is the interval; the rest is the prompt.
        Some((first, rest)) if parse_duration(first).is_some() => {
            (parse_duration(first), rest.trim())
        }
        // No interval (a lone word, or the first word is not a duration): the
        // whole thing is the prompt, a self-paced loop.
        _ => (None, args),
    }
}

/// Parse a duration like `30s`, `5m`, `2h`, or a bare count of seconds.
fn parse_duration(token: &str) -> Option<Duration> {
    let (digits, unit) = match token.chars().last() {
        Some('s') => (&token[..token.len() - 1], 1),
        Some('m') => (&token[..token.len() - 1], 60),
        Some('h') => (&token[..token.len() - 1], 3600),
        _ => (token, 1),
    };
    let n: u64 = digits.parse().ok()?;
    (n > 0).then(|| Duration::from_secs(n * unit))
}

/// A short human duration for the loop notice: `45s`, `5m`, `1m30s`.
fn fmt_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// A byte count as `B`/`KB`/`MB`, for the output-cleaning diagnostic.
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1}MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1000 {
        format!("{}KB", (bytes + 500) / 1000)
    } else {
        format!("{bytes}B")
    }
}

/// Token counts as the status line shows them: `32k`, `1.2M`.
pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let millions = tokens as f64 / 1_000_000.0;
        if millions.fract() < 0.05 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if tokens >= 1000 {
        format!("{}k", (tokens + 500) / 1000)
    } else {
        tokens.to_string()
    }
}

/// `/<name> args` at the start of a message: the command name and the
/// rest. `skill:` may prefix the name (see `slash`). A word with further
/// slashes (`/usr/bin`) is text, not a command.
fn slash_command(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let bare = name.strip_prefix(slash::SKILL_PREFIX).unwrap_or(name);
    if bare.is_empty()
        || !bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((name, args.trim()))
}

/// Delete a session the panel is leaving when it holds no conversation, so
/// empty sessions do not clutter the list or the disk. A session with any
/// message or a user-given name is kept.
fn discard_if_empty(session: Session) {
    if session.is_empty() {
        discard(session);
    }
}

/// Delete `session` from disk unconditionally (the `/clear` path). A failure is
/// logged rather than surfaced: the session is being abandoned regardless.
fn discard(session: Session) {
    if let Err(error) = session.discard() {
        log::warn!("could not remove agent session: {error}");
    }
}

/// The checkpoint store of `session`, under the session directory.
fn checkpoint_store(
    session_dir: Option<&std::path::Path>,
    session: Option<&Session>,
) -> Option<Arc<Mutex<CheckpointStore>>> {
    let dir = session_dir?;
    let session = session?;
    Some(Arc::new(Mutex::new(CheckpointStore::for_session(
        dir,
        session.id(),
    ))))
}

/// The path a tool result saved its full raw log to, if it did.
fn full_log_path(result: &ToolResultMessage) -> Option<std::path::PathBuf> {
    result
        .details
        .as_ref()?
        .get("full_output_path")?
        .as_str()
        .map(std::path::PathBuf::from)
}

/// The span an `@`-file mention occupies on one input line, from the `@`
/// (`start`) to the cursor (`end`), in character columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MentionSpan {
    row: usize,
    start: usize,
    end: usize,
}

/// Files and directories under `root` matching `prefix` (the text after `@`),
/// as completion items: a relative path each, directories ending in `/`,
/// ranked by fuzzy match on the path as the open prompt ranks them. The walk
/// leaves out what git ignores, `.git` and `.termide`, and hidden entries
/// unless `prefix` starts with `.`; it is budgeted, so it stays cheap on
/// every keystroke even in a large tree.
fn file_completions(root: &std::path::Path, prefix: &str) -> Vec<CompletionItem> {
    use termide_ui::fuzzy::{rank, Query};

    const MAX_RESULTS: usize = 50;
    const MAX_VISITED: usize = 4000;

    let mut candidates = termide_walk::project_entries(root, prefix.starts_with('.'), MAX_VISITED);
    // Shortest paths first, so an empty or loose query offers the top of the
    // tree before its depths.
    candidates.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

    let mut query = Query::fuzzy_path(prefix);
    rank(candidates.iter().map(|path| query.score(path)))
        .into_iter()
        .take(MAX_RESULTS)
        .map(|i| {
            let path = &candidates[i];
            let matched = query.positions(path).unwrap_or_default();
            CompletionItem::new(path.clone())
                .with_label(path.clone())
                .with_matched(matched)
        })
        .collect()
}

/// Picker prefix: `●` on the current entry, blank otherwise.
fn current_mark(current: bool) -> &'static str {
    if current {
        "● "
    } else {
        "  "
    }
}

/// Create a session log in `dir` and record the model it starts on, so a
/// later resume comes back on the same model.
fn start_session(
    dir: Option<&std::path::Path>,
    cwd: &std::path::Path,
    connection: &str,
    provider: &str,
    model: &ModelSpec,
    agent: &str,
) -> Option<Session> {
    let mut session = match Session::create_exclusive(dir?, cwd) {
        Ok(session) => session,
        Err(error) => {
            log::warn!("cannot start an agent session log: {error}");
            return None;
        }
    };
    // The connection first: a reopened session goes back to it, and the
    // model after it is that connection's.
    if !connection.is_empty() {
        if let Err(error) = session.append_connection_change(connection) {
            log::warn!("agent session write failed: {error}");
        }
    }
    if let Err(error) = session.append_model_change(provider, &model.id, Some(model.context_window))
    {
        log::warn!("agent session write failed: {error}");
    }
    if let Err(error) = session.append_agent_change(agent) {
        log::warn!("agent session write failed: {error}");
    }
    Some(session)
}

/// What `session` switched off, and the profile to run it with when that
/// differs from what is running: another agent than `current`, or another
/// set switched off than `built_without` (what the running profile was built
/// without). `None` keeps the running profile.
fn session_agent(
    catalog: &dyn AgentCatalog,
    current: &str,
    built_without: &BTreeSet<String>,
    session: Option<&Session>,
) -> (BTreeSet<String>, Option<(String, AgentProfile)>) {
    let off: BTreeSet<String> = session
        .and_then(Session::current_toolset)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let recorded = session
        .and_then(Session::current_agent)
        .filter(|name| name != current);
    if recorded.is_none() && off == *built_without {
        return (off, None);
    }
    let name = recorded.unwrap_or_else(|| current.to_string());
    match catalog.resolve_without(&name, &off) {
        Some(profile) => (off, Some((name, profile))),
        None => {
            log::warn!("session ran as agent {name}, which no longer exists; using {current}");
            (off, None)
        }
    }
}

/// The configured model with the id and context window `session` last ran
/// on, when it recorded them: a resumed conversation continues on its own
/// model.
fn session_model(configured: &ModelSpec, session: Option<&Session>) -> ModelSpec {
    let mut model = match session.and_then(Session::current_model) {
        Some(recorded) if !recorded.id.is_empty() => ModelSpec {
            id: recorded.id,
            context_window: recorded.context_window.unwrap_or(configured.context_window),
            ..configured.clone()
        },
        _ => configured.clone(),
    };
    // A reasoning choice made in this session (the status-bar toggle) outlives
    // a resume, overriding the configured default.
    if let Some(reasoning) = session.and_then(Session::current_reasoning) {
        model.reasoning = reasoning;
    }
    model
}

/// What [`spawn_runtime`] hands back.
struct Spawned {
    runtime: Box<dyn Backend>,
    permission_rx: Receiver<PermissionEnvelope>,
    question_rx: Receiver<QuestionEnvelope>,
    transcript: Transcript,
    mode: ModeHandle,
    external: bool,
}

/// Spawn the agent — the built-in loop on a worker thread, or the external
/// agent `backend` makes — with its permission channel, a transcript
/// mirroring `session`'s history and the live mode handle. An external agent
/// that cannot start is reported in the transcript and the built-in loop
/// runs instead.
#[allow(clippy::too_many_arguments)]
fn spawn_runtime(
    provider: &Arc<dyn Provider>,
    tools: &ToolRegistry,
    model: &ModelSpec,
    cwd: &std::path::Path,
    system_prompt: &str,
    rules: PermissionRules,
    compaction: CompactionPolicy,
    compaction_prompts: &CompactionPrompts,
    plan_prompt: &PlanPrompt,
    goal_prompt: &GoalPrompt,
    handoff_prompt: &HandoffPrompt,
    persist_rule: Option<PersistFn>,
    extra_hooks: Option<&HooksFactory>,
    backend: Option<&BackendFactory>,
    checkpoints: Option<Arc<Mutex<CheckpointStore>>>,
    fold: FoldMode,
    session: Option<&Session>,
    blocked: &Blocked,
) -> Spawned {
    let cancel = CancelToken::new();
    let (prompter, permission_rx) = permission_channel(cancel.clone());
    // The `question` tool asks through this; an external agent asks its own
    // way, so its asker is dropped and nothing ever arrives.
    let (asker, question_rx) = question_channel(cancel.clone());
    let system_prompt = if rules.mode == Mode::Plan {
        plan_prompt.apply(system_prompt)
    } else {
        system_prompt.to_string()
    };
    let system_prompt = system_prompt.as_str();
    // The external backend, if one is used, is handed the same rules so its
    // permission requests get the built-in agent's treatment (read-only
    // commands and matching rules pass without a prompt).
    let backend_rules = rules.clone();
    let mut hooks = PermissionHooks::new(rules, Box::new(prompter));
    let mode = hooks.mode_handle();
    if let Some(persist) = persist_rule {
        hooks = hooks.with_persist(Box::new(persist) as PersistRule);
    }
    // The chain every call of termide's tools runs through, before the
    // permission decision: what the session switched off goes first, refused
    // whatever else would allow it; then plan mode's guard, so nothing — not
    // even a hook's approval — changes a file while it is on; then the
    // checkpoint recorder, so no call that runs is missed; then the command
    // hooks, which may block or approve before anyone is asked, and whose
    // rewritten arguments are what the rules then judge.
    let guards = |checkpoints: Option<Arc<Mutex<CheckpointStore>>>| {
        let mut chain: Vec<Box<dyn Hooks>> = vec![
            Box::new(ToolsetGuard {
                blocked: Arc::clone(blocked),
            }),
            Box::new(PlanGuard::new(mode.clone())),
        ];
        if let Some(store) = checkpoints {
            chain.push(Box::new(CheckpointHooks::new(store)));
        }
        if let Some(factory) = extra_hooks {
            chain.push(factory());
        }
        chain
    };

    let mut transcript = Transcript::default();
    transcript.set_fold(fold);
    let history = session
        .map(|s| s.context_messages_with_times(compaction_prompts))
        .unwrap_or_default();
    for logged in &history {
        push_history(&mut transcript, logged);
    }
    let messages: Vec<Message> = history.into_iter().map(|logged| logged.message).collect();

    if let Some(factory) = backend {
        // The external agent gets its own prompter on a channel of its own,
        // and the same rules, so it builds permission hooks that decide its
        // requests exactly as the built-in agent's do.
        let (external_prompter, external_rx) = permission_channel(cancel.clone());
        // termide's tools, for an agent that calls them in place of its own:
        // each call runs through the built-in loop's chain, asking on the
        // same channel under the same live mode.
        let mut host_permissions =
            PermissionHooks::new(backend_rules.clone(), Box::new(external_prompter.clone()))
                .with_mode_handle(mode.clone());
        if let Some(persist) = persist_rule {
            host_permissions = host_permissions.with_persist(Box::new(persist) as PersistRule);
        }
        let mut host_chain = guards(checkpoints.clone());
        host_chain.push(Box::new(host_permissions));
        match factory(BackendSetup {
            cwd: cwd.to_path_buf(),
            prompter: external_prompter,
            cancel: cancel.clone(),
            rules: backend_rules,
            persist: persist_rule.map(|f| Box::new(f) as PersistRule),
            mode: mode.clone(),
            system_prompt: system_prompt.to_string(),
            host_tools: Some(HostTools {
                tools: tools.clone(),
                hooks: Box::new(ChainedHooks::new(host_chain)),
            }),
        }) {
            Ok(runtime) => {
                if !messages.is_empty() {
                    transcript.push(Item::Notice {
                        text: termide_i18n::t().agent_notice_external_history().into(),
                        kind: NoticeKind::Info,
                    });
                }
                return Spawned {
                    runtime,
                    permission_rx: external_rx,
                    question_rx,
                    transcript,
                    mode,
                    external: true,
                };
            }
            Err(error) => transcript.push(Item::Notice {
                text: termide_i18n::t().agent_notice_external_failed_fmt(&error.to_string()),
                kind: NoticeKind::Error,
            }),
        }
    }

    let agent = Agent::new(
        Arc::clone(provider),
        tools.clone(),
        model.clone(),
        cwd.to_path_buf(),
    )
    .with_system_prompt(system_prompt)
    .with_compaction(compaction)
    .with_compaction_prompts(compaction_prompts.clone())
    .with_goal_prompt(goal_prompt.clone())
    .with_handoff_prompt(handoff_prompt.clone())
    .with_messages(messages)
    .with_asker(asker);
    let mut chain = guards(checkpoints);
    chain.push(Box::new(hooks));
    let hooks: Box<dyn Hooks> = Box::new(ChainedHooks::new(chain));
    let runtime = AgentRuntime::spawn_with_cancel(agent, hooks, cancel);
    Spawned {
        runtime: Box::new(runtime),
        permission_rx,
        question_rx,
        transcript,
        mode,
        external: false,
    }
}

/// Mirror a session's message into transcript items when a session is
/// reopened. The log's timestamp restores when each block was written; its
/// recorded timing, with the turn's own token usage, restores the `⏫`/`✍️`
/// cost and a tool's `🕒` duration, as the live run showed them.
fn push_history(transcript: &mut Transcript, logged: &LoggedMessage) {
    let at = hms_from_millis(logged.timestamp);
    match &logged.message {
        Message::User(user) => transcript.push(Item::User {
            text: user.plain_text(),
            at,
            command: user.command.clone(),
        }),
        Message::Assistant(assistant) => {
            let cost = match logged.timing {
                Some(Timing::Turn { prefill_ms, gen_ms }) => Some(transcript::Cost {
                    prefill_ms,
                    gen_ms,
                    input: assistant.usage.input,
                    output: assistant.usage.output,
                }),
                _ => None,
            };
            // Reasoning is restored as its own block above the tools and answer.
            // When it is present it carries the turn's cost, and the answer is
            // left with its time alone, matching a live turn.
            let thinking = assistant.thinking_text();
            let has_thinking = !thinking.trim().is_empty();
            if has_thinking {
                transcript.push(Item::Thinking {
                    text: thinking,
                    streaming: false,
                    at: at.clone(),
                    cost,
                });
            }
            for call in assistant.tool_calls() {
                transcript.push(Item::Tool {
                    call: call.clone(),
                    result: None,
                    live: None,
                    at: at.clone(),
                    duration_ms: None,
                    waited_ms: None,
                    waiting: false,
                });
            }
            // The answer keeps its wall-clock time; skip an empty answer block
            // when the reasoning already stands for the turn (a tool-only turn),
            // so no phantom block is left behind.
            let answer = assistant.plain_text();
            let error = assistant.error_message.clone();
            if !answer.trim().is_empty() || !has_thinking || error.is_some() {
                transcript.push(Item::Assistant {
                    text: answer,
                    streaming: false,
                    error,
                    at,
                    cost: if has_thinking { None } else { cost },
                    run_ms: None,
                });
            }
        }
        Message::ToolResult(result) => {
            let id = result.tool_call_id.clone();
            let (elapsed, wait) = match logged.timing {
                Some(Timing::Tool {
                    duration_ms,
                    waited_ms,
                }) => (Some(duration_ms), waited_ms),
                _ => (None, None),
            };
            transcript.with_tool(&id, |item| {
                if let Item::Tool {
                    result: slot,
                    at: tool_at,
                    duration_ms,
                    waited_ms,
                    ..
                } = item
                {
                    *slot = Some(result.clone());
                    *tool_at = at;
                    *duration_ms = elapsed;
                    *waited_ms = wait;
                }
            });
        }
    }
}

/// Format an epoch-millis timestamp as the local `HH:MM:SS`, matching
/// [`now_hms`] so restored blocks read the same as live ones.
fn hms_from_millis(ms: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms as i64) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%H:%M:%S").to_string(),
        _ => String::new(),
    }
}

impl Panel for AgentPanel {
    fn name(&self) -> &'static str {
        "agent"
    }

    /// `Agent: <name>` for a named conversation, else `Agent: <first
    /// prompt>`, else `Agent: <working directory>`. The `Agent` label is
    /// replaced by a custom agent's own name (capitalized), so parallel panels
    /// running different agents are told apart. The renderer shortens further
    /// from the left when the panel is narrow, so only a long name or prompt is
    /// cut here.
    fn title(&self) -> String {
        let t = termide_i18n::t();
        let label = if self.agent == DEFAULT_AGENT {
            t.panel_agent().to_string()
        } else {
            capitalize(&self.agent)
        };
        let named = self
            .session
            .as_ref()
            .and_then(Session::name)
            .map(truncate_title);
        let subject = named
            .or_else(|| {
                self.transcript.items().iter().find_map(|item| match item {
                    Item::User { text, command, .. } => {
                        Some(truncate_title(command.as_ref().unwrap_or(text)))
                    }
                    _ => None,
                })
            })
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned());
        format!("{label}: {subject}")
    }

    fn needs_attention(&self) -> bool {
        self.attention
    }

    fn context_menu_items(&self) -> Vec<(String, &'static str)> {
        let t = termide_i18n::t();
        // Only actions with no home elsewhere. New/switch/delete sessions also
        // have F-keys, sessions/prompts/agents live in the AI menu, and the
        // model/agent/mode pickers are status-bar chips — none is repeated here.
        // Session info also answers F3 and `/usage`; the assembled prompt is
        // reached with `/prompt`, so neither needs another menu slot beyond this.
        vec![
            (t.agent_session_info().to_string(), SESSION_INFO_ACTION),
            (t.agent_rename().to_string(), RENAME_ACTION),
            (t.agent_delete_session().to_string(), DELETE_SESSION_ACTION),
        ]
    }

    fn handle_status_action(&mut self, action: &str) -> Vec<PanelEvent> {
        let t = termide_i18n::t();
        match action {
            RENAME_ACTION => vec![PanelEvent::ShowInput {
                prompt: t.agent_rename_prompt().to_string(),
                initial_value: self
                    .session
                    .as_ref()
                    .and_then(Session::name)
                    .unwrap_or_default()
                    .to_string(),
                on_submit: InputAction::Custom(RENAME_ACTION.to_string()),
            }],
            DELETE_SESSION_ACTION => self.ask_delete_session(),
            SLASH_CONFLICTS_ACTION => {
                self.notice_slash_conflicts();
                vec![PanelEvent::NeedsRedraw]
            }
            CONNECTION_ACTION => {
                let Some(connections) = &self.connections else {
                    return Vec::new();
                };
                self.connection_choices = connections.list();
                let options = self
                    .connection_choices
                    .iter()
                    .map(|entry| {
                        let mark = current_mark(entry.name == self.connection);
                        let model = if entry.model.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", entry.model)
                        };
                        format!(
                            "{mark}{} — {}{model}",
                            entry.name,
                            provider_label(&entry.kind)
                        )
                    })
                    .collect();
                vec![PanelEvent::ShowSelect {
                    title: t.agent_pick_connection().to_string(),
                    options,
                    on_select: SelectAction::Custom(CONNECTION_ACTION.to_string()),
                }]
            }
            // An external agent brings its own tools; nothing of ours to list.
            TOOLSET_ACTION if self.external => Vec::new(),
            TOOLSET_ACTION => vec![PanelEvent::ShowChecklist {
                title: t.agent_toolset_title().to_string(),
                prompt: t.agent_toolset_prompt().to_string(),
                items: self.toolset_items(),
                action: TOOLSET_ACTION.to_string(),
            }],
            NEW_SESSION_ACTION => {
                self.switch_session(None);
                vec![PanelEvent::NeedsRedraw]
            }
            RESUME_ACTION => {
                self.session_choices = self.session_list();
                if self.session_choices.is_empty() {
                    return vec![PanelEvent::SetStatusMessage {
                        message: t.agent_no_sessions().to_string(),
                        is_error: false,
                    }];
                }
                let current = self.session.as_ref().map(Session::path);
                let options = self
                    .session_choices
                    .iter()
                    .map(|summary| {
                        let mark = if current == Some(summary.path.as_path()) {
                            "● "
                        } else {
                            "  "
                        };
                        format!(
                            "{mark}{} · {}",
                            civil_date(summary.modified),
                            truncate_title(&summary.label())
                        )
                    })
                    .collect();
                vec![PanelEvent::ShowSelect {
                    title: t.agent_resume().to_string(),
                    options,
                    on_select: SelectAction::Custom(RESUME_ACTION.to_string()),
                }]
            }
            PROMPTS_ACTION => self.prompt_picker(),
            UNDO_ACTION => self.ask_undo(),
            AGENT_ACTION => vec![self.agent_picker()],
            MODEL_ACTION if self.external => self.acp_model_picker(),
            MODE_ACTION if self.external && !self.runtime.follows_mode() => {
                self.notice(PromptError::Unsupported.to_string(), NoticeKind::Warn);
                vec![PanelEvent::NeedsRedraw]
            }
            MODEL_ACTION => self.request_model_list(),
            MODE_ACTION => vec![self.mode_picker()],
            REASONING_ACTION => {
                self.toggle_reasoning();
                vec![PanelEvent::NeedsRedraw]
            }
            SHOW_PROMPT_ACTION => match self.write_system_prompt() {
                Ok(path) => vec![PanelEvent::ViewFile(path)],
                Err(error) => {
                    self.notice(
                        termide_i18n::t().agent_notice_cannot_write_prompt_fmt(&error.to_string()),
                        NoticeKind::Error,
                    );
                    vec![PanelEvent::NeedsRedraw]
                }
            },
            SESSION_INFO_ACTION => self.session_summary(),
            _ => vec![],
        }
    }

    fn icon(&self) -> Option<&'static str> {
        Some("🤖")
    }

    fn width_preference(&self) -> WidthPreference {
        WidthPreference::PreferWide
    }

    fn prepare_render(&mut self, theme: &Theme, _config: &Arc<Config>) {
        self.colors = ThemeColors::from(theme);
        self.is_light = theme.is_light_theme();
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        buf.set_style(area, Style::default().fg(self.colors.fg).bg(self.colors.bg));
        // Shown focused, whatever waited is now in front of the user.
        if ctx.is_focused {
            self.attention = false;
        }

        let input_rows = self.input_rows(area.height, area.width);
        // The input bar carries its own titled top border, which divides it
        // from the content above, so the box is one row taller than its text.
        let bar_rows = input_rows + 1;
        // The agent's question sits above the input; when a card is present a
        // plain separator divides it from the transcript (the bar's own border
        // divides the card from the input). When the panel is too short for the
        // card the keys still answer.
        let form_rows = self
            .pending
            .as_ref()
            .map_or(0, |pending| pending.form().height(area.width))
            .min(area.height.saturating_sub(bar_rows + 1));
        let has_separator = form_rows > 0 && area.height > bar_rows + form_rows;
        // The state strip (a pending pause, queued messages) sits between the
        // transcript and the card, leaving the transcript at least one row.
        let text_width = area.width.saturating_sub(1).max(1);
        let mut state = self.state_lines(text_width);
        let room = area
            .height
            .saturating_sub(bar_rows + form_rows + u16::from(has_separator) + 1);
        state.truncate(room as usize);
        let state_rows = state.len() as u16;
        let transcript_height = area
            .height
            .saturating_sub(bar_rows + form_rows + state_rows)
            .saturating_sub(u16::from(has_separator));
        self.transcript_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: transcript_height,
        };
        self.input_area = Rect {
            x: area.x,
            y: area.y + area.height - bar_rows,
            width: area.width,
            height: bar_rows,
        };
        let form_area = Rect {
            x: area.x,
            y: self.input_area.y - form_rows,
            width: area.width,
            height: form_rows,
        };

        // The rightmost column is the scrollbar gutter (`text_width` above), so
        // wrapped text never sits under the bar.
        let colors = self.colors;
        let is_light = self.is_light;
        // The streaming block's live meta (ticking time + spinner) sits after
        // the last block while the agent works, animating on the ~10 fps redraw.
        let footer = self.live_footer_lines(text_width);
        self.transcript.set_live_footer(footer);
        let total = self.transcript.lines(text_width, &colors, is_light).len();
        let max_top = total.saturating_sub(transcript_height as usize);
        if self.follow {
            self.top = max_top;
        } else {
            self.top = self.top.min(max_top);
        }
        // Keep the chat selection valid, on screen, and note the flat-line
        // range to tint — computed now, before `lines` borrows the transcript.
        let item_count = self.transcript.items().len();
        let mut selected_range: Option<(usize, usize)> = None;
        if self.chat_focus && item_count > 0 {
            self.selected = self
                .transcript
                .selectable_near(self.selected)
                .unwrap_or(item_count - 1);
            // Scrolling stays free while a block is selected: the view is
            // brought to a block only when the selection moves (see
            // `scroll_selected_into_view`), not on every frame. Here we only
            // note the block's flat-line range to tint.
            // The rule or gap around a block stays out of the highlight.
            selected_range = self.transcript.content_lines_of(self.selected);
        }
        let lines = self.transcript.lines(text_width, &colors, is_light);
        // The block under the chat cursor is shown inverted (text and
        // background swapped), so the selection reads as one solid block.
        let selected_style = Style::default().fg(colors.bg).bg(colors.fg);
        if lines.is_empty() {
            // A fresh session shows a welcome banner in place of the (empty)
            // transcript: the logo and what the agent is set up with.
            let welcome = Rect {
                height: transcript_height,
                ..area
            };
            self.render_welcome(welcome, buf, &colors);
        } else {
            // No banner while the session has content, so its click targets go.
            self.banner_hits.clear();
            for row in 0..transcript_height as usize {
                let Some(line) = lines.get(self.top + row) else {
                    break;
                };
                buf.set_line(area.x, area.y + row as u16, line, text_width);
                if selected_range.is_some_and(|(f, l)| self.top + row >= f && self.top + row <= l) {
                    for dx in 0..text_width {
                        let cell = &mut buf[(area.x + dx, area.y + row as u16)];
                        // Success and error keep their hue under the
                        // selection, inverted like the rest: an edit's diff,
                        // a status glyph or a failure still reads as one.
                        let style = if cell.fg == colors.success || cell.fg == colors.error {
                            Style::default().fg(colors.bg).bg(cell.fg)
                        } else {
                            selected_style
                        };
                        cell.set_style(style);
                    }
                }
                // A mouse selection over the text, as a terminal shows one.
                if let Some((start, end)) = self
                    .text_selection
                    .and_then(|sel| sel.columns_on(self.top + row, text_width as usize))
                {
                    for dx in start..end {
                        buf[(area.x + dx as u16, area.y + row as u16)].set_style(
                            Style::default()
                                .fg(colors.selection_fg)
                                .bg(colors.selection_bg),
                        );
                    }
                }
            }
        }
        self.scrollbars.vertical = ScrollBar::render_tracked(
            buf,
            ctx.border_right_x.unwrap_or(area.x + area.width - 1),
            area.y,
            transcript_height,
            self.top,
            transcript_height as usize,
            total,
            &self.colors,
            ctx.is_focused,
        );

        let state_y = area.y + transcript_height;
        for (row, line) in state.iter().enumerate() {
            buf.set_line(area.x, state_y + row as u16, line, text_width);
        }
        // The strip's pending-pause line (after its rule) withdraws the pause
        // on a click.
        self.pause_row = (self.pause_requested && state.len() > 1).then_some(state_y + 1);
        if has_separator {
            let y = form_area.y - 1;
            let style = Style::default().fg(if ctx.is_focused {
                self.colors.border_focused
            } else {
                self.colors.disabled
            });
            for dx in 0..area.width {
                buf[(area.x + dx, y)].set_symbol("─").set_style(style);
            }
        }
        let input_area = self.input_area;
        self.render_input(input_area, buf, ctx.is_focused && !self.chat_focus);
        if form_rows >= 3 {
            if let Some(pending) = &mut self.pending {
                pending
                    .form_mut()
                    .render(form_area, buf, &colors, ctx.is_focused);
            }
        }
        if ctx.is_focused && transcript_height > 0 {
            // The completion list overlays the bottom of the transcript,
            // right above the input bar.
            let above = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: transcript_height,
            };
            if let Some(list) = &mut self.completion {
                list.render(above, buf, &colors);
            }
        }
    }

    fn handle_key(&mut self, chord: KeyChord) -> Vec<PanelEvent> {
        // A shortcut — a Ctrl/Alt chord, or any key while the chat has focus
        // and nothing is being typed — matches on the canonical form, so it
        // works on a non-Latin layout too (`Ctrl+щ` is `Ctrl+O`). Typing into
        // the input or a pending question keeps the raw key.
        let raw = chord.raw;
        let shortcut = raw
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || (self.chat_focus && self.pending.is_none());
        let key = if shortcut { chord.canonical } else { raw };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = (self.viewport_height() as i32 - 1).max(1);

        // A pending question takes the keys first: the arrows, Enter, a
        // digit or Esc answer it; only scrolling passes by.
        if let Some(pending) = &mut self.pending {
            let action = if ctrl || alt {
                ChoiceAction::NotHandled
            } else {
                pending.form_mut().handle_key(key)
            };
            let scroll_key = matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                || (ctrl
                    && matches!(
                        key.code,
                        KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Home
                            | KeyCode::End
                            | KeyCode::Char('o')
                    ));
            if self.apply_form_action(action.clone()) {
                return vec![PanelEvent::NeedsRedraw];
            }
            if action == ChoiceAction::NotHandled && !scroll_key {
                return vec![];
            }
        }

        // F2 renames the session, wherever the focus sits in the panel — the
        // same prompt as the `[≡]` menu's Rename.
        if key.code == KeyCode::F(2) && !ctrl && !alt && !shift {
            return self.handle_status_action(RENAME_ACTION);
        }
        // F3 shows a summary of the session, F4 offers a checkpoint to roll back
        // to.
        if key.code == KeyCode::F(3) && !ctrl && !alt && !shift {
            return self.session_summary();
        }
        if key.code == KeyCode::F(4) && !ctrl && !alt && !shift {
            return self.ask_rollback();
        }
        // F6 switches session (the picker), F7 starts a new one, F8 deletes the
        // current one behind a confirmation card.
        if key.code == KeyCode::F(6) && !ctrl && !alt && !shift {
            return self.handle_status_action(RESUME_ACTION);
        }
        if key.code == KeyCode::F(7) && !ctrl && !alt && !shift {
            return self.handle_status_action(NEW_SESSION_ACTION);
        }
        if key.code == KeyCode::F(8) && !ctrl && !alt && !shift {
            return self.ask_delete_session();
        }

        // The completion list gets the navigation keys while it is open, except
        // the `Shift`-held ones: those extend the prompt's selection.
        let selecting = shift
            && matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
            );
        let completion_action = match &mut self.completion {
            Some(list) if !ctrl && !alt && !selecting => list.handle_key(key),
            _ => CompletionAction::NotHandled,
        };
        match completion_action {
            CompletionAction::Handled => return vec![PanelEvent::NeedsRedraw],
            CompletionAction::Dismiss => {
                self.completion = None;
                return vec![PanelEvent::NeedsRedraw];
            }
            CompletionAction::Accept => {
                // Enter on the command already typed in full sends it; on a
                // partial one, or on Tab, it completes, like a shell.
                let typed = self.input_area().text();
                let exact = self.completion_span.is_none()
                    && key.code == KeyCode::Enter
                    && self
                        .completion
                        .as_ref()
                        .and_then(CompletionList::selected_item)
                        .is_some_and(|item| format!("/{}", item.value) == typed.trim());
                if exact {
                    return self.submit();
                }
                self.accept_completion();
                return vec![PanelEvent::NeedsRedraw];
            }
            CompletionAction::NotHandled => {}
        }

        // Chat focus: the arrows walk the blocks, Space/Enter fold the one
        // under the cursor, Tab or Esc hands focus back to the input.
        if self.chat_focus {
            let count = self.transcript.items().len();
            match key.code {
                KeyCode::Tab | KeyCode::Esc => {
                    self.chat_focus = false;
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::Up if !ctrl => {
                    if let Some(prev) = (0..self.selected)
                        .rev()
                        .find(|&i| self.transcript.is_selectable(i))
                    {
                        self.selected = prev;
                    }
                    self.follow = false;
                    self.scroll_selected_into_view();
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::Down if !ctrl => {
                    if let Some(next) =
                        (self.selected + 1..count).find(|&i| self.transcript.is_selectable(i))
                    {
                        self.selected = next;
                    }
                    self.follow = false;
                    self.scroll_selected_into_view();
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.transcript.toggle_expanded(self.selected);
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::Char('o') if !ctrl => {
                    return self.open_selected_in_panel();
                }
                KeyCode::Char('o') if ctrl => {
                    let expand = !self.transcript.any_expanded();
                    self.transcript.set_all_expanded(expand);
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::PageUp => {
                    self.scroll_by(-page);
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::PageDown => {
                    self.scroll_by(page);
                    return vec![PanelEvent::NeedsRedraw];
                }
                // Everything else is swallowed so it does not type into the
                // (unfocused) input.
                _ => return vec![],
            }
        }
        // From the input, Tab moves focus into the chat when it has a block
        // (annotations alone give the cursor nowhere to stop).
        let last_block = self
            .transcript
            .items()
            .len()
            .checked_sub(1)
            .and_then(|last| self.transcript.selectable_near(last));
        if let (KeyCode::Tab, Some(last_block)) = (key.code, last_block) {
            self.chat_focus = true;
            self.follow = false;
            self.selected = last_block;
            return vec![PanelEvent::NeedsRedraw];
        }

        match key.code {
            KeyCode::Esc => {
                if self.is_busy() {
                    self.abort();
                } else if self.goal_task.take().is_some() {
                    self.notice(
                        termide_i18n::t().agent_notice_goal_stopped(),
                        NoticeKind::Info,
                    );
                } else if self.loop_task.take().is_some() {
                    self.notice(
                        termide_i18n::t().agent_notice_loop_stopped(),
                        NoticeKind::Info,
                    );
                } else if !self.input_area().is_empty() {
                    self.clear_input();
                    self.after_edit();
                } else {
                    return vec![];
                }
            }
            KeyCode::Enter if shift || alt => {
                self.input_area_mut().insert_newline();
                self.after_edit();
            }
            KeyCode::Char('j') if ctrl => {
                self.input_area_mut().insert_newline();
                self.after_edit();
            }
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('o') if ctrl => {
                let expand = !self.transcript.any_expanded();
                self.transcript.set_all_expanded(expand);
            }
            KeyCode::BackTab if !self.external || self.runtime.follows_mode() => {
                let next = self.mode.get().next();
                return vec![self.set_mode(next), PanelEvent::NeedsRedraw];
            }
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown => self.scroll_by(page),
            KeyCode::Home if ctrl => {
                self.top = 0;
                self.follow = false;
            }
            KeyCode::End if ctrl => self.follow = true,
            KeyCode::Up if ctrl => self.scroll_by(-1),
            KeyCode::Down if ctrl => self.scroll_by(1),
            KeyCode::Up | KeyCode::Down => {
                // Past the first or last line, `↑` first takes back what is
                // still queued, then the arrows walk through what was asked
                // before, as in a shell; with Shift held the selection takes
                // the arrow and history waits.
                let up = key.code == KeyCode::Up;
                let handled = self.input.edit_field(0, key) != FieldEdit::NotHandled
                    || shift
                    || (up && self.unqueue())
                    || self.recall(up);
                if !handled {
                    return vec![];
                }
            }
            // Prompt clipboard: the panel owns these so a large paste keeps its
            // placeholder handling and a failure can raise a notice.
            KeyCode::Char('c') if ctrl => {
                if !self.copy_input_selection() && !self.copy_text_selection() {
                    return vec![];
                }
            }
            KeyCode::Char('x') if ctrl => {
                if !self.cut_input_selection() {
                    return vec![];
                }
                self.after_edit();
            }
            KeyCode::Char('v') if ctrl => {
                if !self.paste_clipboard() {
                    return vec![];
                }
                self.after_edit();
            }
            // Everything the prompt edits with: typing, deletion, character and
            // word navigation and selection.
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Char(_)
                if !alt =>
            {
                let edit = self.input.edit_field(0, key);
                if edit == FieldEdit::NotHandled {
                    return vec![];
                }
                if edit == FieldEdit::Edited {
                    self.after_edit();
                }
            }
            _ => return vec![],
        }
        vec![PanelEvent::NeedsRedraw]
    }

    fn captures_escape(&self) -> bool {
        self.pending.is_some()
            || self.completion.is_some()
            || self.is_busy()
            || self.loop_task.is_some()
            || self.goal_task.is_some()
            || !self.input_area().is_empty()
    }

    fn handle_scroll(&mut self, delta: i32, _panel_area: Rect) -> Vec<PanelEvent> {
        self.scroll_by(delta);
        vec![PanelEvent::NeedsRedraw]
    }

    fn handle_mouse(&mut self, event: MouseEvent, _panel_area: Rect) -> Vec<PanelEvent> {
        // The prompt box claims its own presses and drags: a press places the
        // cursor, a drag selects the text under it. It is asked first because
        // the bar sits below the transcript, whose rows would otherwise take
        // every click, and because a release must reach the bar to end a drag
        // that started in it — even after the pointer has been dragged up into
        // the transcript. A pending question keeps its clicks to itself.
        // A run control on the prompt's border acts on the press.
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            let button = self
                .input
                .border_button_at(event.column, event.row)
                .and_then(|index| self.run_buttons.get(index).copied());
            if let Some(button) = button {
                match button {
                    RunButton::Pause => {
                        self.request_pause();
                    }
                    RunButton::Continue if self.paused && !self.is_busy() => self.resume(),
                    RunButton::Continue => self.cancel_pause(),
                    RunButton::Stop if self.paused && !self.is_busy() => self.stop_paused(),
                    RunButton::Stop => self.abort(),
                }
                return vec![PanelEvent::NeedsRedraw];
            }
        }
        if self.pending.is_none() && self.press.is_none() && self.input.mouse_hits(event) {
            self.input.handle_mouse(event);
            match event.kind {
                MouseEventKind::Up(_) => return vec![],
                _ => {
                    self.chat_focus = false;
                    return vec![PanelEvent::NeedsRedraw];
                }
            }
        }
        match event.kind {
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.press else {
                    return vec![];
                };
                // Dragged past an edge, the transcript scrolls under it.
                let area = self.transcript_area;
                if event.row < area.y {
                    self.scroll_by(-1);
                } else if event.row >= area.y + area.height {
                    self.scroll_by(1);
                }
                let head = self.cell_at(event.column, event.row);
                self.text_selection = Some(select::TextSelection { anchor, head });
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(press) = self.press.take() else {
                    return vec![];
                };
                if self.text_selection.is_some_and(|sel| !sel.is_empty()) {
                    return vec![PanelEvent::NeedsRedraw];
                }
                self.text_selection = None;
                return self.click_line(press.line);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.pending.is_some() {
                    // A click on a row selects it, and only a second click (a
                    // double click) on the same row confirms — so a misplaced
                    // click cannot answer. A click on the detail folds it.
                    let hit = self
                        .pending
                        .as_ref()
                        .unwrap()
                        .form()
                        .hit(event.column, event.row);
                    if let Some(index) = hit {
                        if self.form_clicks.click(index) >= 2 {
                            self.form_clicks.reset();
                            let action =
                                self.pending.as_mut().unwrap().form_mut().activate_at(index);
                            if self.apply_form_action(action) {
                                return vec![PanelEvent::NeedsRedraw];
                            }
                        } else {
                            self.pending.as_mut().unwrap().form_mut().select(index);
                        }
                        return vec![PanelEvent::NeedsRedraw];
                    }
                    if self
                        .pending
                        .as_mut()
                        .unwrap()
                        .form_mut()
                        .click_select(event.column, event.row)
                    {
                        self.form_clicks.reset();
                        return vec![PanelEvent::NeedsRedraw];
                    }
                }
                if let Some(list) = &mut self.completion {
                    if let Some(index) = list.hit(event.column, event.row) {
                        list.select(index);
                        self.accept_completion();
                        return vec![PanelEvent::NeedsRedraw];
                    }
                }
                // A click on a re-pickable field in the welcome banner opens its
                // picker — the same one its status-bar chip opens.
                let banner_hit = self
                    .banner_hits
                    .iter()
                    .find(|(rect, _)| {
                        event.column >= rect.x
                            && event.column < rect.x + rect.width
                            && event.row == rect.y
                    })
                    .map(|(_, action)| *action);
                if let Some(action) = banner_hit {
                    return self.handle_status_action(action);
                }
                let area = self.transcript_area;
                let inside = event.column >= area.x
                    && event.column < area.x + area.width
                    && event.row >= area.y
                    && event.row < area.y + area.height;
                // The state strip's pending-pause line withdraws the pause.
                if self.pause_requested && self.pause_row == Some(event.row) {
                    self.cancel_pause();
                    return vec![PanelEvent::NeedsRedraw];
                }
                if !inside {
                    // A click below the transcript lands on the input: hand focus
                    // back to it so typing resumes.
                    if self.chat_focus {
                        self.chat_focus = false;
                        return vec![PanelEvent::NeedsRedraw];
                    }
                    return vec![];
                }
                // The press may start a text selection; only a release
                // without a drag clicks the block under it.
                self.press = Some(self.cell_at(event.column, event.row));
                self.text_selection = None;
            }
            _ => return vec![],
        }
        vec![PanelEvent::NeedsRedraw]
    }

    fn tick(&mut self) -> Vec<PanelEvent> {
        let mut changed = false;
        // A pause's line ticks its length, redrawn once a second; so does a
        // call's wait on a permission question, until the question is gone.
        if let Some(start) = self.pause_start {
            changed |= self.transcript.set_pause_length(millis(start.elapsed()));
        }
        if self.context_stale && !self.is_busy() {
            self.refresh_context();
            changed = true;
        }
        if let Some((start, before)) = self.permission_wait {
            if matches!(
                self.pending,
                Some(Pending::Permission { .. } | Pending::Question { .. })
            ) {
                changed |= self
                    .transcript
                    .set_tool_wait(before.saturating_add(millis(start.elapsed())), true);
            } else {
                self.end_permission_wait();
                changed = true;
            }
        }
        for event in self.runtime.drain() {
            self.apply(event);
            changed = true;
        }
        let mut events = self.poll_permissions();
        events.append(&mut self.poll_questions());
        events.append(&mut self.pending_events);
        changed |= self.poll_late_tools();
        changed |= self.poll_command();
        let fetched = self.model_fetch.as_ref().map(Receiver::try_recv);
        match fetched {
            Some(Ok(result)) => {
                self.model_fetch = None;
                events.push(self.model_picker(result));
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.model_fetch = None;
                events.push(self.model_picker(Err(
                    termide_i18n::t().agent_model_request_dropped().to_string(),
                )));
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
        // The silent context-window probe: adopt the active model's real
        // window when it arrives, and stay quiet on failure.
        match self.context_probe.as_ref().map(Receiver::try_recv) {
            Some(Ok(Ok(models))) => {
                self.context_probe = None;
                changed |= self.adopt_listed_models(&models);
            }
            Some(Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected)) => {
                self.context_probe = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
        // An external agent's model becomes known once its handshake finishes
        // (its adapter starts asynchronously): adopt the current model for the
        // banner and the Model chip, and note whether it offers a choice.
        if self.external {
            if !self.acp_has_models && !self.runtime.available_models().is_empty() {
                self.acp_has_models = true;
                changed = true;
                // Apply the configured pre-selected model once, now that the
                // agent's models are known.
                if let Some(pref) = self.pending_preferred_model.take() {
                    if self.runtime.current_model().as_deref() != Some(pref.as_str()) {
                        match self.runtime.select_model(pref.clone()) {
                            Ok(()) => self.model.id = pref,
                            Err(error) => {
                                log::warn!("cannot pre-select the model: {error}");
                            }
                        }
                    }
                }
            }
            if let Some(id) = self.runtime.current_model() {
                if id != self.model.id {
                    self.model.id = id;
                    changed = true;
                }
            }
            // The context's fill and size, as the agent reports them.
            if let Some((used, size)) = self.runtime.context_usage() {
                if (used, size) != (self.context_tokens, self.model.context_window) {
                    self.context_tokens = used;
                    self.model.context_window = size;
                    changed = true;
                }
            }
        }
        // A loop whose wait has elapsed starts its next iteration once the
        // panel is free (no run in flight, no card waiting for an answer).
        let due = self
            .loop_task
            .as_ref()
            .and_then(|t| t.next_at)
            .is_some_and(|at| at <= Instant::now());
        if due && !self.is_busy() && self.pending.is_none() {
            events.extend(self.loop_step());
            changed = true;
        }
        // A goal whose work turn has finished runs the judge once the panel is
        // free and no judge call is already in flight.
        let judge_due = self
            .goal_task
            .as_ref()
            .is_some_and(|t| !t.judging && t.judge_at.is_some_and(|at| at <= Instant::now()));
        if judge_due && !self.is_busy() && self.pending.is_none() {
            events.extend(self.run_goal_judge());
            changed = true;
        }
        // While the agent works, keep the ticking timer and the block's
        // spinner moving without waiting for an event (throttled to ~10 fps).
        if self.is_busy() && self.last_anim.elapsed() >= Duration::from_millis(100) {
            self.last_anim = Instant::now();
            changed = true;
        }
        if changed || !events.is_empty() {
            events.push(PanelEvent::NeedsRedraw);
        }
        events
    }

    fn handle_command(&mut self, cmd: PanelCommand<'_>) -> CommandResult {
        match cmd {
            PanelCommand::Paste if !self.chat_focus => match self.paste_clipboard() {
                true => {
                    self.after_edit();
                    CommandResult::Handled(true)
                }
                false => CommandResult::Handled(false),
            },
            PanelCommand::PasteText { text } => {
                self.paste(&text);
                self.after_edit();
                CommandResult::NeedsRedraw(true)
            }
            // Copy takes the chat block while the chat holds focus, and the
            // prompt's selection while the input does.
            PanelCommand::Copy => {
                if self.copy_text_selection() {
                    CommandResult::Handled(true)
                } else if self.chat_focus {
                    match self.selected_block_text() {
                        Some(text) if !text.trim().is_empty() => {
                            self.copy_text(&text);
                            CommandResult::Handled(true)
                        }
                        _ => CommandResult::Handled(false),
                    }
                } else {
                    CommandResult::Handled(self.copy_input_selection())
                }
            }
            PanelCommand::Cut if !self.chat_focus => {
                CommandResult::Handled(self.cut_input_selection())
            }
            PanelCommand::ChecklistDone { action, checked } if action == TOOLSET_ACTION => {
                self.apply_toolset(&checked);
                self.pending_events.push(PanelEvent::NeedsRedraw);
                CommandResult::Handled(true)
            }
            PanelCommand::SelectionMade { action, index } if action == RESUME_ACTION => {
                CommandResult::Handled(self.resume_choice(index))
            }
            PanelCommand::SelectionMade { action, index } if action == ROLLBACK_ACTION => {
                let events = self.perform_rollback(index);
                self.pending_events.extend(events);
                CommandResult::Handled(true)
            }
            PanelCommand::SelectionMade { action, index } if action == PROMPTS_ACTION => {
                let choice = self.prompt_choices.get(index).cloned();
                self.prompt_choices.clear();
                if let Some(template) = choice {
                    self.set_input(&format!("/{} ", template.name));
                    self.after_edit();
                }
                CommandResult::Handled(true)
            }
            PanelCommand::SelectionMade { action, index } if action == AGENT_ACTION => {
                let choice = self.agent_choices.get(index).cloned();
                self.agent_choices.clear();
                CommandResult::Handled(choice.is_some_and(|name| self.switch_agent(&name)))
            }
            PanelCommand::SelectionMade { action, index } if action == MODE_ACTION => {
                if let Some(mode) = Mode::ALL.get(index).copied() {
                    let event = self.set_mode(mode);
                    self.pending_events.push(event);
                }
                CommandResult::Handled(true)
            }
            PanelCommand::SelectionMade { action, index }
                if action == MODEL_ACTION && self.external =>
            {
                let choice = self.acp_models.get(index).cloned();
                self.acp_models.clear();
                if let Some(model) = choice {
                    match self.runtime.select_model(model.id.clone()) {
                        Ok(()) => {
                            self.model.id = model.id.clone();
                            if !self.is_fresh() {
                                self.notice(
                                    termide_i18n::t().agent_notice_model_fmt(&model.id),
                                    NoticeKind::Info,
                                );
                            }
                        }
                        Err(error) => self.notice(
                            termide_i18n::t()
                                .agent_notice_cannot_switch_model_fmt(&error.to_string()),
                            NoticeKind::Warn,
                        ),
                    }
                }
                CommandResult::Handled(true)
            }
            PanelCommand::SelectionMade { action, index } if action == CONNECTION_ACTION => {
                if let Some(entry) = self.connection_choices.get(index).cloned() {
                    self.switch_connection(&entry.name);
                }
                self.connection_choices.clear();
                self.pending_events.push(PanelEvent::NeedsRedraw);
                CommandResult::Handled(true)
            }
            PanelCommand::SelectionMade { action, index } if action == MODEL_ACTION => {
                let choice = self.model_choices.get(index).cloned();
                self.model_choices.clear();
                match choice {
                    Some(model) => {
                        self.switch_model(&model.id, model.context_window);
                    }
                    // The entry after the list: type an id instead.
                    None => {
                        let event = self.model_input();
                        self.pending_events.push(event);
                    }
                }
                CommandResult::Handled(true)
            }
            PanelCommand::InputSubmitted { action, text } if action == MODEL_INPUT_ACTION => {
                CommandResult::Handled(self.switch_model(&text, None))
            }
            PanelCommand::InputSubmitted { action, text } if action == RENAME_ACTION => {
                CommandResult::Handled(self.rename_session(&text))
            }
            PanelCommand::Confirmed { action } if action == DELETE_SESSION_ACTION => {
                self.perform_delete_session();
                CommandResult::Handled(true)
            }
            PanelCommand::GetScrollBars => CommandResult::ScrollBars(self.scrollbars),
            PanelCommand::SetScrollOffset { axis, offset } => {
                if axis == ScrollAxis::Vertical {
                    self.top = offset.min(self.max_top());
                    self.follow = self.top >= self.max_top();
                }
                CommandResult::NeedsRedraw(true)
            }
            _ => CommandResult::None,
        }
    }

    fn status_segments(&self) -> Vec<StatusSegment> {
        // Separators are the panel's job: the status bar concatenates the
        // segments as given. The knobs sit on the left, the figures flush
        // right; a narrow bar cuts the knobs, never the figures. The live
        // phase is not repeated here: each chat block carries its own byline.
        let t = termide_i18n::t();
        let sep = || StatusSegment::new(" │ ", SegmentKind::Label);
        let mut segments = vec![
            StatusSegment::new(" ", SegmentKind::Label),
            StatusSegment::clickable(t.agent_chip_agent(), SegmentKind::Label, AGENT_ACTION),
            StatusSegment::clickable(self.agent.clone(), SegmentKind::Active, AGENT_ACTION),
        ];
        if self.external {
            // An external agent has its own model and permission model, unless
            // termide judges its calls or maps its modes.
            segments.push(StatusSegment::new(" (acp)", SegmentKind::Label));
            if self.runtime.follows_mode() {
                segments.extend([
                    sep(),
                    StatusSegment::clickable(t.agent_chip_mode(), SegmentKind::Label, MODE_ACTION),
                    StatusSegment::clickable(
                        self.mode.get().label(),
                        SegmentKind::Active,
                        MODE_ACTION,
                    ),
                ]);
            }
            // A CLI agent is a connection too: the way back is here.
            if self.connections.is_some() {
                segments.extend([
                    sep(),
                    StatusSegment::clickable(
                        t.agent_chip_connection(),
                        SegmentKind::Label,
                        CONNECTION_ACTION,
                    ),
                    StatusSegment::clickable(
                        self.connection_display(),
                        SegmentKind::Active,
                        CONNECTION_ACTION,
                    ),
                ]);
            }
        } else {
            segments.extend([
                sep(),
                StatusSegment::clickable(t.agent_chip_mode(), SegmentKind::Label, MODE_ACTION),
                StatusSegment::clickable(self.mode.get().label(), SegmentKind::Active, MODE_ACTION),
                sep(),
                StatusSegment::clickable(
                    t.agent_chip_reasoning(),
                    SegmentKind::Label,
                    REASONING_ACTION,
                ),
                StatusSegment::clickable(
                    if self.model.reasoning {
                        t.agent_chip_on()
                    } else {
                        t.agent_chip_off()
                    },
                    SegmentKind::Active,
                    REASONING_ACTION,
                ),
                sep(),
                StatusSegment::clickable(t.agent_chip_tools(), SegmentKind::Label, TOOLSET_ACTION),
                StatusSegment::clickable(
                    {
                        let (on, all) = self.toolset_counts();
                        format!("{on}/{all}")
                    },
                    SegmentKind::Active,
                    TOOLSET_ACTION,
                ),
                sep(),
                StatusSegment::clickable(
                    t.agent_chip_connection(),
                    SegmentKind::Label,
                    CONNECTION_ACTION,
                ),
                StatusSegment::clickable(
                    self.connection_display(),
                    SegmentKind::Active,
                    CONNECTION_ACTION,
                ),
            ]);
        }
        // The agent's model over ACP, when it advertised any: clickable to
        // switch, like the built-in loop's Model chip.
        if !self.external || self.acp_has_models {
            segments.extend([
                sep(),
                StatusSegment::clickable(t.agent_chip_model(), SegmentKind::Label, MODEL_ACTION),
                StatusSegment::clickable(self.model_display(), SegmentKind::Active, MODEL_ACTION),
            ]);
        }
        segments.push(StatusSegment::spacer());
        let queued = self.queued.0 + self.queued.1;
        if queued > 0 {
            segments.push(StatusSegment::new(
                format!("{} ", t.agent_queued_fmt(queued)),
                SegmentKind::Inactive,
            ));
        }
        // Session token totals, for an external agent too when it reports
        // them.
        if self.session_input > 0 || self.session_cached > 0 || self.session_output > 0 {
            segments.push(StatusSegment::new(
                format!("{} ", self.token_totals()),
                SegmentKind::Value,
            ));
        }
        // An external agent's window is known once it reports it; until then
        // the configured fallback would mislead.
        let window_known = !self.external || self.runtime.context_usage().is_some();
        if window_known && self.model.context_window > 0 {
            let percent = ((self.context_tokens * 100) / self.model.context_window).min(100);
            let kind = if percent >= 80 {
                SegmentKind::Warn
            } else {
                SegmentKind::Value
            };
            segments.push(StatusSegment::new(
                format!(
                    "{}/{} {} ",
                    format_tokens(self.context_tokens),
                    format_tokens(self.model.context_window),
                    context_bar(percent)
                ),
                kind,
            ));
        }
        segments
    }

    fn has_running_processes(&self) -> bool {
        self.is_busy()
    }

    /// The working directory and the session log, which is all a restore
    /// needs: the model is in the log and the rest comes from the config.
    fn to_state(&self, _session_dir: &std::path::Path) -> Option<termide_core::PanelState> {
        Some(termide_core::PanelState::Agent {
            cwd: self.cwd.clone(),
            session: self.session_path().map(std::path::Path::to_path_buf),
            agent: (self.agent != DEFAULT_AGENT).then(|| self.agent.clone()),
        })
    }

    fn kill_processes(&mut self) {
        self.runtime.abort();
    }

    fn get_working_directory(&self) -> Option<PathBuf> {
        Some(self.cwd.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests;
