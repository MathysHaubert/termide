//! The coding agent panel: a transcript above a multi-line input, tool calls
//! collapsed to one line each, permission prompts routed through termide's
//! selection modal.
//!
//! The panel owns an [`AgentRuntime`] and mirrors its events into a
//! [`Transcript`] from `tick()`, so it never blocks the UI thread. Every
//! transcript change also goes to the JSONL [`Session`] when one is attached.

mod transcript;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use termide_agent_core::{
    civil_date, now_millis, permission_channel, Agent, AgentEvent, Backend, BackendSetup,
    CancelToken, ChainedHooks, CheckpointHooks, CheckpointStore, CommandScript, CompactionPolicy,
    CompactionPrompts, Decision, Hooks, LateTools, Message, Mode, ModeHandle, ModelInfo, ModelSpec,
    PermissionAnswer, PermissionEnvelope, PermissionHooks, PermissionRules, PersistRule, PlanGuard,
    PlanPrompt, PromptTemplate, Provider, Session, SessionSummary, StreamEvent, Tool, ToolRegistry,
    ToolResultMessage, ToolUpdate, UserMessage, DEFAULT_AGENT,
};
use termide_agent_core::{AgentRuntime, PromptError};
use termide_config::Config;
use termide_core::{
    CommandResult, InputAction, KeyChord, Panel, PanelCommand, PanelEvent, RenderContext,
    ScrollAxis, ScrollBars, SegmentKind, SelectAction, StatusSegment, ThemeColors, WidthPreference,
};
use termide_theme::Theme;
use termide_ui::textarea::TextArea;
use termide_ui::{
    ChoiceAction, ChoiceForm, CompletionAction, CompletionItem, CompletionList, InputBar, ScrollBar,
};

pub use transcript::{Item, NoticeKind, Transcript};

/// Labels of the four permission answers, in the order the form shows them.
const PERMISSION_OPTIONS: [&str; 4] = [
    "Allow once",
    "Allow for this session",
    "Allow always",
    "Deny",
];
/// Longest input the panel grows to before it scrolls.
const MAX_INPUT_ROWS: u16 = 5;
/// Context-menu action that renames the session.
const RENAME_ACTION: &str = "agent_rename";
/// Context-menu action that starts a fresh session.
const NEW_SESSION_ACTION: &str = "agent_new_session";
/// Context-menu action that opens the session picker.
const RESUME_ACTION: &str = "agent_resume";
/// Status chip and context-menu action that opens the model picker.
const MODEL_ACTION: &str = "agent_model";
/// Input action carrying a model id typed by hand.
const MODEL_INPUT_ACTION: &str = "agent_model_input";
/// Status chip and context-menu action that opens the permission-mode picker.
const MODE_ACTION: &str = "agent_mode";
/// Status chip that toggles whether the model is asked to reason.
const REASONING_ACTION: &str = "agent_reasoning";
/// Context-menu action that opens the assembled system prompt in a viewer.
const SHOW_PROMPT_ACTION: &str = "agent_show_prompt";
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
/// Context-menu action that undoes the last request.
const UNDO_ACTION: &str = "agent_undo";

/// What a card in the panel is asking: the agent's permission request, or
/// whether a command script that came with the project may run.
enum Pending {
    Permission {
        envelope: PermissionEnvelope,
        form: ChoiceForm,
    },
    Command {
        script: CommandScript,
        args: String,
        form: ChoiceForm,
    },
    Undo {
        form: ChoiceForm,
    },
    /// Plan mode: the agent answered, carry the plan out or keep planning?
    Plan {
        form: ChoiceForm,
    },
}

impl Pending {
    fn form(&self) -> &ChoiceForm {
        match self {
            Pending::Permission { form, .. }
            | Pending::Command { form, .. }
            | Pending::Undo { form }
            | Pending::Plan { form } => form,
        }
    }

    fn form_mut(&mut self) -> &mut ChoiceForm {
        match self {
            Pending::Permission { form, .. }
            | Pending::Command { form, .. }
            | Pending::Undo { form }
            | Pending::Plan { form } => form,
        }
    }
}

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
    /// Where "allow always" rules go; a plain function so it survives a
    /// session switch. `None` keeps such rules in memory only.
    pub persist_rule: Option<PersistFn>,
    /// Directory holding this project's session logs; `None` runs without
    /// persistence and without the session picker.
    pub session_dir: Option<PathBuf>,
    /// Session to start in; `None` creates one in `session_dir`.
    pub session: Option<Session>,
    /// Fold each block to a preview by default (the answer always shows).
    pub autofold: bool,
}

/// Records an "allow always" rule outside the panel (in the project config).
pub type PersistFn = fn(&str, &str, Decision);

/// Makes the extra hooks of one agent (command hooks from `hooks.toml`).
pub type HooksFactory = Arc<dyn Fn() -> Box<dyn Hooks> + Send + Sync>;

/// Starts an external agent (ACP) in place of the built-in loop.
pub type BackendFactory =
    Arc<dyn Fn(BackendSetup) -> Result<Box<dyn Backend>, String> + Send + Sync>;

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
}

/// The app's view of the agent definitions (`agents/<name>/` across the
/// agent directories); the panel only chooses among them.
pub trait AgentCatalog: Send + Sync {
    fn list(&self) -> Vec<AgentEntry>;
    fn resolve(&self, name: &str) -> Option<AgentProfile>;
    /// Prompt templates (`prompts/<name>.md`), for `/<name>` in the input.
    fn prompts(&self) -> Vec<PromptTemplate> {
        Vec::new()
    }
    /// Command scripts (`commands/<name>`), for `/<name>` in the input.
    fn commands(&self) -> Vec<CommandScript> {
        Vec::new()
    }
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

impl Phase {
    /// Short label shown in the status bar and the block footer.
    fn label(self) -> &'static str {
        match self {
            Phase::Prefill => "prefill",
            Phase::Generating => "generating",
            Phase::Tool => "tool",
            Phase::Compact => "compacting",
        }
    }
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

pub struct AgentPanel {
    runtime: Box<dyn Backend>,
    /// The runtime is an external agent: model and mode are not ours to set.
    external: bool,
    permission_rx: Receiver<PermissionEnvelope>,
    /// The question a card in the panel is asking, if any.
    pending: Option<Pending>,
    /// Command scripts the user let run for this session, by name.
    allowed_commands: HashSet<String>,
    /// A command script running on a thread; its output becomes a request.
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
    model: ModelSpec,
    /// The model from the configuration: the base every session's model is
    /// built on, since the log records only an id and a context window.
    configured_model: ModelSpec,
    /// Live permission mode, shared with the hooks on the agent thread.
    mode: ModeHandle,
    /// Models offered by the last picker, in the order they were shown.
    model_choices: Vec<ModelInfo>,
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
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    rules: PermissionRules,
    system_prompt: String,
    compaction: CompactionPolicy,
    compaction_prompts: CompactionPrompts,
    plan_prompt: PlanPrompt,
    /// Fold blocks to a preview by default; passed to each transcript.
    autofold: bool,
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
    queued: (usize, usize),
    /// Tokens of the last reported context, for the status chip.
    context_tokens: u64,
    /// What the agent is doing right now; `None` when idle.
    activity: Option<Activity>,
    /// Session token totals from `Usage`: input (prefill) and output.
    session_input: u64,
    session_output: u64,
    /// When each running tool started, to report how long it took (`🕒`).
    tool_starts: HashMap<String, Instant>,
    /// Throttles the animation redraws requested while busy.
    last_anim: Instant,

    colors: ThemeColors,
    is_light: bool,
    transcript_area: Rect,
    input_area: Rect,
    scrollbars: ScrollBars,
}

impl AgentPanel {
    #[must_use]
    pub fn new(mut setup: AgentPanelSetup) -> Self {
        let session = setup.session.or_else(|| {
            start_session(
                setup.session_dir.as_deref(),
                &setup.cwd,
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
        if let Some((name, profile)) =
            session_agent(setup.catalog.as_ref(), &agent, session.as_ref())
        {
            agent = name;
            system_prompt = profile.system_prompt;
            tools = profile.tools;
            late_tools = profile.late_tools;
            backend = profile.backend;
            if let Some(mode) = profile.mode {
                setup.rules.mode = mode;
            }
        }
        let Spawned {
            runtime,
            permission_rx,
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
            setup.persist_rule,
            setup.hooks.as_ref(),
            backend.as_ref(),
            checkpoints.clone(),
            setup.autofold,
            session.as_ref(),
        );
        // Learn the context window from the provider in the background and
        // adopt the active model's real `max_model_len`; the configured window
        // is only a fallback (an external agent has no such endpoint).
        let context_probe = (!external).then(|| spawn_model_list(Arc::clone(&setup.provider)));
        Self {
            runtime,
            external,
            permission_rx,
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
            mode,
            model_choices: Vec::new(),
            model_fetch: None,
            context_probe,
            pending_events: Vec::new(),
            hooks: setup.hooks,
            backend,
            provider: setup.provider,
            provider_kind: setup.provider_kind,
            tools,
            rules: setup.rules,
            system_prompt,
            compaction: setup.compaction,
            compaction_prompts: setup.compaction_prompts,
            plan_prompt: setup.plan_prompt,
            autofold: setup.autofold,
            prompt_stale: false,
            shown_system: String::new(),
            persist_rule: setup.persist_rule,
            transcript,
            input: InputBar::new(vec![])
                .with_multiline_field("")
                .with_placeholder("Ask the agent…")
                .with_border(String::new(), String::new()),
            history_pos: None,
            draft: String::new(),
            completion: None,
            completion_span: None,
            chat_focus: false,
            selected: 0,
            top: 0,
            follow: true,
            busy: false,
            queued: (0, 0),
            context_tokens: 0,
            activity: None,
            session_input: 0,
            session_output: 0,
            tool_starts: HashMap::new(),
            last_anim: Instant::now(),
            colors: ThemeColors::default(),
            is_light: false,
            transcript_area: Rect::default(),
            input_area: Rect::default(),
            scrollbars: ScrollBars::default(),
        }
    }

    /// Replace the running agent with one continuing `session` (or a fresh
    /// one when `None`). Refuses while a run is in flight.
    pub fn switch_session(&mut self, session: Option<Session>) -> bool {
        if self.is_busy() {
            self.notice("finish or stop the current task first", NoticeKind::Warn);
            return false;
        }
        let session = session.or_else(|| {
            start_session(
                self.session_dir.as_deref(),
                &self.cwd,
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
        if let Some((name, profile)) =
            session_agent(self.catalog.as_ref(), &agent, session.as_ref())
        {
            agent = name;
            system_prompt = profile.system_prompt;
            tools = profile.tools;
            self.late_tools = profile.late_tools;
            self.backend = profile.backend;
            self.waiting_tools.clear();
            if let Some(mode) = profile.mode {
                self.rules.mode = mode;
            }
        }
        let Spawned {
            runtime,
            permission_rx,
            transcript,
            mode,
            external,
        } = spawn_runtime(
            &self.provider,
            &tools,
            &model,
            &self.cwd,
            &system_prompt,
            self.rules.clone(),
            self.compaction,
            &self.compaction_prompts,
            &self.plan_prompt,
            self.persist_rule,
            self.hooks.as_ref(),
            self.backend.as_ref(),
            self.checkpoints.clone(),
            self.autofold,
            session.as_ref(),
        );
        // Dropping the old runtime cancels it and asks its worker to stop.
        self.runtime = runtime;
        self.external = external;
        self.permission_rx = permission_rx;
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
    }

    #[must_use]
    pub fn session_path(&self) -> Option<&std::path::Path> {
        self.session.as_ref().map(Session::path)
    }

    /// Send the input box: a new run when idle, a steering message while
    /// the agent works.
    pub fn submit(&mut self) -> Vec<PanelEvent> {
        let text = self.input_area().text().trim().to_string();
        if text.is_empty() {
            return vec![];
        }
        self.completion = None;
        self.history_pos = None;
        self.draft.clear();
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
                        self.notice("finish or stop the current task first", NoticeKind::Warn)
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
                    self.notice("finish or stop the current task first", NoticeKind::Warn);
                    return vec![PanelEvent::NeedsRedraw];
                }
                self.clear_input();
                if let Some(old) = self.session.take() {
                    discard(old);
                }
                self.switch_session(None);
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((name, args)) => {
                let prompts = self.catalog.prompts();
                if let Some(template) = prompts.iter().find(|p| p.name == name) {
                    template.expand(args)
                } else if let Some(script) =
                    self.catalog.commands().into_iter().find(|c| c.name == name)
                {
                    // A command script: its output becomes the request, once
                    // it has run (and, for a project's script, been allowed).
                    self.clear_input();
                    self.run_command(script, args.to_string());
                    return vec![PanelEvent::NeedsRedraw];
                } else {
                    let mut names: Vec<String> = prompts.iter().map(|p| p.name.clone()).collect();
                    names.extend(self.catalog.commands().into_iter().map(|c| c.name));
                    names.push(COMPACT_COMMAND.to_string());
                    if self.session_dir.is_some() {
                        names.push(NEW_COMMAND.to_string());
                        names.push(CLEAR_COMMAND.to_string());
                    }
                    self.notice(
                        format!("no command named {name}; available: {}", names.join(", ")),
                        NoticeKind::Warn,
                    );
                    return vec![PanelEvent::NeedsRedraw];
                }
            }
            None => text,
        };
        self.clear_input();
        self.send(text)
    }

    /// Send `text` as the next request: a new run when idle, a steering
    /// message while the agent works.
    fn send(&mut self, text: String) -> Vec<PanelEvent> {
        self.follow = true;
        let message = UserMessage::text(text);
        if self.is_busy() {
            self.runtime.steer(message);
            self.queued = self.runtime.queue_lens();
            self.notice("queued for the next turn", NoticeKind::Info);
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
                Err(error) => self.notice(format!("cannot start: {error}"), NoticeKind::Error),
            }
        }
        vec![PanelEvent::NeedsRedraw]
    }

    pub fn abort(&mut self) {
        if self.is_busy() {
            self.runtime.abort();
            self.notice("stopping…", NoticeKind::Warn);
        }
    }

    fn notice(&mut self, text: impl Into<String>, kind: NoticeKind) {
        self.transcript.push(Item::Notice {
            text: text.into(),
            kind,
        });
    }

    /// Apply one runtime event to the transcript and the session log.
    /// Note streamed output: enter the generating phase on the first token,
    /// then count characters for the live token estimate and speed.
    fn note_generation(&mut self, chars: usize) {
        let activity = self
            .activity
            .get_or_insert_with(|| Activity::new(Phase::Generating));
        activity.first_token.get_or_insert_with(Instant::now);
        if activity.phase != Phase::Generating {
            activity.enter(Phase::Generating);
        }
        activity.gen_chars += chars;
    }

    /// Switch the current activity to `phase` (starting one if idle).
    fn set_phase(&mut self, phase: Phase) {
        match &mut self.activity {
            Some(activity) => activity.enter(phase),
            None => self.activity = Some(Activity::new(phase)),
        }
    }

    /// The streaming block's live meta line: the same right-aligned zone a
    /// finished block shows, but with the block glyph, the ticking elapsed time
    /// (and a live token estimate while generating) and, in place of the status
    /// check, an animated spinner. Sits after the last block, animating on the
    /// panel's ~10 fps redraw while busy; `None` when idle.
    fn live_footer_line(&self, width: u16) -> Option<Line<'static>> {
        const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let activity = self.activity.as_ref()?;
        let elapsed = activity.since.elapsed();
        let frame = (elapsed.as_millis() / 80) as usize % SPINNER.len();
        let secs = elapsed.as_secs_f32();
        let glyph = match activity.phase {
            Phase::Tool => "⚙\u{fe0f} ",
            _ => "🤖 ",
        };
        let dim = Style::default().fg(self.colors.disabled);
        let mut spans = vec![Span::raw(glyph)];
        if activity.phase == Phase::Generating {
            spans.push(Span::styled(
                format!("{secs:.1}s · {} tok ", activity.est_tokens()),
                dim,
            ));
        } else {
            spans.push(Span::styled(format!("{secs:.1}s "), dim));
        }
        spans.push(Span::styled(
            SPINNER[frame].to_string(),
            Style::default()
                .fg(self.colors.info)
                .add_modifier(Modifier::BOLD),
        ));
        // Right-align to one column short of the scrollbar gutter, like a
        // finished block's meta.
        let content: usize = spans
            .iter()
            .map(|s| termide_ui::str_display_width(&s.content))
            .sum();
        let pad = (width as usize).saturating_sub(content + 1);
        let mut out = vec![Span::raw(" ".repeat(pad))];
        out.extend(spans);
        Some(Line::from(out))
    }

    /// The live phase text for the status bar: the phase, its ticking elapsed
    /// time and, while generating, the estimated speed. `None` when idle.
    fn activity_status(&self) -> Option<String> {
        let activity = self.activity.as_ref()?;
        let elapsed = activity.since.elapsed().as_secs_f32();
        Some(if activity.phase == Phase::Generating {
            let speed = if elapsed > 0.1 {
                (activity.est_tokens() as f32 / elapsed).round() as u64
            } else {
                0
            };
            format!("{} {elapsed:.1}s · {speed} tok/s", activity.phase.label())
        } else {
            format!("{} {elapsed:.1}s", activity.phase.label())
        })
    }

    fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.busy = true,
            AgentEvent::AgentEnd => {
                self.busy = false;
                self.activity = None;
                self.queued = self.runtime.queue_lens();
                if let Some(store) = &self.checkpoints {
                    store.lock().unwrap().end_run();
                }
                if self.prompt_stale {
                    self.sync_system_prompt();
                }
                self.offer_plan();
            }
            AgentEvent::TurnStart | AgentEvent::TurnEnd => {}
            AgentEvent::MessageStart => {
                // The reasoning and answer blocks are created lazily on their
                // first delta, so the reasoning lands above the answer and a
                // prefill with neither shows only the spinner.
                self.activity = Some(Activity::new(Phase::Prefill));
            }
            AgentEvent::MessageUpdate(StreamEvent::TextDelta(delta)) => {
                self.note_generation(delta.chars().count());
                self.transcript.stream_answer(&delta);
            }
            AgentEvent::MessageUpdate(StreamEvent::ThinkingDelta(delta)) => {
                self.note_generation(delta.chars().count());
                self.transcript.stream_thinking(&delta);
            }
            AgentEvent::MessageUpdate(StreamEvent::Retry {
                attempt,
                max_attempts,
                delay_ms,
                error,
            }) => self.notice(
                format!("retry {attempt}/{max_attempts} in {delay_ms} ms: {error}"),
                NoticeKind::Warn,
            ),
            AgentEvent::MessageUpdate(_) => {}
            AgentEvent::MessageEnd(message) => {
                match &message {
                    Message::User(user) => self.transcript.push(Item::User {
                        text: user.plain_text(),
                        at: now_hms(),
                    }),
                    Message::Assistant(assistant) => {
                        if assistant.usage.total() > 0 {
                            self.context_tokens = assistant.usage.total();
                        }
                        self.session_input += assistant.usage.input;
                        self.session_output += assistant.usage.output;
                        let cost = self
                            .activity
                            .as_ref()
                            .map(|a| a.cost(assistant.usage.input, assistant.usage.output));
                        let at = now_hms();
                        let error = assistant.error_message.clone();
                        // The answer always carries the wall-clock time; a
                        // reasoning block, if any, carries the prefill/generation
                        // indicators (else the answer does). A tool-only turn
                        // (reasoning, no answer text) leaves no answer block.
                        let had_thinking = self.transcript.finish_thinking(&at, cost);
                        let answer_cost = if had_thinking { None } else { cost };
                        self.transcript.finish_assistant(
                            assistant.plain_text(),
                            error,
                            answer_cost,
                            at,
                            had_thinking,
                        );
                    }
                    Message::ToolResult(_) => {}
                }
                if let Some(session) = &mut self.session {
                    if let Err(error) = session.append_message(&message) {
                        log::warn!("agent session write failed: {error}");
                    }
                }
            }
            AgentEvent::ToolExecutionStart { call } => {
                self.set_phase(Phase::Tool);
                self.tool_starts.insert(call.id.clone(), Instant::now());
                self.transcript.push(Item::Tool {
                    call,
                    result: None,
                    live: None,
                    at: String::new(),
                    duration_ms: None,
                });
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                update: ToolUpdate::Output(output),
            } => {
                self.transcript.with_tool(&tool_call_id, |item| {
                    if let Item::Tool { live, .. } = item {
                        *live = Some(output);
                    }
                });
            }
            AgentEvent::ToolExecutionEnd { result } => {
                if let Some(path) = changed_file(&result) {
                    self.pending_events
                        .push(PanelEvent::FileChangedOnDisk(path));
                }
                let id = result.tool_call_id.clone();
                let finished = now_hms();
                let elapsed = self
                    .tool_starts
                    .remove(&id)
                    .map(|start| start.elapsed().as_millis() as u32);
                self.transcript.with_tool(&id, |item| {
                    if let Item::Tool {
                        result: slot,
                        live,
                        at,
                        duration_ms,
                        ..
                    } = item
                    {
                        *slot = Some(result);
                        *live = None;
                        *at = finished;
                        *duration_ms = elapsed;
                    }
                });
            }
            AgentEvent::QueueUpdate {
                steering,
                follow_up,
            } => self.queued = (steering, follow_up),
            AgentEvent::CompactionStart { .. } => {
                self.set_phase(Phase::Compact);
                self.notice("compacting the conversation…", NoticeKind::Info)
            }
            AgentEvent::Compacted {
                summary,
                kept,
                tokens_before,
            } => {
                self.notice(
                    format!("compacted {tokens_before} tokens, kept the last {kept} messages"),
                    NoticeKind::Info,
                );
                if let Some(session) = &mut self.session {
                    if let Err(error) = session.append_compaction(&summary, tokens_before, kept) {
                        log::warn!("agent session write failed: {error}");
                    }
                }
            }
            AgentEvent::CompactionFailed { error } => {
                self.notice(format!("compaction failed: {error}"), NoticeKind::Warn)
            }
        }
    }

    fn poll_permissions(&mut self) -> Vec<PanelEvent> {
        let mut events = Vec::new();
        while let Ok(envelope) = self.permission_rx.try_recv() {
            if self.pending.is_some() {
                // Prompts are sequential on the agent thread; a second one
                // cannot arrive before the first is answered. Deny defensively.
                let _ = envelope.reply.send(PermissionAnswer::Deny);
                continue;
            }
            // The question is asked in the panel, not in an app-wide modal:
            // with several panels open a modal does not say who is asking.
            // The status line still announces it for an unfocused panel.
            let request = &envelope.request;
            // MCP tools and others without a path or command have no subject.
            let title = if request.subject.is_empty() {
                format!("Agent wants to run {}", request.tool)
            } else {
                format!("Agent wants to run {}: {}", request.tool, request.subject)
            };
            let options = PERMISSION_OPTIONS
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    if index == 2 {
                        format!("{label} ({})", request.suggested_pattern)
                    } else {
                        label.to_string()
                    }
                })
                .collect();
            events.push(PanelEvent::SetStatusMessage {
                message: title.clone(),
                is_error: false,
            });
            let form = ChoiceForm::new(title, options)
                .with_custom("Deny and tell the agent why")
                .with_cancel("Stop the run");
            self.pending = Some(Pending::Permission { envelope, form });
        }
        events
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
                    format!("cannot open that session: {error}"),
                    NoticeKind::Error,
                );
            }
        }
        true
    }

    /// Answer the outstanding question; `false` when there is none.
    pub fn answer_permission(&mut self, answer: PermissionAnswer) -> bool {
        let Some(Pending::Permission { envelope, .. }) = self.pending.take() else {
            return false;
        };
        let _ = envelope.reply.send(answer);
        true
    }

    /// The system prompt as the agent receives it, written next to the
    /// session logs (or to the temp directory without them) so it can be
    /// opened in a viewer.
    fn write_system_prompt(&self) -> std::io::Result<PathBuf> {
        let dir = self.session_dir.clone().unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("system-prompt.md");
        std::fs::write(&path, self.effective_system_prompt())?;
        Ok(path)
    }

    /// Offer the endpoint's models. The list is fetched off the UI thread
    /// and the picker opens from `tick()` when it arrives; an endpoint that
    /// cannot list models falls back to a typed id.
    fn request_model_list(&mut self) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice("finish or stop the current task first", NoticeKind::Warn);
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
                self.notice(format!("model list unavailable: {error}"), NoticeKind::Info);
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
                    Mode::AcceptEdits => t.agent_mode_accept_edits(),
                    Mode::Auto => t.agent_mode_auto(),
                    Mode::Plan => t.agent_mode_plan(),
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

    /// The prompt the worker runs on: the agent's, plus the plan-mode
    /// instructions while that mode is on.
    fn effective_system_prompt(&self) -> String {
        if self.mode.get() == Mode::Plan {
            self.plan_prompt.apply(&self.system_prompt)
        } else {
            self.system_prompt.clone()
        }
    }

    /// Hand the worker the current effective prompt. During a run the
    /// update is refused; it is retried when the run ends.
    fn sync_system_prompt(&mut self) {
        let prompt = self.effective_system_prompt();
        match self
            .runtime
            .update(Box::new(move |agent| agent.set_system_prompt(prompt)))
        {
            Ok(()) => self.prompt_stale = false,
            Err(PromptError::Busy) => self.prompt_stale = true,
            // An external agent has no prompt of ours to update.
            Err(_) => self.prompt_stale = false,
        }
    }

    /// In plan mode, once the agent has answered: offer to carry the plan
    /// out, in accept-edits or asking, or to keep planning.
    fn offer_plan(&mut self) {
        if self.external || self.mode.get() != Mode::Plan || self.pending.is_some() {
            return;
        }
        let answered = matches!(
            self.transcript.items().last(),
            Some(Item::Assistant { text, error: None, .. }) if !text.trim().is_empty()
        );
        if !answered {
            return;
        }
        let form = ChoiceForm::new(
            "Plan mode: carry the plan out?",
            vec![
                "Yes, accepting edits".into(),
                "Yes, asking before each change".into(),
            ],
        )
        .with_cancel("Keep planning");
        self.pending = Some(Pending::Plan { form });
    }

    /// The plan was accepted: leave plan mode for `mode` and send the
    /// request that carries it out.
    fn carry_out_plan(&mut self, mode: Mode) -> Vec<PanelEvent> {
        let mut events = vec![self.set_mode(mode)];
        let request = self.plan_prompt.request.trim().to_string();
        if request.is_empty() {
            self.notice(
                "system/plan.md names no request: tell the agent to go ahead yourself",
                NoticeKind::Warn,
            );
        } else {
            events.extend(self.send(request));
        }
        events.push(PanelEvent::NeedsRedraw);
        events
    }

    /// Take in tools that finished connecting and hand them to the worker as
    /// soon as it is between runs. `true` when something was shown.
    fn poll_late_tools(&mut self) -> bool {
        let mut arrivals = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.late_tools {
            loop {
                match rx.try_recv() {
                    Ok(event) => arrivals.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.late_tools = None;
        }
        let changed = !arrivals.is_empty();
        for event in arrivals {
            match event {
                LateTools::Ready { source, tools } => {
                    self.notice(
                        format!("mcp {source}: {} tools connected", tools.len()),
                        NoticeKind::Info,
                    );
                    self.waiting_tools.extend(tools);
                }
                LateTools::Failed { source, error } => {
                    self.notice(format!("mcp {source}: {error}"), NoticeKind::Warn);
                }
            }
        }
        if !self.waiting_tools.is_empty() && !self.is_busy() {
            let batch = std::mem::take(&mut self.waiting_tools);
            let for_worker = batch.clone();
            match self.runtime.update(Box::new(move |agent| {
                for tool in for_worker {
                    agent.tools_mut().insert(tool);
                }
            })) {
                Ok(()) => {
                    for tool in batch {
                        self.tools.insert(tool);
                    }
                }
                Err(_) => self.waiting_tools = batch,
            }
        }
        changed
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
        let Some(profile) = self.catalog.resolve(name) else {
            self.notice(format!("no agent named {name}"), NoticeKind::Warn);
            return false;
        };
        // An external agent, or leaving one: the runtime is rebuilt on the
        // same session log, which is replayed into the transcript only.
        if profile.backend.is_some() || self.external {
            if self.is_busy() {
                self.notice("finish or stop the current task first", NoticeKind::Warn);
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
            self.notice(format!("agent: {name}"), NoticeKind::Info);
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
                format!("cannot switch the agent: {error}"),
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
        if let Some(session) = &mut self.session {
            if let Err(error) = session.append_agent_change(name) {
                log::warn!("agent session write failed: {error}");
            }
        }
        if let Some(mode) = profile.mode {
            self.mode.set(mode);
            self.rules.mode = mode;
        }
        self.agent = name.to_string();
        self.notice(format!("agent: {name}"), NoticeKind::Info);
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
                format!("cannot switch the model: {error}"),
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
        // A silent window adoption (same id) leaves no notice.
        if id_changed {
            self.notice(format!("model: {id}"), NoticeKind::Info);
        }
        true
    }

    /// Adopt the active model's real context window from a `list_models`
    /// result, when it is known and differs. Returns whether it changed.
    fn adopt_context_window(&mut self, models: &[ModelInfo]) -> bool {
        if self.is_busy() {
            return false;
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
            self.notice("finish or stop the current task first", NoticeKind::Warn);
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
                format!("cannot change reasoning: {error}"),
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
        self.notice(
            format!("reasoning: {}", if reasoning { "on" } else { "off" }),
            NoticeKind::Info,
        );
        true
    }

    /// Earlier requests of this session, oldest first, repeats collapsed.
    fn history(&self) -> Vec<String> {
        let mut history: Vec<String> = Vec::new();
        for item in self.transcript.items() {
            if let Item::User { text, .. } = item {
                if history.last() != Some(text) {
                    history.push(text.clone());
                }
            }
        }
        history
    }

    /// Show an earlier (`older`) or later request in the input, the way a
    /// shell recalls its history; past the newest, the draft comes back.
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
            Item::Notice { .. } => return vec![PanelEvent::NeedsRedraw],
        };
        if content.trim().is_empty() {
            self.notice("nothing to open yet", NoticeKind::Info);
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
                self.notice(format!("cannot open the block: {error}"), NoticeKind::Error);
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
                "project command".to_string()
            } else {
                format!("{} (project)", script.description)
            };
            items.push(
                CompletionItem::new(script.name.clone())
                    .with_label(format!("/{}", script.name))
                    .with_hint(script.argument_hint)
                    .with_description(description),
            );
        }
        if UNDO_COMMAND.starts_with(prefix) && !self.external {
            items.push(
                CompletionItem::new(UNDO_COMMAND)
                    .with_label(format!("/{UNDO_COMMAND}"))
                    .with_description(
                        "Undo the last request: restore its files, rewind the session",
                    ),
            );
        }
        if COMPACT_COMMAND.starts_with(prefix) && !self.external {
            items.push(
                CompletionItem::new(COMPACT_COMMAND)
                    .with_label(format!("/{COMPACT_COMMAND}"))
                    .with_hint("[focus]")
                    .with_description("Summarise the older part of the session now"),
            );
        }
        if self.session_dir.is_some() {
            if NEW_COMMAND.starts_with(prefix) {
                items.push(
                    CompletionItem::new(NEW_COMMAND)
                        .with_label(format!("/{NEW_COMMAND}"))
                        .with_description("Start a fresh session, keeping the current one"),
                );
            }
            if CLEAR_COMMAND.starts_with(prefix) {
                items.push(
                    CompletionItem::new(CLEAR_COMMAND)
                        .with_label(format!("/{CLEAR_COMMAND}"))
                        .with_description("Discard the current session and start fresh"),
                );
            }
        }
        if items.is_empty() {
            self.completion = None;
            return;
        }
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

    /// Turn what the card reported into an answer. For a permission,
    /// `Cancelled` denies and stops the run: the user wants out, not just a
    /// "no" to this one call. For a command script, the rows are run once,
    /// run for the session, run always (a rule is written) and don't run.
    /// `false` for `NotHandled`.
    fn apply_form_action(&mut self, action: ChoiceAction) -> bool {
        match (&self.pending, action) {
            (_, ChoiceAction::Handled) => {}
            (_, ChoiceAction::NotHandled) => return false,
            (Some(Pending::Permission { .. }), ChoiceAction::Chosen(index)) => {
                self.answer_permission(Self::permission_answer(index));
            }
            (Some(Pending::Permission { .. }), ChoiceAction::Custom(reason)) => {
                self.answer_permission(PermissionAnswer::DenyWithReason(reason));
            }
            (Some(Pending::Permission { .. }), ChoiceAction::Cancelled) => {
                self.answer_permission(PermissionAnswer::Deny);
                self.abort();
            }
            (Some(Pending::Command { .. }), ChoiceAction::Chosen(index)) => {
                let Some(Pending::Command { script, args, .. }) = self.pending.take() else {
                    return true;
                };
                match index {
                    1 => {
                        self.allowed_commands.insert(script.name.clone());
                    }
                    2 => {
                        self.rules.add("command", &script.name, Decision::Allow);
                        if let Some(persist) = self.persist_rule {
                            persist("command", &script.name, Decision::Allow);
                        }
                    }
                    3 => return true,
                    _ => {}
                }
                self.start_command(script, args);
            }
            (Some(Pending::Command { .. }), ChoiceAction::Cancelled | ChoiceAction::Custom(_)) => {
                self.pending = None;
            }
            (Some(Pending::Undo { .. }), ChoiceAction::Chosen(_)) => {
                self.pending = None;
                let events = self.perform_undo();
                self.pending_events.extend(events);
            }
            (Some(Pending::Undo { .. }), ChoiceAction::Cancelled | ChoiceAction::Custom(_)) => {
                self.pending = None;
            }
            (Some(Pending::Plan { .. }), ChoiceAction::Chosen(index)) => {
                self.pending = None;
                let mode = if index == 0 {
                    Mode::AcceptEdits
                } else {
                    Mode::Ask
                };
                let events = self.carry_out_plan(mode);
                self.pending_events.extend(events);
            }
            (Some(Pending::Plan { .. }), ChoiceAction::Cancelled | ChoiceAction::Custom(_)) => {
                self.pending = None;
            }
            (None, _) => {}
        }
        true
    }

    /// Offer to undo the last request: its files go back and the
    /// conversation is rewound to before it.
    fn ask_undo(&mut self) -> Vec<PanelEvent> {
        if self.is_busy() {
            self.notice("finish or stop the current task first", NoticeKind::Warn);
            return vec![PanelEvent::NeedsRedraw];
        }
        let files = self
            .checkpoints
            .as_ref()
            .map(|store| store.lock().unwrap().last_files())
            .unwrap_or_default();
        if files.is_empty() {
            self.notice(
                "nothing to undo: the last request changed no files",
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
        let changed = if names.len() == 1 {
            names[0].clone()
        } else {
            format!("{} files: {}", names.len(), names.join(", "))
        };
        let form = ChoiceForm::new(
            format!("Undo the last request? It changed {changed}"),
            vec!["Restore the files and rewind the conversation".into()],
        )
        .with_cancel("Keep everything");
        self.pending = Some(Pending::Undo { form });
        vec![PanelEvent::NeedsRedraw]
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
                self.notice(format!("cannot undo: {error}"), NoticeKind::Error);
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
        self.notice(
            format!(
                "undid the last request: {count} file{} restored, conversation rewound",
                if count == 1 { "" } else { "s" }
            ),
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
                format!("/{} is denied by the permission rules", script.name),
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
        let title = format!(
            "Run the project command /{} ({})?",
            script.name,
            script.path.display()
        );
        let form = ChoiceForm::new(
            title.clone(),
            vec![
                "Run once".into(),
                "Run for this session".into(),
                "Run always".into(),
                "Don't run".into(),
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
            self.notice("a command is still running", NoticeKind::Warn);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cwd = self.cwd.clone();
        let name = script.name.clone();
        let reported = name.clone();
        std::thread::spawn(move || {
            let outcome = script.run(&args, &cwd);
            let _ = tx.send((reported, outcome));
        });
        self.command_run = Some(rx);
        self.pending_events.push(PanelEvent::SetStatusMessage {
            message: format!("running /{}…", name),
            is_error: false,
        });
    }

    /// Take in a finished command script: its output goes out as a request.
    fn poll_command(&mut self) -> bool {
        let outcome = self.command_run.as_ref().map(Receiver::try_recv);
        match outcome {
            Some(Ok((_, Ok(text)))) => {
                self.command_run = None;
                self.send(text);
                true
            }
            Some(Ok((_, Err(error)))) => {
                self.command_run = None;
                self.notice(error, NoticeKind::Error);
                true
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.command_run = None;
                self.notice("the command was dropped", NoticeKind::Error);
                true
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => false,
        }
    }

    /// The question a permission form's option `index` answers with.
    fn permission_answer(index: usize) -> PermissionAnswer {
        match index {
            0 => PermissionAnswer::AllowOnce,
            1 => PermissionAnswer::AllowSession,
            2 => PermissionAnswer::AllowAlways,
            _ => PermissionAnswer::Deny,
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

    fn input_rows(&self, available: u16) -> u16 {
        let rows = self.input_area().line_count().max(1) as u16;
        rows.min(MAX_INPUT_ROWS)
            .min(available.saturating_sub(2).max(1))
    }

    fn render_input(&mut self, area: Rect, buf: &mut Buffer, focused: bool) {
        let colors = self.colors;
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

/// Token counts as the status line shows them: `32k`, `1.2M`.
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

fn format_tokens(tokens: u64) -> String {
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

/// `/<name> args` at the start of a message: the template name and the
/// rest. A word with further slashes (`/usr/bin`) is text, not a command.
fn slash_command(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty()
        || !name
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
/// as completion items: a relative path each, directories ending in `/`. A
/// shallow, budgeted walk that skips version-control and build noise, so it
/// stays cheap on every keystroke even in a large tree.
fn file_completions(root: &std::path::Path, prefix: &str) -> Vec<CompletionItem> {
    const MAX_RESULTS: usize = 50;
    const MAX_VISITED: usize = 4000;
    /// Directory names never worth offering.
    const SKIP: [&str; 4] = [".git", "target", "node_modules", ".termide"];

    let needle = prefix.to_ascii_lowercase();
    let wants_hidden = prefix.starts_with('.');
    let mut out: Vec<(bool, String)> = Vec::new(); // (name_starts_with, path)
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if visited >= MAX_VISITED {
                break;
            }
            visited += 1;
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP.contains(&name.as_str()) {
                continue;
            }
            if name.starts_with('.') && !wants_hidden {
                continue;
            }
            let path = entry.path();
            let is_dir = path.is_dir();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let mut rel = relative.to_string_lossy().replace('\\', "/");
            if is_dir {
                rel.push('/');
                if stack.len() < MAX_VISITED {
                    stack.push(path.clone());
                }
            }
            let hay = rel.to_ascii_lowercase();
            let name_match = name.to_ascii_lowercase().starts_with(&needle);
            if needle.is_empty() || name_match || hay.contains(&needle) {
                out.push((name_match, rel));
            }
        }
        if visited >= MAX_VISITED {
            break;
        }
    }
    // Name-prefix matches first, then shortest paths, then alphabetical.
    out.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(&b.1))
    });
    out.truncate(MAX_RESULTS);
    out.into_iter()
        .map(|(_, path)| CompletionItem::new(path.clone()).with_label(path))
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
    if let Err(error) = session.append_model_change(provider, &model.id, Some(model.context_window))
    {
        log::warn!("agent session write failed: {error}");
    }
    if let Err(error) = session.append_agent_change(agent) {
        log::warn!("agent session write failed: {error}");
    }
    Some(session)
}

/// The agent `session` last ran as, resolved through `catalog`, when that
/// is not `current`. An agent the log names but no root defines any more is
/// reported and `None` returned, keeping the current one.
fn session_agent(
    catalog: &dyn AgentCatalog,
    current: &str,
    session: Option<&Session>,
) -> Option<(String, AgentProfile)> {
    let name = session.and_then(Session::current_agent)?;
    if name == current {
        return None;
    }
    match catalog.resolve(&name) {
        Some(profile) => Some((name, profile)),
        None => {
            log::warn!("session ran as agent {name}, which no longer exists; using {current}");
            None
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
    persist_rule: Option<PersistFn>,
    extra_hooks: Option<&HooksFactory>,
    backend: Option<&BackendFactory>,
    checkpoints: Option<Arc<Mutex<CheckpointStore>>>,
    autofold: bool,
    session: Option<&Session>,
) -> Spawned {
    let cancel = CancelToken::new();
    let (prompter, permission_rx) = permission_channel(cancel.clone());
    let system_prompt = if rules.mode == Mode::Plan {
        plan_prompt.apply(system_prompt)
    } else {
        system_prompt.to_string()
    };
    let system_prompt = system_prompt.as_str();
    let mut hooks = PermissionHooks::new(rules, Box::new(prompter));
    let mode = hooks.mode_handle();
    if let Some(persist) = persist_rule {
        hooks = hooks.with_persist(Box::new(persist) as PersistRule);
    }

    let mut transcript = Transcript::default();
    transcript.set_autofold(autofold);
    let history = session
        .map(|s| s.context_messages_with_times(compaction_prompts))
        .unwrap_or_default();
    for (message, ts) in &history {
        push_history(&mut transcript, message, hms_from_millis(*ts));
    }
    let messages: Vec<Message> = history.into_iter().map(|(message, _)| message).collect();

    if let Some(factory) = backend {
        // The external agent gets its own prompter on a channel of its own;
        // the permission hooks built above are not used for it.
        let (external_prompter, external_rx) = permission_channel(cancel.clone());
        match factory(BackendSetup {
            cwd: cwd.to_path_buf(),
            prompter: external_prompter,
            cancel: cancel.clone(),
        }) {
            Ok(runtime) => {
                if !messages.is_empty() {
                    transcript.push(Item::Notice {
                        text: "earlier messages are shown but not known to the external agent"
                            .into(),
                        kind: NoticeKind::Info,
                    });
                }
                return Spawned {
                    runtime,
                    permission_rx: external_rx,
                    transcript,
                    mode,
                    external: true,
                };
            }
            Err(error) => transcript.push(Item::Notice {
                text: format!("cannot start the external agent: {error}; using the built-in one"),
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
    .with_messages(messages);
    // Plan mode's guard goes first: nothing, not even a hook's approval,
    // changes a file while it is on. Then the checkpoint recorder, so no
    // call that runs is missed; then the command hooks, which may block or
    // approve before anyone is asked, and whose rewritten arguments are what
    // the rules then judge.
    let mut chain: Vec<Box<dyn Hooks>> = vec![Box::new(PlanGuard::new(mode.clone()))];
    if let Some(store) = checkpoints {
        chain.push(Box::new(CheckpointHooks::new(store)));
    }
    if let Some(factory) = extra_hooks {
        chain.push(factory());
    }
    chain.push(Box::new(hooks));
    let hooks: Box<dyn Hooks> = Box::new(ChainedHooks::new(chain));
    let runtime = AgentRuntime::spawn_with_cancel(agent, hooks, cancel);
    Spawned {
        runtime: Box::new(runtime),
        permission_rx,
        transcript,
        mode,
        external: false,
    }
}

/// Mirror a session's message into transcript items when a session is
/// reopened. `at` is the message's wall-clock time (`HH:MM:SS`), restored from
/// the session log so historical blocks still show when they were written; the
/// per-phase cost is not persisted, so restored answers get no prefill/gen
/// indicators.
fn push_history(transcript: &mut Transcript, message: &Message, at: String) {
    match message {
        Message::User(user) => transcript.push(Item::User {
            text: user.plain_text(),
            at,
        }),
        Message::Assistant(assistant) => {
            // Reasoning is restored as its own block above the tools and answer.
            // When it is present it carries the turn's meta (time), and the
            // answer is left as plain text, matching a live turn. The prefill /
            // generation cost is not persisted, so no `⏫`/`✍️` on resume.
            let thinking = assistant.thinking_text();
            let has_thinking = !thinking.trim().is_empty();
            if has_thinking {
                transcript.push(Item::Thinking {
                    text: thinking,
                    streaming: false,
                    at: at.clone(),
                    cost: None,
                });
            }
            for call in assistant.tool_calls() {
                transcript.push(Item::Tool {
                    call: call.clone(),
                    result: None,
                    live: None,
                    at: at.clone(),
                    duration_ms: None,
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
                    cost: None,
                });
            }
        }
        Message::ToolResult(result) => {
            let id = result.tool_call_id.clone();
            transcript.with_tool(&id, |item| {
                if let Item::Tool {
                    result: slot,
                    at: tool_at,
                    ..
                } = item
                {
                    *slot = Some(result.clone());
                    *tool_at = at;
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
                    Item::User { text, .. } => Some(truncate_title(text)),
                    _ => None,
                })
            })
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned());
        format!("{label}: {subject}")
    }

    fn context_menu_items(&self) -> Vec<(String, &'static str)> {
        let t = termide_i18n::t();
        let mut items = vec![(t.agent_rename().to_string(), RENAME_ACTION)];
        if self.session_dir.is_some() {
            items.push((t.agent_new_session().to_string(), NEW_SESSION_ACTION));
            items.push((t.agent_resume().to_string(), RESUME_ACTION));
        }
        items.push((t.agent_prompts().to_string(), PROMPTS_ACTION));
        items.push((t.agent_change_agent().to_string(), AGENT_ACTION));
        items.push((t.agent_change_model().to_string(), MODEL_ACTION));
        items.push((t.agent_change_mode().to_string(), MODE_ACTION));
        items.push((t.agent_show_prompt().to_string(), SHOW_PROMPT_ACTION));
        if !self.external {
            items.push((t.agent_undo().to_string(), UNDO_ACTION));
        }
        items
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
            MODEL_ACTION | MODE_ACTION if self.external => {
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
                        format!("cannot write the system prompt: {error}"),
                        NoticeKind::Error,
                    );
                    vec![PanelEvent::NeedsRedraw]
                }
            },
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

        let input_rows = self.input_rows(area.height);
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
            .map_or(0, |pending| pending.form().height())
            .min(area.height.saturating_sub(bar_rows + 1));
        let has_separator = form_rows > 0 && area.height > bar_rows + form_rows;
        let transcript_height = area
            .height
            .saturating_sub(bar_rows + form_rows)
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

        // The rightmost column is the scrollbar gutter, so wrapped text never
        // sits under the bar.
        let text_width = area.width.saturating_sub(1).max(1);
        let colors = self.colors;
        let is_light = self.is_light;
        // The streaming block's live meta (ticking time + spinner) sits after
        // the last block while the agent works, animating on the ~10 fps redraw.
        let footer = self.live_footer_line(text_width);
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
            self.selected = self.selected.min(item_count - 1);
            if let Some(first) = self.transcript.first_line_of(self.selected) {
                let height = transcript_height as usize;
                if first < self.top {
                    self.top = first;
                } else if height > 0 && first >= self.top + height {
                    self.top = first + 1 - height;
                }
                let mut last = first;
                while self.transcript.item_at_line(last + 1) == Some(self.selected) {
                    last += 1;
                }
                // The block's first line is its leading rule (or, for a user
                // message, the gap above the plate); leave it out of the
                // highlight so the divider is not inverted.
                selected_range = Some((first + 1, last));
            }
        }
        let lines = self.transcript.lines(text_width, &colors, is_light);
        // The block under the chat cursor is shown inverted (text and
        // background swapped), so the selection reads as one solid block.
        let selected_style = Style::default().fg(colors.bg).bg(colors.fg);
        for row in 0..transcript_height as usize {
            let Some(line) = lines.get(self.top + row) else {
                break;
            };
            buf.set_line(area.x, area.y + row as u16, line, text_width);
            if selected_range.is_some_and(|(f, l)| self.top + row >= f && self.top + row <= l) {
                for dx in 0..text_width {
                    buf[(area.x + dx, area.y + row as u16)].set_style(selected_style);
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
        let key = chord.raw;
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

        // The completion list gets the navigation keys while it is open.
        let completion_action = match &mut self.completion {
            Some(list) if !ctrl && !alt => list.handle_key(key),
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
                    self.selected = self.selected.saturating_sub(1);
                    self.follow = false;
                    return vec![PanelEvent::NeedsRedraw];
                }
                KeyCode::Down if !ctrl => {
                    if self.selected + 1 < count {
                        self.selected += 1;
                    }
                    self.follow = false;
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
        // From the input, Tab moves focus into the chat when there is one.
        if key.code == KeyCode::Tab && !self.transcript.items().is_empty() {
            self.chat_focus = true;
            self.follow = false;
            self.selected = self.transcript.items().len() - 1;
            return vec![PanelEvent::NeedsRedraw];
        }

        match key.code {
            KeyCode::Esc => {
                if self.is_busy() {
                    self.abort();
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
            KeyCode::BackTab if !self.external => {
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
            KeyCode::Up => {
                // Past the first line, the arrow walks back through what was
                // asked before, as in a shell.
                if !self.input_area_mut().move_up() && !self.recall(true) {
                    return vec![];
                }
            }
            KeyCode::Down => {
                if !self.input_area_mut().move_down() && !self.recall(false) {
                    return vec![];
                }
            }
            KeyCode::Left => {
                self.input_area_mut().move_left();
            }
            KeyCode::Right => {
                self.input_area_mut().move_right();
            }
            KeyCode::Home => self.input_area_mut().move_home(),
            KeyCode::End => self.input_area_mut().move_end(),
            KeyCode::Backspace => {
                self.input_area_mut().backspace();
                self.after_edit();
            }
            KeyCode::Delete => {
                self.input_area_mut().delete();
                self.after_edit();
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.input_area_mut().insert(c);
                self.after_edit();
            }
            _ => return vec![],
        }
        vec![PanelEvent::NeedsRedraw]
    }

    fn captures_escape(&self) -> bool {
        self.pending.is_some()
            || self.completion.is_some()
            || self.is_busy()
            || !self.input_area().is_empty()
    }

    fn handle_scroll(&mut self, delta: i32, _panel_area: Rect) -> Vec<PanelEvent> {
        self.scroll_by(delta);
        vec![PanelEvent::NeedsRedraw]
    }

    fn handle_mouse(&mut self, event: MouseEvent, _panel_area: Rect) -> Vec<PanelEvent> {
        match event.kind {
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(pending) = &mut self.pending {
                    let action = pending.form_mut().click(event.column, event.row);
                    if self.apply_form_action(action) {
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
                let area = self.transcript_area;
                let inside = event.column >= area.x
                    && event.column < area.x + area.width
                    && event.row >= area.y
                    && event.row < area.y + area.height;
                if !inside {
                    // A click below the transcript lands on the input: hand focus
                    // back to it so typing resumes.
                    if self.chat_focus {
                        self.chat_focus = false;
                        return vec![PanelEvent::NeedsRedraw];
                    }
                    return vec![];
                }
                let line = self.top + (event.row - area.y) as usize;
                let Some(index) = self.transcript.item_at_line(line) else {
                    return vec![];
                };
                // A click focuses the chat and selects the clicked block; a
                // second click on the block already selected folds/unfolds it.
                if self.chat_focus && self.selected == index {
                    self.transcript.toggle_expanded(index);
                } else {
                    self.chat_focus = true;
                    self.selected = index;
                }
            }
            _ => return vec![],
        }
        vec![PanelEvent::NeedsRedraw]
    }

    fn tick(&mut self) -> Vec<PanelEvent> {
        let mut changed = false;
        for event in self.runtime.drain() {
            self.apply(event);
            changed = true;
        }
        let mut events = self.poll_permissions();
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
                events.push(self.model_picker(Err("the request was dropped".to_string())));
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
        // The silent context-window probe: adopt the active model's real
        // window when it arrives, and stay quiet on failure.
        match self.context_probe.as_ref().map(Receiver::try_recv) {
            Some(Ok(Ok(models))) => {
                self.context_probe = None;
                changed |= self.adopt_context_window(&models);
            }
            Some(Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected)) => {
                self.context_probe = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
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
            PanelCommand::PasteText { text } => {
                self.input_area_mut().insert_str(&text);
                self.after_edit();
                CommandResult::NeedsRedraw(true)
            }
            // Copy the focused chat block. With the input focused instead, let
            // the key fall through (the input has no selection of its own).
            PanelCommand::Copy => match self.chat_focus.then(|| self.selected_block_text()) {
                Some(Some(text)) if !text.trim().is_empty() => {
                    if let Err(error) = termide_ui::clipboard::copy(&text) {
                        log::warn!("agent copy failed: {error}");
                        self.notice("could not copy to the clipboard", NoticeKind::Warn);
                    }
                    CommandResult::Handled(true)
                }
                _ => CommandResult::Handled(false),
            },
            PanelCommand::SelectionMade { action, index } if action == RESUME_ACTION => {
                CommandResult::Handled(self.resume_choice(index))
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
        // segments as given.
        let sep = || StatusSegment::new(" │ ", SegmentKind::Label);
        let mut segments = vec![StatusSegment::new(" ", SegmentKind::Label)];
        if !self.external {
            // An external agent has its own model and permission model.
            segments.extend([
                StatusSegment::clickable("Mode: ", SegmentKind::Label, MODE_ACTION),
                StatusSegment::clickable(self.mode.get().label(), SegmentKind::Active, MODE_ACTION),
            ]);
            // The live phase sits right after the mode, so the phase/speed stays
            // visible even when the bar is truncated on a narrow terminal. Its
            // animated spinner is in the chat block, not here.
            if let Some(text) = self.activity_status() {
                segments.push(sep());
                segments.push(StatusSegment::new(text, SegmentKind::Active));
            } else if self.is_busy() {
                segments.push(sep());
                segments.push(StatusSegment::new("working", SegmentKind::Active));
            }
            segments.extend([
                sep(),
                StatusSegment::clickable("Model: ", SegmentKind::Label, MODEL_ACTION),
                StatusSegment::clickable(self.model.id.clone(), SegmentKind::Active, MODEL_ACTION),
            ]);
            if let Some(endpoint) = self.provider.endpoint() {
                segments.push(StatusSegment::new(
                    format!(" @ {endpoint}"),
                    SegmentKind::Label,
                ));
            }
            // A clickable reasoning toggle: bright when on, dim when off.
            segments.push(sep());
            let reasoning_kind = if self.model.reasoning {
                SegmentKind::Active
            } else {
                SegmentKind::Inactive
            };
            segments.push(StatusSegment::clickable(
                "reasoning",
                reasoning_kind,
                REASONING_ACTION,
            ));
            segments.push(sep());
        }
        segments.extend([
            StatusSegment::clickable("Agent: ", SegmentKind::Label, AGENT_ACTION),
            StatusSegment::clickable(self.agent.clone(), SegmentKind::Active, AGENT_ACTION),
        ]);
        if self.external {
            segments.push(StatusSegment::new(" (acp)", SegmentKind::Label));
        }
        if !self.external && self.model.context_window > 0 {
            segments.push(sep());
            let percent = ((self.context_tokens * 100) / self.model.context_window).min(100);
            let kind = if percent >= 80 {
                SegmentKind::Warn
            } else {
                SegmentKind::Value
            };
            segments.push(StatusSegment::new(
                format!("ctx {} {percent}%", context_bar(percent)),
                kind,
            ));
        }
        // Session token totals: ↑ input (prefill), ↓ output (generated).
        if !self.external && (self.session_input > 0 || self.session_output > 0) {
            segments.push(sep());
            segments.push(StatusSegment::new(
                format!(
                    "↑{} ↓{}",
                    format_tokens(self.session_input),
                    format_tokens(self.session_output)
                ),
                SegmentKind::Value,
            ));
        }
        let queued = self.queued.0 + self.queued.1;
        if queued > 0 {
            segments.push(sep());
            segments.push(StatusSegment::new(
                format!("{queued} queued"),
                SegmentKind::Inactive,
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
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use termide_agent_core::PermissionPrompter;
    use termide_agent_core::{AssistantContent, AssistantMessage, Request, StopReason, Usage};
    use termide_core::PanelConfig;

    /// Replays one scripted assistant message per model call and records
    /// which model each call asked for.
    struct Scripted {
        replies: Mutex<Vec<AssistantMessage>>,
        models: Result<Vec<ModelInfo>, String>,
        seen_models: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(replies: Vec<AssistantMessage>) -> Self {
            Self {
                replies: Mutex::new(replies),
                models: Ok(vec![
                    ModelInfo {
                        id: "big".into(),
                        context_window: Some(64_000),
                    },
                    ModelInfo {
                        id: "m".into(),
                        context_window: None,
                    },
                ]),
                seen_models: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn stream(
            &self,
            request: &Request<'_>,
            on_event: &mut dyn FnMut(StreamEvent),
            _cancel: &CancelToken,
        ) -> AssistantMessage {
            self.seen_models
                .lock()
                .unwrap()
                .push(request.model.id.clone());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return AssistantMessage::failed("scripted", "m", StopReason::Error, "exhausted");
            }
            let reply = replies.remove(0);
            let thinking = reply.thinking_text();
            if !thinking.is_empty() {
                on_event(StreamEvent::ThinkingDelta(thinking));
            }
            on_event(StreamEvent::TextDelta(reply.plain_text()));
            reply
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, String> {
            self.models.clone()
        }
    }

    fn reply_thinking(text: &str, thinking: &str) -> AssistantMessage {
        let mut message = reply(text);
        message.content.insert(
            0,
            AssistantContent::Thinking {
                text: thinking.into(),
            },
        );
        message
    }

    fn reply(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text { text: text.into() }],
            stop_reason: StopReason::Stop,
            usage: Usage {
                input: 100,
                output: 20,
                cache_read: 0,
                cache_write: 0,
            },
            provider: "scripted".into(),
            model: "m".into(),
            error_message: None,
            timestamp: 0,
        }
    }

    fn panel(replies: Vec<AssistantMessage>) -> AgentPanel {
        AgentPanel::new(setup(replies))
    }

    fn setup(replies: Vec<AssistantMessage>) -> AgentPanelSetup {
        setup_with(Arc::new(Scripted::new(replies)))
    }

    /// Command scripts the test catalog offers; tests fill it.
    static COMMANDS: Mutex<Vec<CommandScript>> = Mutex::new(Vec::new());

    /// Two agents: the default one and a terse reviewer on another model.
    struct Agents;

    impl AgentCatalog for Agents {
        fn list(&self) -> Vec<AgentEntry> {
            vec![
                AgentEntry {
                    name: "default".into(),
                    description: String::new(),
                },
                AgentEntry {
                    name: "review".into(),
                    description: "Reviews diffs".into(),
                },
                AgentEntry {
                    name: "outside".into(),
                    description: "An external agent".into(),
                },
            ]
        }
        fn prompts(&self) -> Vec<PromptTemplate> {
            vec![PromptTemplate {
                name: "review".into(),
                description: "Review a file".into(),
                argument_hint: "<path>".into(),
                body: "Review $1 carefully.".into(),
            }]
        }
        fn commands(&self) -> Vec<CommandScript> {
            COMMANDS.lock().unwrap().clone()
        }
        fn resolve(&self, name: &str) -> Option<AgentProfile> {
            match name {
                "default" => Some(AgentProfile {
                    system_prompt: "default prompt".into(),
                    tools: ToolRegistry::new(),
                    model: None,
                    mode: None,
                    late_tools: None,
                    backend: None,
                }),
                "review" => Some(AgentProfile {
                    system_prompt: "You review diffs.".into(),
                    tools: ToolRegistry::new(),
                    model: Some("big".into()),
                    mode: Some(Mode::AcceptEdits),
                    late_tools: None,
                    backend: None,
                }),
                "outside" => Some(AgentProfile {
                    system_prompt: String::new(),
                    tools: ToolRegistry::new(),
                    model: None,
                    mode: None,
                    late_tools: None,
                    backend: Some(Arc::new(|setup: BackendSetup| {
                        Ok(Box::new(External::new(setup)) as Box<dyn Backend>)
                    })),
                }),
                _ => None,
            }
        }
    }

    fn setup_with(provider: Arc<Scripted>) -> AgentPanelSetup {
        AgentPanelSetup {
            cwd: PathBuf::from("/tmp"),
            agent: "default".into(),
            catalog: Arc::new(Agents),
            late_tools: None,
            hooks: None,
            backend: None,
            provider,
            provider_kind: "openai_compatible".into(),
            model: ModelSpec {
                provider: "scripted".into(),
                id: "m".into(),
                context_window: 1000,
                max_tokens: 100,
                reasoning: false,
            },
            tools: ToolRegistry::new(),
            rules: PermissionRules::default(),
            system_prompt: String::new(),
            compaction: CompactionPolicy::default(),
            compaction_prompts: CompactionPrompts::default(),
            plan_prompt: PlanPrompt::default(),
            persist_rule: None,
            session_dir: None,
            session: None,
            autofold: true,
        }
    }

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> KeyChord {
        let event = KeyEvent::new(code, modifiers);
        KeyChord {
            raw: event,
            canonical: event,
        }
    }

    fn type_text(panel: &mut AgentPanel, text: &str) {
        for c in text.chars() {
            panel.handle_key(chord(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn settle(panel: &mut AgentPanel) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            panel.tick();
            if !panel.is_busy() {
                return;
            }
            assert!(Instant::now() < deadline, "agent did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Tick until an event matching `wanted` arrives, returning it.
    fn wait_for(panel: &mut AgentPanel, wanted: fn(&PanelEvent) -> bool) -> PanelEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = panel.tick().into_iter().find(&wanted) {
                return event;
            }
            assert!(Instant::now() < deadline, "event did not arrive");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Text of the bold chip that carries `action`.
    fn chip(panel: &AgentPanel, action: &str) -> String {
        panel
            .status_segments()
            .into_iter()
            .find(|s| s.action == Some(action) && s.kind == SegmentKind::Active)
            .map(|s| s.text)
            .expect("chip present")
    }

    fn select(panel: &mut AgentPanel, event: &PanelEvent, index: usize) -> CommandResult {
        let PanelEvent::ShowSelect {
            on_select: SelectAction::Custom(action),
            ..
        } = event
        else {
            panic!("expected a picker, got {event:?}");
        };
        panel.handle_command(PanelCommand::SelectionMade {
            action: action.clone(),
            index,
        })
    }

    fn render_text(panel: &mut AgentPanel, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let colors = ThemeColors::default();
        let config = PanelConfig {
            tab_size: 4,
            word_wrap: false,
            show_line_numbers: false,
            show_hidden_files: false,
        };
        let ctx = RenderContext {
            theme: &colors,
            config: &config,
            is_focused: true,
            panel_index: 0,
            terminal_width: width,
            terminal_height: height,
            border_right_x: None,
            border_bottom_y: None,
        };
        panel.render(area, &mut buf, &ctx);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn typing_enter_runs_a_turn_and_renders_it() {
        let mut panel = panel(vec![reply("Hello from the model")]);
        type_text(&mut panel, "hi there");
        assert_eq!(panel.input_text(), "hi there");
        assert!(panel.captures_escape(), "non-empty input keeps Esc");

        let events = panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert!(events.iter().any(|e| matches!(e, PanelEvent::NeedsRedraw)));
        assert!(panel.input_text().is_empty());
        settle(&mut panel);

        let items = panel.transcript().items();
        assert!(matches!(&items[0], Item::User { text, .. } if text == "hi there"));
        assert!(items.iter().any(|item| matches!(
            item,
            Item::Assistant { text, streaming: false, .. } if text == "Hello from the model"
        )));
        let rows = render_text(&mut panel, 40, 16);
        assert!(rows.iter().any(|r| r.contains("› hi there")));
        assert!(rows.iter().any(|r| r.contains("Hello from the model")));
        assert!(
            rows.last().unwrap().starts_with("›"),
            "input box at the bottom"
        );
        assert!(
            rows[rows.len() - 2].starts_with("─"),
            "separator above the input"
        );

        let chips: String = panel
            .status_segments()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(
            chips,
            " Mode: ask │ Model: m │ reasoning │ Agent: default │ ctx ▰▱▱▱▱▱▱▱ 12% │ ↑100 ↓20"
        );
    }

    #[test]
    fn title_follows_the_first_prompt() {
        let mut fresh = panel(vec![reply("ok")]);
        // Empty conversation: the working directory.
        assert_eq!(fresh.title(), "Agent: /tmp");

        type_text(&mut fresh, "  make the   timeout configurable  ");
        fresh.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut fresh);
        assert_eq!(fresh.title(), "Agent: make the timeout configurable");

        // A long prompt is cut with an ellipsis.
        let mut wordy = panel(vec![reply("ok")]);
        type_text(&mut wordy, &"word ".repeat(30));
        wordy.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut wordy);
        let title = wordy.title();
        assert!(title.ends_with('…'), "{title}");
        assert_eq!(title.chars().count(), "Agent: ".len() + MAX_TITLE_CHARS);
    }

    #[test]
    fn the_context_window_is_learned_from_the_provider() {
        let mut panel = panel(vec![reply("ok")]);
        // The configured window is only a fallback until the provider is known.
        assert_eq!(panel.model.context_window, 1000);
        // The provider reports the active model's real window; the panel always
        // adopts it, overriding the fallback.
        let models = vec![ModelInfo {
            id: panel.model.id.clone(),
            context_window: Some(48_000),
        }];
        assert!(panel.adopt_context_window(&models));
        assert_eq!(panel.model.context_window, 48_000);
        // A second identical report is a no-op.
        assert!(!panel.adopt_context_window(&models));
    }

    #[test]
    fn a_model_without_a_reported_window_keeps_the_fallback() {
        // When the provider reports no window for the active model, the
        // configured fallback stays.
        let mut panel = panel(vec![reply("ok")]);
        let models = vec![ModelInfo {
            id: panel.model.id.clone(),
            context_window: None,
        }];
        assert!(!panel.adopt_context_window(&models));
        assert_eq!(panel.model.context_window, 1000);
    }

    #[test]
    fn an_empty_session_is_discarded_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let panel = AgentPanel::new(AgentPanelSetup {
                session_dir: Some(dir.path().to_path_buf()),
                ..setup(vec![reply("ok")])
            });
            let path = panel.session.as_ref().unwrap().path().to_path_buf();
            assert!(path.exists());
            path
        };
        assert!(!path.exists(), "an unused session is removed on close");
    }

    #[test]
    fn a_session_with_messages_survives_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let mut panel = AgentPanel::new(AgentPanelSetup {
                session_dir: Some(dir.path().to_path_buf()),
                ..setup(vec![reply("ok")])
            });
            let path = panel.session.as_ref().unwrap().path().to_path_buf();
            type_text(&mut panel, "do something");
            panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
            settle(&mut panel);
            assert!(path.exists());
            path
        };
        assert!(path.exists(), "a session with a conversation is kept");
    }

    #[test]
    fn switching_away_from_an_empty_session_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("ok")])
        });
        let empty = panel.session.as_ref().unwrap().path().to_path_buf();
        assert!(empty.exists());
        // A new session: the empty one we leave is discarded, not listed.
        assert!(panel.switch_session(None));
        assert!(!empty.exists(), "the empty session is removed on switch");
        let fresh = panel.session.as_ref().unwrap().path().to_path_buf();
        assert!(fresh.exists());
        assert_ne!(empty, fresh);
    }

    #[test]
    fn the_context_bar_fills_with_the_percentage() {
        assert_eq!(context_bar(0), "▱▱▱▱▱▱▱▱");
        assert_eq!(context_bar(12), "▰▱▱▱▱▱▱▱");
        assert_eq!(context_bar(50), "▰▰▰▰▱▱▱▱");
        assert_eq!(context_bar(100), "▰▰▰▰▰▰▰▰");
        assert_eq!(context_bar(200), "▰▰▰▰▰▰▰▰");
    }

    #[test]
    fn activity_follows_the_events_and_totals_accumulate() {
        let mut panel = panel(vec![reply("hi")]);
        assert!(panel.activity.is_none());
        panel.apply(AgentEvent::AgentStart);
        panel.apply(AgentEvent::MessageStart);
        assert_eq!(panel.activity.map(|a| a.phase), Some(Phase::Prefill));
        panel.apply(AgentEvent::MessageUpdate(StreamEvent::TextDelta(
            "hello".into(),
        )));
        assert_eq!(panel.activity.map(|a| a.phase), Some(Phase::Generating));
        panel.apply(AgentEvent::MessageEnd(Message::Assistant(reply("hello"))));
        // reply()'s usage is input 100 / output 20.
        assert_eq!((panel.session_input, panel.session_output), (100, 20));
        panel.apply(AgentEvent::ToolExecutionStart {
            call: termide_agent_core::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
            },
        });
        assert_eq!(panel.activity.map(|a| a.phase), Some(Phase::Tool));
        panel.apply(AgentEvent::AgentEnd);
        assert!(panel.activity.is_none());
    }

    #[test]
    fn the_session_records_the_provider_kind() {
        let dir = tempfile::tempdir().unwrap();
        let panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            provider_kind: "anthropic_compatible".into(),
            ..setup(vec![reply("ok")])
        });
        let recorded = panel.session.as_ref().unwrap().current_model().unwrap();
        assert_eq!(recorded.provider, "anthropic_compatible");
    }

    #[test]
    fn reasoning_toggles_from_the_chip_and_persists_in_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("ok")])
        });
        assert!(!panel.model.reasoning);
        let path = panel.session.as_ref().unwrap().path().to_path_buf();

        panel.handle_status_action(REASONING_ACTION);
        assert!(panel.model.reasoning, "the chip turns reasoning on");

        // The choice is written to the session, so a resume brings it back.
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.current_reasoning(), Some(true));
        assert!(session_model(&panel.configured_model, Some(&reopened)).reasoning);
    }

    #[test]
    fn a_click_focuses_the_chat_and_selects_a_block() {
        let mut panel = panel(vec![reply("Hello from the model")]);
        type_text(&mut panel, "hi there");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        // Render so the transcript area and its lines exist.
        let _ = render_text(&mut panel, 40, 12);
        assert!(!panel.chat_focus, "starts on the input");
        let area = panel.transcript_area;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        panel.handle_mouse(click, area);
        assert!(panel.chat_focus, "a click focuses the chat");
        assert!(panel
            .selected_block_text()
            .is_some_and(|t| !t.trim().is_empty()));

        // A click below the transcript (on the input) hands focus back.
        let below = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y + area.height + 1,
            modifiers: KeyModifiers::NONE,
        };
        panel.handle_mouse(below, area);
        assert!(
            !panel.chat_focus,
            "a click on the input returns focus to it"
        );
    }

    #[test]
    fn a_resumed_block_shows_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), dir.path()).unwrap();
        session
            .append_message(&Message::User(UserMessage::text("earlier question")))
            .unwrap();
        session
            .append_message(&Message::Assistant(reply("earlier answer")))
            .unwrap();
        let path = session.path().to_path_buf();
        // Reopen: the restored blocks carry the wall-clock time from the log.
        let session = Session::open(&path).unwrap();
        let mut transcript = Transcript::default();
        for (message, ts) in &session.context_messages_with_times(&CompactionPrompts::default()) {
            push_history(&mut transcript, message, hms_from_millis(*ts));
        }
        let has_time = transcript
            .items()
            .iter()
            .any(|item| matches!(item, Item::Assistant { at, .. } if !at.is_empty()));
        assert!(has_time, "a resumed answer keeps its time");
    }

    #[test]
    fn a_custom_agent_names_the_title() {
        let mut panel = panel(vec![reply("ok")]);
        // The default agent shows the localized "Agent" label.
        assert_eq!(panel.title(), "Agent: /tmp");
        // A custom agent replaces the label with its own capitalized name.
        assert!(panel.switch_agent("review"));
        assert_eq!(panel.title(), "Review: /tmp");
    }

    #[test]
    fn a_named_session_titles_the_panel() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("ok")])
        });

        type_text(&mut panel, "first request");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(panel.title(), "Agent: first request");

        // The context menu raises an input prompt carrying our action.
        let (label, action) = panel.context_menu_items().remove(0);
        assert_eq!(label, "Rename session");
        let events = panel.handle_status_action(action);
        let Some(PanelEvent::ShowInput { on_submit, .. }) = events.first() else {
            panic!("expected an input prompt, got {events:?}");
        };
        let InputAction::Custom(submit_action) = on_submit else {
            panic!("expected a custom action");
        };
        assert_eq!(submit_action, RENAME_ACTION);

        // The submitted name wins over the first prompt and survives a reopen.
        let result = panel.handle_command(PanelCommand::InputSubmitted {
            action: submit_action.clone(),
            text: "  timeout work  ".into(),
        });
        assert!(matches!(result, CommandResult::Handled(true)));
        assert_eq!(panel.title(), "Agent: timeout work");
        let path = panel.session_path().unwrap().to_path_buf();
        drop(panel);
        assert_eq!(Session::open(&path).unwrap().name(), Some("timeout work"));

        // Without a session log there is nothing to record the name in.
        let mut logless = AgentPanel::new(setup(vec![]));
        assert!(!logless.rename_session("x"));
    }

    #[test]
    fn sessions_can_be_listed_switched_and_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("one"), reply("two")])
        });
        // A session is created eagerly, so the log exists before the first
        // prompt.
        let first_path = panel.session_path().unwrap().to_path_buf();
        type_text(&mut panel, "first task");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);

        // "New session" starts an empty one and leaves the old log alone.
        let items = panel.context_menu_items();
        let labels: Vec<&str> = items.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Rename session",
                "New session",
                "Open session",
                "Insert prompt…",
                "Change agent…",
                "Change model…",
                "Permission mode",
                "Show system prompt",
                "Undo last request"
            ]
        );
        panel.handle_status_action(NEW_SESSION_ACTION);
        assert!(panel.transcript().items().is_empty());
        assert_ne!(panel.session_path().unwrap(), first_path);
        assert_eq!(panel.title(), "Agent: /tmp");

        // The picker lists both, newest first, marking the current one.
        let events = panel.handle_status_action(RESUME_ACTION);
        let Some(PanelEvent::ShowSelect {
            options, on_select, ..
        }) = events.first()
        else {
            panic!("expected a picker, got {events:?}");
        };
        assert_eq!(options.len(), 2);
        assert!(options[0].starts_with("● "), "{:?}", options[0]);
        assert!(options[1].contains("first task"), "{:?}", options[1]);
        let SelectAction::Custom(action) = on_select else {
            panic!("expected a custom action");
        };

        // Choosing the older one replays its transcript into the panel.
        let result = panel.handle_command(PanelCommand::SelectionMade {
            action: action.clone(),
            index: 1,
        });
        assert!(matches!(result, CommandResult::Handled(true)));
        assert_eq!(panel.session_path().unwrap(), first_path);
        assert_eq!(panel.title(), "Agent: first task");
        let items = panel.transcript().items();
        assert!(matches!(&items[0], Item::User { text, .. } if text == "first task"));
        assert!(items
            .iter()
            .any(|item| matches!(item, Item::Assistant { text, .. } if text == "one")));

        // The resumed agent keeps the old messages as context.
        type_text(&mut panel, "second task");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        let reopened = Session::open(&first_path).unwrap();
        assert_eq!(
            roles(&reopened.context_messages()),
            vec!["user", "assistant", "user", "assistant"]
        );
    }

    #[test]
    fn slash_new_starts_a_fresh_session_and_keeps_the_old() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("one")])
        });
        let first_path = panel.session_path().unwrap().to_path_buf();
        type_text(&mut panel, "first task");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);

        // /new opens a fresh session; the used one is left on disk to resume.
        type_text(&mut panel, "/new");
        panel.submit();
        assert!(panel.transcript().items().is_empty());
        assert_ne!(panel.session_path().unwrap(), first_path);
        assert!(first_path.exists(), "the previous session log is kept");
        assert_eq!(panel.session_list().len(), 2);
    }

    #[test]
    fn slash_clear_discards_the_current_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("one")])
        });
        let first_path = panel.session_path().unwrap().to_path_buf();
        type_text(&mut panel, "first task");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);

        // /clear deletes the current session and starts a fresh one, so there
        // is nothing to resume back to: only the new empty session remains.
        type_text(&mut panel, "/clear");
        panel.submit();
        assert!(panel.transcript().items().is_empty());
        assert_ne!(panel.session_path().unwrap(), first_path);
        assert!(!first_path.exists(), "the previous session log is removed");
        assert_eq!(panel.session_list().len(), 1);
    }

    fn roles(messages: &[Message]) -> Vec<&'static str> {
        messages
            .iter()
            .map(|m| match m {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "tool_result",
            })
            .collect()
    }

    #[test]
    fn shift_enter_adds_a_line_and_esc_clears() {
        let mut panel = panel(vec![]);
        type_text(&mut panel, "one");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut panel, "two");
        assert_eq!(panel.input_text(), "one\ntwo");
        let rows = render_text(&mut panel, 30, 6);
        assert!(rows[4].starts_with("› one"));
        assert!(rows[5].starts_with("  two"));

        panel.handle_key(chord(KeyCode::Esc, KeyModifiers::NONE));
        assert!(panel.input_text().is_empty());
        assert!(!panel.captures_escape());
        assert!(panel
            .handle_key(chord(KeyCode::Esc, KeyModifiers::NONE))
            .is_empty());
    }

    #[test]
    fn permission_prompt_is_answered_in_the_panel() {
        let mut panel = panel(vec![]);
        let (mut prompter, rx) = permission_channel(CancelToken::new());
        panel.permission_rx = rx;
        let worker = std::thread::spawn(move || {
            prompter.ask(&termide_agent_core::PermissionRequest {
                tool: "bash".into(),
                subject: "git push".into(),
                call: termide_agent_core::ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": "git push" }),
                },
                suggested_pattern: "git push *".into(),
            })
        });

        // The question arrives as a form in the panel and a status line.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = panel.tick();
            if panel.pending.is_some() {
                assert!(events.iter().any(|e| matches!(
                    e,
                    PanelEvent::SetStatusMessage { message, .. } if message.contains("git push")
                )));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        {
            let form = panel.pending.as_ref().unwrap().form();
            assert_eq!(form.title(), "Agent wants to run bash: git push");
            assert_eq!(form.options()[2], "Allow always (git push *)");
        }
        assert!(panel.captures_escape());
        let rows = render_text(&mut panel, 60, 14);
        assert!(
            rows.iter().any(|r| r.contains("2. Allow for this session")),
            "{rows:?}"
        );
        // Typing goes nowhere while the question is open; Down + Enter answer it.
        type_text(&mut panel, "x");
        assert_eq!(panel.input_text(), "");
        panel.handle_key(chord(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel.pending.is_none());
        assert_eq!(worker.join().unwrap(), PermissionAnswer::AllowSession);

        // A digit answers at once; Esc declines.
        let (mut prompter, rx) = permission_channel(CancelToken::new());
        panel.permission_rx = rx;
        let request = termide_agent_core::PermissionRequest {
            tool: "edit".into(),
            subject: "src/x.rs".into(),
            call: termide_agent_core::ToolCall {
                id: "c".into(),
                name: "edit".into(),
                arguments: serde_json::json!({ "path": "src/x.rs" }),
            },
            suggested_pattern: "src/x.rs".into(),
        };
        let asked = request.clone();
        let worker = std::thread::spawn(move || {
            let first = prompter.ask(&asked);
            let second = prompter.ask(&asked);
            (first, second)
        });
        let wait = |panel: &mut AgentPanel| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while panel.pending.is_none() {
                panel.tick();
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut panel);
        panel.handle_key(chord(KeyCode::Char('3'), KeyModifiers::NONE));
        // The fifth row takes a reason the model gets to read.
        wait(&mut panel);
        {
            let form = panel.pending.as_ref().unwrap().form();
            assert_eq!(
                form.height(),
                8,
                "four answers, a reason row and a stop row"
            );
        }
        panel.handle_key(chord(KeyCode::Char('5'), KeyModifiers::NONE));
        type_text(&mut panel, "edit the test instead");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            worker.join().unwrap(),
            (
                PermissionAnswer::AllowAlways,
                PermissionAnswer::DenyWithReason("edit the test instead".into())
            )
        );
        let _ = request;
    }
    #[test]
    fn mode_switches_from_the_chip_and_with_shift_tab() {
        let mut panel = panel(vec![]);
        let hooks_mode = panel.mode.clone();
        assert_eq!(chip(&panel, MODE_ACTION), "ask");

        // Shift+Tab cycles and reports the new mode in the status line.
        let events = panel.handle_key(chord(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(events.iter().any(|e| matches!(
            e,
            PanelEvent::SetStatusMessage { message, .. } if message.ends_with("accept-edits")
        )));
        assert_eq!(chip(&panel, MODE_ACTION), "accept-edits");
        assert_eq!(hooks_mode.get(), Mode::AcceptEdits);

        // The chip opens a picker with the current mode marked.
        let events = panel.handle_status_action(MODE_ACTION);
        let picker = events.first().expect("picker");
        let PanelEvent::ShowSelect { options, .. } = picker else {
            panic!("expected a picker, got {picker:?}");
        };
        assert_eq!(options.len(), 4);
        assert!(options[1].starts_with("● accept-edits"), "{:?}", options[1]);
        assert!(options[3].starts_with("  plan"), "{:?}", options[3]);
        assert!(matches!(
            select(&mut panel, picker, 2),
            CommandResult::Handled(true)
        ));
        assert_eq!(chip(&panel, MODE_ACTION), "auto");
        assert_eq!(hooks_mode.get(), Mode::Auto);
        let events = panel.tick();
        assert!(events.iter().any(|e| matches!(
            e,
            PanelEvent::SetStatusMessage { message, .. } if message.ends_with("auto")
        )));

        // Cycling passes plan and wraps, and a rebuilt agent starts in the
        // chosen mode.
        panel.handle_key(chord(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(chip(&panel, MODE_ACTION), "plan");
        panel.handle_key(chord(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(chip(&panel, MODE_ACTION), "ask");
        panel.handle_key(chord(KeyCode::BackTab, KeyModifiers::SHIFT));
        panel.switch_session(None);
        assert_eq!(chip(&panel, MODE_ACTION), "accept-edits");
        assert_eq!(panel.mode.get(), Mode::AcceptEdits);
    }

    #[test]
    fn model_switches_are_recorded_and_followed_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(Scripted::new(vec![reply("one"), reply("two")]));
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup_with(Arc::clone(&provider))
        });
        let first_path = panel.session_path().unwrap().to_path_buf();
        // A fresh session records the model it starts on.
        assert_eq!(
            Session::open(&first_path).unwrap().current_model(),
            Some(termide_agent_core::SessionModel {
                provider: "openai_compatible".into(),
                id: "m".into(),
                context_window: Some(1000),
            })
        );
        assert_eq!(chip(&panel, MODEL_ACTION), "m");

        // The chip fetches the list off-thread; the picker marks the current
        // model and ends with the typed-id entry.
        let events = panel.handle_status_action(MODEL_ACTION);
        assert!(matches!(
            events.first(),
            Some(PanelEvent::SetStatusMessage { .. })
        ));
        let picker = wait_for(&mut panel, |e| matches!(e, PanelEvent::ShowSelect { .. }));
        let PanelEvent::ShowSelect { options, .. } = &picker else {
            unreachable!()
        };
        assert_eq!(options, &["  big", "● m", "  Enter a model id…"]);
        select(&mut panel, &picker, 0);
        assert_eq!(chip(&panel, MODEL_ACTION), "big");
        // The endpoint's context window comes along with the id.
        assert_eq!(panel.model.context_window, 64_000);
        assert!(panel
            .transcript()
            .items()
            .iter()
            .any(|item| matches!(item, Item::Notice { text, .. } if text == "model: big")));

        // The next run goes to the new model.
        type_text(&mut panel, "go");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(
            *provider.seen_models.lock().unwrap(),
            vec!["big".to_string()]
        );

        // A new session starts on the current model; the last picker entry
        // asks for an id by hand.
        panel.handle_status_action(NEW_SESSION_ACTION);
        assert_eq!(chip(&panel, MODEL_ACTION), "big");
        panel.handle_status_action(MODEL_ACTION);
        let picker = wait_for(&mut panel, |e| matches!(e, PanelEvent::ShowSelect { .. }));
        select(&mut panel, &picker, 2);
        let input = wait_for(&mut panel, |e| matches!(e, PanelEvent::ShowInput { .. }));
        let PanelEvent::ShowInput {
            initial_value,
            on_submit: InputAction::Custom(action),
            ..
        } = input
        else {
            panic!("expected an input prompt, got {input:?}");
        };
        assert_eq!(initial_value, "big");
        panel.handle_command(PanelCommand::InputSubmitted {
            action,
            text: " typed ".to_string(),
        });
        assert_eq!(chip(&panel, MODEL_ACTION), "typed");
        // A typed id keeps the window of the model it replaced.
        assert_eq!(panel.model.context_window, 64_000);

        // Reopening the first session returns to the model it last used.
        panel.session_choices = panel.session_list();
        let index = panel
            .session_choices
            .iter()
            .position(|s| s.path == first_path)
            .unwrap();
        panel.resume_choice(index);
        assert_eq!(chip(&panel, MODEL_ACTION), "big");
        // The window comes back from the log too.
        assert_eq!(panel.model.context_window, 64_000);
    }

    #[test]
    fn model_picker_falls_back_to_a_typed_id() {
        let mut provider = Scripted::new(vec![]);
        provider.models = Err("HTTP 404: no such route".into());
        let mut panel = AgentPanel::new(setup_with(Arc::new(provider)));
        panel.handle_status_action(MODEL_ACTION);
        let input = wait_for(&mut panel, |e| matches!(e, PanelEvent::ShowInput { .. }));
        assert!(
            matches!(input, PanelEvent::ShowInput { initial_value, .. } if initial_value == "m")
        );
        assert!(panel.transcript().items().iter().any(|item| matches!(
            item,
            Item::Notice { text, .. } if text.contains("HTTP 404")
        )));
    }
    #[test]
    fn saved_state_names_the_directory_and_the_session_log() {
        let no_log = panel(vec![]);
        assert_eq!(
            no_log.to_state(Path::new("/unused")),
            Some(termide_core::PanelState::Agent {
                cwd: PathBuf::from("/tmp"),
                session: None,
                agent: None,
            })
        );

        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("done")])
        });
        type_text(&mut panel, "task");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        let Some(termide_core::PanelState::Agent { cwd, session, .. }) =
            panel.to_state(Path::new("/unused"))
        else {
            panic!("agent state expected");
        };
        assert_eq!(cwd, PathBuf::from("/tmp"));
        let session = session.expect("session path");
        assert_eq!(panel.session_path(), Some(session.as_path()));

        // Rebuilding from that state brings the conversation back.
        let restored = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            session: Some(Session::open(&session).unwrap()),
            ..setup(vec![])
        });
        assert_eq!(restored.title(), "Agent: task");
        assert_eq!(restored.session_path(), Some(session.as_path()));
    }

    #[test]
    fn a_successful_edit_reports_the_changed_file() {
        use termide_agent_core::ToolCall;
        let mut panel = panel(vec![]);
        let call = |name: &str| ToolCall {
            id: format!("{name}-1"),
            name: name.into(),
            arguments: serde_json::json!({}),
        };
        let changed = |events: &[PanelEvent]| -> Vec<PathBuf> {
            events
                .iter()
                .filter_map(|e| match e {
                    PanelEvent::FileChangedOnDisk(path) => Some(path.clone()),
                    _ => None,
                })
                .collect()
        };

        let edit = call("edit");
        panel.apply(AgentEvent::ToolExecutionStart { call: edit.clone() });
        panel.apply(AgentEvent::ToolExecutionEnd {
            result: ToolResultMessage::text(&edit, "Edited")
                .with_details(serde_json::json!({ "path": "/tmp/f.rs", "replacements": 1 })),
        });
        assert_eq!(changed(&panel.tick()), vec![PathBuf::from("/tmp/f.rs")]);

        // A failed edit, a read and a shell command report nothing.
        panel.apply(AgentEvent::ToolExecutionEnd {
            result: ToolResultMessage::error(&edit, "no match")
                .with_details(serde_json::json!({ "path": "/tmp/f.rs" })),
        });
        let read = call("read");
        panel.apply(AgentEvent::ToolExecutionEnd {
            result: ToolResultMessage::text(&read, "…")
                .with_details(serde_json::json!({ "path": "/tmp/g.rs" })),
        });
        let bash = call("bash");
        panel.apply(AgentEvent::ToolExecutionEnd {
            result: ToolResultMessage::text(&bash, "ok"),
        });
        assert!(changed(&panel.tick()).is_empty());
    }
    #[test]
    fn the_system_prompt_can_be_opened_as_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            system_prompt: "You are terse.\n".into(),
            ..setup(vec![])
        });
        let labels: Vec<String> = panel
            .context_menu_items()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert!(labels.iter().any(|label| label == "Show system prompt"));

        let events = panel.handle_status_action(SHOW_PROMPT_ACTION);
        let Some(PanelEvent::ViewFile(path)) = events.first() else {
            panic!("expected a viewer, got {events:?}");
        };
        assert_eq!(path, &dir.path().join("system-prompt.md"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "You are terse.\n");
        // The prompt file is not mistaken for a session.
        assert!(panel.session_list().iter().all(|s| s.path != *path));
    }

    #[test]
    fn a_system_prompt_block_shows_once_and_on_change() {
        let mut panel = AgentPanel::new(AgentPanelSetup {
            system_prompt: "Base prompt.".into(),
            ..setup(vec![reply("a"), reply("b"), reply("c")])
        });
        let count = |p: &AgentPanel| {
            p.transcript()
                .items()
                .iter()
                .filter(|i| matches!(i, Item::System { .. }))
                .count()
        };

        type_text(&mut panel, "hi");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(count(&panel), 1, "shown with the first message");

        // Unchanged prompt: not repeated.
        type_text(&mut panel, "again");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(count(&panel), 1, "not repeated when unchanged");

        // A different agent has a different prompt: shown again.
        assert!(panel.switch_agent("review"));
        type_text(&mut panel, "more");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(count(&panel), 2, "shown again after it changed");
    }

    #[test]
    fn switching_agents_changes_prompt_model_and_mode_and_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(Scripted::new(vec![reply("ok")]));
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup_with(Arc::clone(&provider))
        });
        assert_eq!(chip(&panel, AGENT_ACTION), "default");

        let events = panel.handle_status_action(AGENT_ACTION);
        let picker = events.first().expect("picker");
        let PanelEvent::ShowSelect { options, .. } = picker else {
            panic!("expected a picker, got {picker:?}");
        };
        assert_eq!(
            options,
            &[
                "● default",
                "  review · Reviews diffs",
                "  outside · An external agent"
            ]
        );
        assert!(matches!(
            select(&mut panel, picker, 1),
            CommandResult::Handled(true)
        ));

        assert_eq!(chip(&panel, AGENT_ACTION), "review");
        assert_eq!(chip(&panel, MODEL_ACTION), "big");
        assert_eq!(chip(&panel, MODE_ACTION), "accept-edits");
        assert_eq!(panel.system_prompt, "You review diffs.");
        assert!(panel
            .transcript()
            .items()
            .iter()
            .any(|item| matches!(item, Item::Notice { text, .. } if text == "agent: review")));

        // The next run goes to the reviewer's model, and the layout state
        // names the agent so a restore comes back as it.
        type_text(&mut panel, "go");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(
            *provider.seen_models.lock().unwrap(),
            vec!["big".to_string()]
        );
        let Some(termide_core::PanelState::Agent { agent, .. }) =
            panel.to_state(Path::new("/unused"))
        else {
            panic!("agent state expected");
        };
        assert_eq!(agent.as_deref(), Some("review"));
        assert_eq!(
            Session::open(panel.session_path().unwrap())
                .unwrap()
                .current_model()
                .unwrap()
                .id,
            "big"
        );

        // A new session starts as the current agent; reopening the first
        // one comes back as the agent it last ran as, prompt and tools too.
        let first_path = panel.session_path().unwrap().to_path_buf();
        panel.handle_status_action(NEW_SESSION_ACTION);
        assert_eq!(chip(&panel, AGENT_ACTION), "review");
        panel.switch_agent("default");
        assert_eq!(panel.system_prompt, "default prompt");
        assert_eq!(chip(&panel, MODEL_ACTION), "big", "the model stays");
        assert_eq!(chip(&panel, MODE_ACTION), "accept-edits", "the mode stays");
        panel.session_choices = panel.session_list();
        let index = panel
            .session_choices
            .iter()
            .position(|s| s.path == first_path)
            .unwrap();
        panel.resume_choice(index);
        assert_eq!(chip(&panel, AGENT_ACTION), "review");
        assert_eq!(panel.system_prompt, "You review diffs.");
        assert_eq!(
            Session::open(&first_path)
                .unwrap()
                .current_agent()
                .as_deref(),
            Some("review")
        );
        assert!(!panel.switch_agent("missing"));
    }
    #[test]
    fn o_opens_the_selected_block_in_a_read_only_panel() {
        let mut panel = AgentPanel::new(setup(vec![reply("the answer here")]));
        type_text(&mut panel, "do it");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE)); // chat focus, last block (assistant)
        let events = panel.handle_key(chord(KeyCode::Char('o'), KeyModifiers::NONE));
        let path = events.iter().find_map(|e| match e {
            PanelEvent::ViewFile(path) => Some(path.clone()),
            _ => None,
        });
        let path = path.expect("o should open a ViewFile panel");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("the answer here"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tab_moves_focus_to_the_chat_and_arrows_fold_blocks() {
        // The answer folds only when its reasoning is long enough to be worth
        // hiding.
        let thinking = "l1\nl2\nl3\nl4\nl5\nl6";
        let mut panel = AgentPanel::new(setup(vec![reply_thinking("the answer", thinking)]));
        type_text(&mut panel, "do it");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        // A user block and an assistant block exist.
        assert!(panel.transcript().items().len() >= 2);
        assert!(!panel.chat_focus);

        // Tab moves focus to the chat, on the last (answer) block, which is not
        // foldable. Blocks are user(0), thinking(1), answer(2).
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        assert!(panel.chat_focus);
        assert_eq!(panel.selected, panel.transcript().items().len() - 1);

        // Up walks to the reasoning block, which folds; Space expands it, again
        // folds it.
        panel.handle_key(chord(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.selected, panel.transcript().items().len() - 2);
        assert!(!panel.transcript().any_expanded());
        panel.handle_key(chord(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(panel.transcript().any_expanded());
        panel.handle_key(chord(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(!panel.transcript().any_expanded());

        // Up walks to the user block; a printable key does not type.
        panel.handle_key(chord(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.selected, 0);
        panel.handle_key(chord(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(panel.input_text().is_empty());

        // Tab returns focus to the input.
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!panel.chat_focus);
        type_text(&mut panel, "hi");
        assert_eq!(panel.input_text(), "hi");
    }

    #[test]
    fn file_completions_list_paths_and_at_mentions_insert_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref").unwrap();

        // The walker skips .git and offers files and directories.
        let all = file_completions(dir.path(), "");
        let values: Vec<&str> = all.iter().map(|i| i.value.as_str()).collect();
        assert!(values.contains(&"README.md"), "{values:?}");
        assert!(values.contains(&"src/"), "{values:?}");
        assert!(values.contains(&"src/main.rs"), "{values:?}");
        assert!(!values.iter().any(|v| v.contains(".git")), "{values:?}");
        // A prefix filters, name matches rank first.
        let main = file_completions(dir.path(), "main");
        assert_eq!(main.first().map(|i| i.value.as_str()), Some("src/main.rs"));

        // Typing @ opens the file popup; selecting a file replaces the token.
        let mut panel = AgentPanel::new(AgentPanelSetup {
            cwd: dir.path().to_path_buf(),
            ..setup(vec![])
        });
        type_text(&mut panel, "look at @READ");
        assert!(panel.completion.is_some(), "no @ completion popup");
        assert!(panel.completion_span.is_some());
        assert!(panel
            .completion
            .as_ref()
            .unwrap()
            .items()
            .iter()
            .any(|i| i.value == "README.md"));
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "look at README.md ");
        assert!(panel.completion.is_none());

        // A directory keeps the popup open for its contents.
        type_text(&mut panel, "@src");
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "look at README.md @src/");
        assert!(panel.completion.is_some(), "dir did not reopen the popup");
        assert!(panel
            .completion
            .as_ref()
            .unwrap()
            .items()
            .iter()
            .any(|i| i.value == "src/main.rs"));
    }

    #[test]
    fn slash_commands_expand_prompt_templates() {
        let mut expanding = panel(vec![reply("done")]);
        type_text(&mut expanding, "/review src/x.rs");
        expanding.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut expanding);
        let items = expanding.transcript().items();
        assert!(
            matches!(&items[0], Item::User { text, .. } if text == "Review src/x.rs carefully."),
            "{items:?}"
        );

        // An unknown command is refused with the names on offer; a path is text.
        let mut panel = panel(vec![reply("ok")]);
        type_text(&mut panel, "/nope");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel.transcript().items().iter().any(
            |item| matches!(item, Item::Notice { text, .. } if text.contains("available: review"))
        ));
        assert_eq!(panel.input_text(), "/nope", "the input is kept for editing");
        assert_eq!(slash_command("/usr/bin/ls -la"), None);
        assert_eq!(slash_command("/review a b"), Some(("review", "a b")));
        assert_eq!(slash_command("/"), None);

        // The picker puts the command into the input, ready for arguments.
        let events = panel.handle_status_action(PROMPTS_ACTION);
        let picker = events.first().expect("picker");
        let PanelEvent::ShowSelect { options, .. } = picker else {
            panic!("expected a picker, got {picker:?}");
        };
        assert_eq!(options, &["/review <path> · Review a file"]);
        select(&mut panel, picker, 0);
        assert_eq!(panel.input_text(), "/review ");
    }
    /// A tool with nothing behind it, standing in for one an MCP server sent.
    struct Late(&'static str);

    impl termide_agent_core::Tool for Late {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "late"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn execute(
            &self,
            call: &termide_agent_core::ToolCall,
            _ctx: &termide_agent_core::ToolContext,
            _on_update: &mut dyn FnMut(ToolUpdate),
            _cancel: &CancelToken,
        ) -> ToolResultMessage {
            ToolResultMessage::text(call, "late")
        }
    }

    #[test]
    fn late_tools_join_the_registry_and_failures_are_reported() {
        let (tx, rx) = mpsc::channel();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            late_tools: Some(rx),
            ..setup(vec![])
        });
        tx.send(LateTools::Ready {
            source: "github".into(),
            tools: vec![Arc::new(Late("github__search"))],
        })
        .unwrap();
        tx.send(LateTools::Failed {
            source: "ghost".into(),
            error: "cannot start ghost-server".into(),
        })
        .unwrap();
        let events = panel.tick();
        assert!(!events.is_empty());
        assert!(panel.tools.get("github__search").is_some());
        let notices: Vec<String> = panel
            .transcript()
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [
                "mcp github: 1 tools connected",
                "mcp ghost: cannot start ghost-server"
            ]
        );
        // The worker got them too: the next run sees the tool in its registry.
        let runtime = std::mem::replace(&mut panel.runtime, Box::new(Idle));
        let agent = runtime.into_agent().expect("agent");
        assert!(agent.tools().get("github__search").is_some());
    }
    /// A backend with nothing behind it, to swap out of a panel in a test.
    struct Idle;

    impl Backend for Idle {
        fn prompt(&self, _message: UserMessage) -> Result<(), PromptError> {
            Err(PromptError::Stopped)
        }
        fn steer(&self, _message: UserMessage) {}
        fn queue_lens(&self) -> (usize, usize) {
            (0, 0)
        }
        fn abort(&self) {}
        fn is_busy(&self) -> bool {
            false
        }
        fn drain(&self) -> Vec<AgentEvent> {
            Vec::new()
        }
        fn update(&self, _update: Box<dyn FnOnce(&mut Agent) + Send>) -> Result<(), PromptError> {
            Err(PromptError::Unsupported)
        }
        fn compact(&self, _focus: Option<String>) -> Result<(), PromptError> {
            Err(PromptError::Unsupported)
        }
        fn into_agent(self: Box<Self>) -> Option<Agent> {
            None
        }
    }

    /// An "external agent" for tests: answers every prompt with one text
    /// message, through the same events as the real ACP backend.
    struct External {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl External {
        fn new(_setup: BackendSetup) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl Backend for External {
        fn prompt(&self, message: UserMessage) -> Result<(), PromptError> {
            let mut events = self.events.lock().unwrap();
            events.push(AgentEvent::AgentStart);
            events.push(AgentEvent::MessageEnd(Message::User(message)));
            events.push(AgentEvent::MessageEnd(Message::Assistant(
                AssistantMessage {
                    content: vec![AssistantContent::Text {
                        text: "from outside".into(),
                    }],
                    stop_reason: StopReason::Stop,
                    usage: Usage::default(),
                    provider: "acp".into(),
                    model: "outside".into(),
                    error_message: None,
                    timestamp: 0,
                },
            )));
            events.push(AgentEvent::AgentEnd);
            Ok(())
        }
        fn steer(&self, _message: UserMessage) {}
        fn queue_lens(&self) -> (usize, usize) {
            (0, 0)
        }
        fn abort(&self) {}
        fn is_busy(&self) -> bool {
            false
        }
        fn drain(&self) -> Vec<AgentEvent> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
        fn update(&self, _update: Box<dyn FnOnce(&mut Agent) + Send>) -> Result<(), PromptError> {
            Err(PromptError::Unsupported)
        }
        fn compact(&self, _focus: Option<String>) -> Result<(), PromptError> {
            Err(PromptError::Unsupported)
        }
        fn into_agent(self: Box<Self>) -> Option<Agent> {
            None
        }
    }

    #[test]
    fn an_external_agent_replaces_the_loop_and_hides_its_knobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            ..setup(vec![reply("native")])
        });
        type_text(&mut panel, "hello");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);

        assert!(panel.switch_agent("outside"));
        assert_eq!(chip(&panel, AGENT_ACTION), "outside");
        let texts: Vec<String> = panel
            .status_segments()
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert!(!texts.iter().any(|t| t.starts_with("Mode")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.starts_with("Model")), "{texts:?}");
        assert!(texts.contains(&" (acp)".to_string()));
        // The earlier conversation is shown, and marked as unknown to the agent.
        let items = panel.transcript().items();
        assert!(matches!(&items[0], Item::User { text, .. } if text == "hello"));
        assert!(items.iter().any(
            |i| matches!(i, Item::Notice { text, .. } if text.contains("not known to the external agent"))
        ));

        // Model and mode are not ours any more.
        let events = panel.handle_status_action(MODE_ACTION);
        assert!(!events
            .iter()
            .any(|e| matches!(e, PanelEvent::ShowSelect { .. })));
        panel.handle_key(chord(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(panel.mode.get(), Mode::Ask);

        type_text(&mut panel, "go");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert!(panel
            .transcript()
            .items()
            .iter()
            .any(|i| matches!(i, Item::Assistant { text, .. } if text == "from outside")));
        // The log records both the switch and the external agent's answer.
        let session = Session::open(panel.session_path().unwrap()).unwrap();
        assert_eq!(session.current_agent().as_deref(), Some("outside"));
        assert_eq!(session.context_messages().len(), 4);

        // Back to the built-in loop.
        assert!(panel.switch_agent("default"));
        assert!(!panel.external);
        assert_eq!(chip(&panel, MODE_ACTION), "ask");
    }
    #[test]
    fn arrow_keys_recall_earlier_requests_and_bring_the_draft_back() {
        let mut panel = panel(vec![reply("a"), reply("b")]);
        for request in ["first request", "second request"] {
            type_text(&mut panel, request);
            panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
            settle(&mut panel);
        }
        type_text(&mut panel, "half typ");
        panel.handle_key(chord(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "second request");
        panel.handle_key(chord(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "first request");
        // Past the oldest it stays; back down it returns to the draft.
        panel.handle_key(chord(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "first request");
        panel.handle_key(chord(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(chord(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "half typ");
        assert!(panel.history_pos.is_none());
        // Typing ends browsing; the arrows then move inside a multi-line input.
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut panel, "more");
        panel.handle_key(chord(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "half typ\nmore");
        assert_eq!(format_tokens(32_000), "32k");
        assert_eq!(format_tokens(262_144), "262k");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_250_000), "1.2M");
        assert_eq!(format_tokens(512), "512");
    }

    #[test]
    fn typing_a_slash_offers_templates_and_tab_or_enter_completes() {
        let mut panel = panel(vec![reply("ok")]);
        type_text(&mut panel, "/re");
        let popup = panel.completion.as_ref().expect("popup");
        assert_eq!(popup.items()[0].value, "review");
        let rows = render_text(&mut panel, 60, 12);
        assert!(
            rows.iter()
                .any(|r| r.contains("/review <path>  Review a file")),
            "{rows:?}"
        );
        // Tab completes and closes the popup; the space invites arguments.
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "/review ");
        assert!(panel.completion.is_none());

        // Enter on a partial name completes; on the full name it sends.
        panel.handle_key(chord(KeyCode::Esc, KeyModifiers::NONE));
        type_text(&mut panel, "/rev");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "/review ");
        assert!(panel.transcript().items().is_empty());
        panel.handle_key(chord(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(
            panel.completion.is_some(),
            "a lone /review shows the popup again"
        );
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert!(matches!(
            panel.transcript().items().first(),
            Some(Item::User { text, .. }) if text == "Review  carefully."
        ));

        // No match, no popup; Esc closes an open one.
        type_text(&mut panel, "/zzz");
        assert!(panel.completion.is_none());
        panel.handle_key(chord(KeyCode::Esc, KeyModifiers::NONE));
        type_text(&mut panel, "/r");
        assert!(panel.completion.is_some());
        panel.handle_key(chord(KeyCode::Esc, KeyModifiers::NONE));
        assert!(panel.completion.is_none());
        assert_eq!(
            panel.input_text(),
            "/r",
            "Esc closes the popup, not the input"
        );
    }
    #[test]
    fn slash_compact_is_built_in_and_reports_through_the_transcript() {
        let mut panel = panel(vec![]);
        type_text(&mut panel, "/comp");
        let popup = panel.completion.as_ref().expect("popup");
        assert!(popup.items().iter().any(|i| i.value == "compact"));
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "/compact ");
        type_text(&mut panel, "the tests");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "", "the command is consumed, not sent");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            panel.tick();
            let failed = panel.transcript().items().iter().any(|item| {
                matches!(item, Item::Notice { text, .. } if text.contains("too few messages"))
            });
            if failed {
                break;
            }
            assert!(Instant::now() < deadline, "no compaction notice");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(panel
            .transcript()
            .items()
            .iter()
            .all(|i| !matches!(i, Item::User { .. })));
    }
    #[cfg(unix)]
    #[test]
    fn command_scripts_run_and_a_project_one_asks_first() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str, trusted: bool| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            CommandScript::from_file(&path, trusted).unwrap()
        };
        let gather = script(
            "gather",
            "#!/bin/sh\n# description: Gather context\n# argument-hint: <topic>\necho \"Context about $1\"\n",
            true,
        );
        let project = script(
            "scan",
            "#!/bin/sh\n# description: Scan\necho scanned\n",
            false,
        );
        *COMMANDS.lock().unwrap() = vec![gather, project];
        let mut panel = panel(vec![reply("a"), reply("b")]);
        let wait_user = |panel: &mut AgentPanel, text: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                panel.tick();
                if panel
                    .transcript()
                    .items()
                    .iter()
                    .any(|i| matches!(i, Item::User { text: t, .. } if t == text))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{:?}",
                    panel.transcript().items()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // The trusted script runs unasked and its output is the request.
        type_text(&mut panel, "/ga");
        assert!(panel
            .completion
            .as_ref()
            .unwrap()
            .items()
            .iter()
            .any(|i| i.value == "gather"));
        panel.handle_key(chord(KeyCode::Tab, KeyModifiers::NONE));
        type_text(&mut panel, "parsers");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.input_text(), "");
        wait_user(&mut panel, "Context about parsers");
        settle(&mut panel);

        // The project's script asks; "run for this session" runs it now and
        // next time without asking.
        type_text(&mut panel, "/scan");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        let form = panel.pending.as_ref().expect("a card asks").form();
        assert!(form.title().starts_with("Run the project command /scan"));
        assert_eq!(form.options().len(), 4);
        panel.handle_key(chord(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(panel.pending.is_none());
        wait_user(&mut panel, "scanned");
        settle(&mut panel);
        assert!(panel.allowed_commands.contains("scan"));

        // "Don't run" leaves nothing behind.
        *COMMANDS.lock().unwrap() = vec![script("other", "#!/bin/sh\necho x\n", false)];
        type_text(&mut panel, "/other");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel.pending.is_some());
        panel.handle_key(chord(KeyCode::Char('4'), KeyModifiers::NONE));
        assert!(panel.pending.is_none() && panel.command_run.is_none());
        *COMMANDS.lock().unwrap() = Vec::new();
    }
    #[test]
    fn undo_restores_the_files_and_rewinds_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before").unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            cwd: dir.path().to_path_buf(),
            session_dir: Some(sessions),
            ..setup(vec![reply("a"), reply("b"), reply("c")])
        });
        type_text(&mut panel, "task one");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        // Nothing changed files yet: /undo has nothing to offer.
        type_text(&mut panel, "/undo");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel.pending.is_none());
        assert!(panel
            .transcript()
            .items()
            .iter()
            .any(|i| matches!(i, Item::Notice { text, .. } if text.contains("nothing to undo"))));

        // The second request "edits" the file: the store records it the way
        // the hook does for the edit tool.
        let leaf = panel
            .session
            .as_ref()
            .unwrap()
            .leaf_id()
            .map(str::to_string);
        {
            let store = panel.checkpoints.as_ref().unwrap();
            let mut store = store.lock().unwrap();
            store.begin_run(leaf);
            store.save(&file).unwrap();
            std::fs::write(&file, "after").unwrap();
            store.end_run();
        }
        type_text(&mut panel, "task two");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        assert_eq!(panel.session.as_ref().unwrap().context_messages().len(), 4);

        type_text(&mut panel, "/un");
        assert!(panel
            .completion
            .as_ref()
            .unwrap()
            .items()
            .iter()
            .any(|i| i.value == "undo"));
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE)); // completes
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE)); // sends /undo
        let form = panel.pending.as_ref().expect("undo card").form();
        assert!(form.title().contains("notes.txt"), "{}", form.title());
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        let events = panel.tick();
        assert!(events
            .iter()
            .any(|e| matches!(e, PanelEvent::FileChangedOnDisk(p) if p == &file)));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        // The conversation is back at the end of task one, on disk too.
        assert_eq!(panel.session.as_ref().unwrap().context_messages().len(), 2);
        assert!(!panel
            .transcript()
            .items()
            .iter()
            .any(|i| matches!(i, Item::User { text, .. } if text == "task two")));
        assert!(panel.transcript().items().iter().any(
            |i| matches!(i, Item::Notice { text, .. } if text.contains("undid the last request"))
        ));
        let reopened = Session::open(panel.session_path().unwrap()).unwrap();
        assert_eq!(reopened.context_messages().len(), 2);
    }
    #[test]
    fn plan_mode_adds_its_instructions_and_offers_to_carry_the_plan_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = AgentPanel::new(AgentPanelSetup {
            session_dir: Some(dir.path().to_path_buf()),
            system_prompt: "Base prompt.".into(),
            plan_prompt: PlanPrompt::from_file("---\nrequest: Do it.\n---\nPlan first."),
            ..setup(vec![reply("1. change a\n2. change b"), reply("done")])
        });
        // ask → accept-edits → auto → plan
        for _ in 0..3 {
            panel.handle_key(chord(KeyCode::BackTab, KeyModifiers::SHIFT));
        }
        assert_eq!(panel.mode.get(), Mode::Plan);
        assert_eq!(chip(&panel, MODE_ACTION), "plan");
        let shown = panel.write_system_prompt().unwrap();
        assert_eq!(
            std::fs::read_to_string(shown).unwrap(),
            "Base prompt.\n\nPlan first."
        );

        type_text(&mut panel, "add a feature");
        panel.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut panel);
        let form = panel.pending.as_ref().expect("plan card").form();
        assert!(form.title().starts_with("Plan mode"), "{}", form.title());

        // Esc keeps planning; the next answer offers again.
        panel.handle_key(chord(KeyCode::Esc, KeyModifiers::NONE));
        assert!(panel.pending.is_none());
        assert_eq!(panel.mode.get(), Mode::Plan);

        panel.offer_plan();
        panel.handle_key(chord(KeyCode::Char('1'), KeyModifiers::NONE));
        let _ = panel.tick();
        assert_eq!(panel.mode.get(), Mode::AcceptEdits);
        settle(&mut panel);
        let users: Vec<&str> = panel
            .transcript()
            .items()
            .iter()
            .filter_map(|i| match i {
                Item::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["add a feature", "Do it."]);
        let shown = panel.write_system_prompt().unwrap();
        assert_eq!(std::fs::read_to_string(shown).unwrap(), "Base prompt.");
        assert!(panel.pending.is_none(), "no card outside plan mode");
    }
}
