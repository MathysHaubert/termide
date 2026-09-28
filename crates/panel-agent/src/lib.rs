//! The coding agent panel: a transcript above a multi-line input, tool calls
//! collapsed to one line each, permission prompts routed through termide's
//! selection modal.
//!
//! The panel owns an [`AgentRuntime`] and mirrors its events into a
//! [`Transcript`] from `tick()`, so it never blocks the UI thread. Every
//! transcript change also goes to the JSONL [`Session`] when one is attached.

mod events;
mod input;
mod pending;
mod pickers;
mod runtime;
mod select;
mod session_ops;
mod slash;
mod submit;
mod toolset;
mod transcript;

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use termide_agent_core::{
    civil_date, Backend, BackendModel, BackendSetup, CheckpointStore, CommandScript,
    CompactionPolicy, CompactionPrompts, Decision, GoalPrompt, HandoffPrompt, Hooks, LateTools,
    Mode, ModeHandle, ModelInfo, ModelSpec, PermissionEnvelope, PermissionRules, PersistScope,
    PlanPrompt, PromptError, PromptTemplate, Provider, QuestionEnvelope, Session, SessionSummary,
    SkillInfo, Tool, ToolRegistry, ToolResultMessage, DEFAULT_AGENT,
};
use termide_config::Config;
use termide_core::{
    CommandResult, InputAction, KeyChord, Panel, PanelCommand, PanelEvent, RenderContext,
    ScrollAxis, ScrollBars, SegmentKind, SelectAction, StatusSegment, ThemeColors, WidthPreference,
};
use termide_theme::Theme;
use termide_ui::{ClickTracker, CompletionList, InputBar, ScrollBar};

use crate::input::MentionSpan;
use crate::pending::Pending;
use crate::pickers::current_mark;
use crate::runtime::{
    checkpoint_store, session_agent, session_model, spawn_model_list, spawn_runtime, start_session,
    Spawned,
};
use crate::session_ops::discard_if_empty;
use crate::toolset::{Blocked, TOOLSET_ACTION};

pub use transcript::{FoldMode, Item, NoticeKind, Transcript};

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

    #[must_use]
    pub fn session_path(&self) -> Option<&std::path::Path> {
        self.session.as_ref().map(Session::path)
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

    /// The rules the agent runs under: the configured and "always" rules, and
    /// apart from them this session's answers, which count in modes the
    /// configured rules do not. Rebuilt into every agent the panel spawns so
    /// an answer survives a rebuild.
    fn effective_rules(&self) -> PermissionRules {
        let mut rules = self.rules.clone();
        rules.session = self.session_rules.tools.clone();
        rules
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
        self.on_key(chord)
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
        self.on_mouse(event)
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
