//! The coding agent panel: a transcript above a multi-line input, tool calls
//! collapsed to one line each, permission prompts routed through termide's
//! selection modal.
//!
//! The panel owns an [`AgentRuntime`] and mirrors its events into a
//! [`Transcript`] from `tick()`, so it never blocks the UI thread. Every
//! transcript change also goes to the JSONL [`Session`] when one is attached.

mod transcript;

use std::any::Any;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use termide_agent_core::{
    civil_date, permission_channel, Agent, AgentEvent, Backend, BackendSetup, CancelToken,
    ChainedHooks, CompactionPolicy, CompactionPrompts, Decision, Hooks, LateTools, Message, Mode,
    ModeHandle, ModelInfo, ModelSpec, PermissionAnswer, PermissionEnvelope, PermissionHooks,
    PermissionRules, PersistRule, PromptTemplate, Provider, Session, SessionSummary, StreamEvent,
    Tool, ToolRegistry, ToolResultMessage, ToolUpdate, UserMessage, DEFAULT_AGENT,
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
    ChoiceAction, ChoiceForm, CompletionAction, CompletionItem, CompletionList, ScrollBar,
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
/// Context-menu action that opens the assembled system prompt in a viewer.
const SHOW_PROMPT_ACTION: &str = "agent_show_prompt";
/// Status chip and context-menu action that opens the agent picker.
const AGENT_ACTION: &str = "agent_agent";
/// Context-menu action that opens the prompt-template picker.
const PROMPTS_ACTION: &str = "agent_prompts";
/// The built-in `/compact [focus]` command.
const COMPACT_COMMAND: &str = "compact";

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
    pub model: ModelSpec,
    pub tools: ToolRegistry,
    pub rules: PermissionRules,
    pub system_prompt: String,
    pub compaction: CompactionPolicy,
    /// The texts of a compaction, from the agent directory's `system/` files.
    pub compaction_prompts: CompactionPrompts,
    /// Where "allow always" rules go; a plain function so it survives a
    /// session switch. `None` keeps such rules in memory only.
    pub persist_rule: Option<PersistFn>,
    /// Directory holding this project's session logs; `None` runs without
    /// persistence and without the session picker.
    pub session_dir: Option<PathBuf>,
    /// Session to start in; `None` creates one in `session_dir`.
    pub session: Option<Session>,
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
}

pub struct AgentPanel {
    runtime: Box<dyn Backend>,
    /// The runtime is an external agent: model and mode are not ours to set.
    external: bool,
    permission_rx: Receiver<PermissionEnvelope>,
    /// The agent's question waiting for an answer, and the form asking it.
    pending_permission: Option<(PermissionEnvelope, ChoiceForm)>,
    session: Option<Session>,
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
    persist_rule: Option<PersistFn>,

    transcript: Transcript,
    input: TextArea,
    /// Which earlier request the input shows while browsing history with
    /// the arrow keys; `None` while typing.
    history_pos: Option<usize>,
    /// What was being typed when browsing started, restored on the way back.
    draft: String,
    /// The `/command` completion list, while the input is a lone `/word`.
    completion: Option<CompletionList>,
    /// First visible transcript line.
    top: usize,
    /// Keep the view pinned to the newest line while true.
    follow: bool,
    busy: bool,
    queued: (usize, usize),
    /// Tokens of the last reported context, for the status chip.
    context_tokens: u64,

    colors: ThemeColors,
    is_light: bool,
    transcript_area: Rect,
    input_area: Rect,
    scrollbars: ScrollBars,
}

impl AgentPanel {
    #[must_use]
    pub fn new(setup: AgentPanelSetup) -> Self {
        let session = setup.session.or_else(|| {
            start_session(
                setup.session_dir.as_deref(),
                &setup.cwd,
                setup.provider.name(),
                &setup.model,
                &setup.agent,
            )
        });
        let model = session_model(&setup.model, session.as_ref());
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
            setup.persist_rule,
            setup.hooks.as_ref(),
            backend.as_ref(),
            session.as_ref(),
        );
        Self {
            runtime,
            external,
            permission_rx,
            pending_permission: None,
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
            pending_events: Vec::new(),
            hooks: setup.hooks,
            backend,
            provider: setup.provider,
            tools,
            rules: setup.rules,
            system_prompt,
            compaction: setup.compaction,
            compaction_prompts: setup.compaction_prompts,
            persist_rule: setup.persist_rule,
            transcript,
            input: TextArea::new(),
            history_pos: None,
            draft: String::new(),
            completion: None,
            top: 0,
            follow: true,
            busy: false,
            queued: (0, 0),
            context_tokens: 0,
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
                self.provider.name(),
                &self.model,
                &self.agent,
            )
        });
        let model = session_model(&self.configured_model, session.as_ref());
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
            self.persist_rule,
            self.hooks.as_ref(),
            self.backend.as_ref(),
            session.as_ref(),
        );
        // Dropping the old runtime cancels it and asks its worker to stop.
        self.runtime = runtime;
        self.external = external;
        self.permission_rx = permission_rx;
        self.pending_permission = None;
        self.transcript = transcript;
        self.session = session;
        self.model = model;
        self.agent = agent;
        self.system_prompt = system_prompt;
        self.tools = tools;
        self.mode = mode;
        self.model_choices.clear();
        self.model_fetch = None;
        self.input = TextArea::new();
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
        self.input.text()
    }

    #[must_use]
    pub fn session_path(&self) -> Option<&std::path::Path> {
        self.session.as_ref().map(Session::path)
    }

    /// Send the input box: a new run when idle, a steering message while
    /// the agent works.
    pub fn submit(&mut self) -> Vec<PanelEvent> {
        let text = self.input.text().trim().to_string();
        if text.is_empty() {
            return vec![];
        }
        self.completion = None;
        self.history_pos = None;
        self.draft.clear();
        let text = match slash_command(&text) {
            Some((COMPACT_COMMAND, focus)) => {
                // Built in: summarise the older part of the session now.
                let focus = (!focus.is_empty()).then(|| focus.to_string());
                match self.runtime.compact(focus) {
                    Ok(()) => self.input = TextArea::new(),
                    Err(PromptError::Busy) => {
                        self.notice("finish or stop the current task first", NoticeKind::Warn)
                    }
                    Err(error) => self.notice(error.to_string(), NoticeKind::Warn),
                }
                return vec![PanelEvent::NeedsRedraw];
            }
            Some((name, args)) => {
                let prompts = self.catalog.prompts();
                let Some(template) = prompts.iter().find(|p| p.name == name) else {
                    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
                    self.notice(
                        if names.is_empty() {
                            format!("no prompt named {name}; there are no prompt templates")
                        } else {
                            format!("no prompt named {name}; available: {}", names.join(", "))
                        },
                        NoticeKind::Warn,
                    );
                    return vec![PanelEvent::NeedsRedraw];
                };
                template.expand(args)
            }
            None => text,
        };
        self.input = TextArea::new();
        self.follow = true;
        let message = UserMessage::text(text);
        if self.is_busy() {
            self.runtime.steer(message);
            self.queued = self.runtime.queue_lens();
            self.notice("queued for the next turn", NoticeKind::Info);
        } else {
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
    fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.busy = true,
            AgentEvent::AgentEnd => {
                self.busy = false;
                self.queued = self.runtime.queue_lens();
            }
            AgentEvent::TurnStart | AgentEvent::TurnEnd => {}
            AgentEvent::MessageStart => self.transcript.push(Item::Assistant {
                text: String::new(),
                thinking_chars: 0,
                streaming: true,
                error: None,
            }),
            AgentEvent::MessageUpdate(StreamEvent::TextDelta(delta)) => {
                self.transcript
                    .with_streaming_assistant(|text, _| text.push_str(&delta));
            }
            AgentEvent::MessageUpdate(StreamEvent::ThinkingDelta(delta)) => {
                self.transcript.with_streaming_assistant(|_, thinking| {
                    *thinking += delta.chars().count();
                });
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
                    }),
                    Message::Assistant(assistant) => {
                        if assistant.usage.total() > 0 {
                            self.context_tokens = assistant.usage.total();
                        }
                        self.transcript.finish_assistant(
                            assistant.plain_text(),
                            assistant.error_message.clone(),
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
            AgentEvent::ToolExecutionStart { call } => self.transcript.push(Item::Tool {
                call,
                result: None,
                live: None,
                expanded: false,
            }),
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
                self.transcript.with_tool(&id, |item| {
                    if let Item::Tool {
                        result: slot, live, ..
                    } = item
                    {
                        *slot = Some(result);
                        *live = None;
                    }
                });
            }
            AgentEvent::QueueUpdate {
                steering,
                follow_up,
            } => self.queued = (steering, follow_up),
            AgentEvent::CompactionStart { .. } => {
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
            if self.pending_permission.is_some() {
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
            self.pending_permission = Some((envelope, form));
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
        match Session::open(&summary.path) {
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
        let Some((envelope, _)) = self.pending_permission.take() else {
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
        std::fs::write(&path, &self.system_prompt)?;
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
        let provider = Arc::clone(&self.provider);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(provider.list_models());
        });
        self.model_fetch = Some(rx);
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
        self.mode.set(mode);
        self.rules.mode = mode;
        PanelEvent::SetStatusMessage {
            message: format!(
                "{}: {}",
                termide_i18n::t().agent_change_mode(),
                mode.label()
            ),
            is_error: false,
        }
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
        let prompt = profile.system_prompt.clone();
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
                    self.provider.name(),
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
        if id == self.model.id {
            return true;
        }
        let model = ModelSpec {
            id: id.to_string(),
            context_window: context_window.unwrap_or(self.model.context_window),
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
                self.provider.name(),
                id,
                Some(self.model.context_window),
            ) {
                log::warn!("agent session write failed: {error}");
            }
        }
        self.notice(format!("model: {id}"), NoticeKind::Info);
        true
    }

    /// Earlier requests of this session, oldest first, repeats collapsed.
    fn history(&self) -> Vec<String> {
        let mut history: Vec<String> = Vec::new();
        for item in self.transcript.items() {
            if let Item::User { text } = item {
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
                self.draft = self.input.text();
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
        self.input = TextArea::with_text(text);
        while self.input.move_down() {}
        self.input.move_end();
    }

    /// Recompute the `/command` popup after the input changed: it shows
    /// while the input is a single `/word` with no space yet.
    fn refresh_completion(&mut self) {
        let text = self.input.text();
        let word = text
            .strip_prefix('/')
            .filter(|rest| self.input.line_count() <= 1 && !rest.contains(char::is_whitespace));
        let Some(prefix) = word else {
            self.completion = None;
            return;
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
        if COMPACT_COMMAND.starts_with(prefix) && !self.external {
            items.push(
                CompletionItem::new(COMPACT_COMMAND)
                    .with_label(format!("/{COMPACT_COMMAND}"))
                    .with_hint("[focus]")
                    .with_description("Summarise the older part of the session now"),
            );
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

    /// Put the highlighted command into the input, ready for arguments.
    fn accept_completion(&mut self) -> bool {
        let Some(list) = self.completion.take() else {
            return false;
        };
        let Some(item) = list.selected_item() else {
            return false;
        };
        let text = format!("/{} ", item.value);
        self.set_input(&text);
        true
    }

    /// Turn what the permission form reported into an answer. `Cancelled`
    /// denies and stops the run: the user wants out, not just a "no" to this
    /// one call. `false` for `NotHandled`.
    fn apply_permission_action(&mut self, action: ChoiceAction) -> bool {
        match action {
            ChoiceAction::Chosen(index) => {
                self.answer_permission(Self::permission_answer(index));
            }
            ChoiceAction::Custom(reason) => {
                self.answer_permission(PermissionAnswer::DenyWithReason(reason));
            }
            ChoiceAction::Cancelled => {
                self.answer_permission(PermissionAnswer::Deny);
                self.abort();
            }
            ChoiceAction::Handled => {}
            ChoiceAction::NotHandled => return false,
        }
        true
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
        let rows = self.input.line_count().max(1) as u16;
        rows.min(MAX_INPUT_ROWS)
            .min(available.saturating_sub(2).max(1))
    }

    fn render_input(&mut self, area: Rect, buf: &mut Buffer, focused: bool) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let prompt_style = Style::default()
            .fg(self.colors.info)
            .add_modifier(Modifier::BOLD);
        let text_style = Style::default().fg(self.colors.fg);
        self.input.ensure_cursor_visible(area.height as usize);
        let offset = self.input.scroll_offset();
        let lines = self.input.lines();
        let text_x = area.x + 2;
        let text_width = area.width.saturating_sub(2);
        for row in 0..area.height as usize {
            let y = area.y + row as u16;
            let prefix = if row + offset == 0 { "› " } else { "  " };
            buf.set_string(area.x, y, prefix, prompt_style);
            if let Some(line) = lines.get(row + offset) {
                buf.set_stringn(text_x, y, line, text_width as usize, text_style);
            }
        }
        if lines.len() <= 1 && lines.first().is_none_or(String::is_empty) && !focused {
            buf.set_stringn(
                text_x,
                area.y,
                "Ask the agent…",
                text_width as usize,
                Style::default().fg(self.colors.disabled),
            );
        }
        if focused {
            let cursor = self.input.cursor();
            if cursor.row >= offset && cursor.row - offset < area.height as usize {
                let line = lines.get(cursor.row).map(String::as_str).unwrap_or("");
                let col: usize = line
                    .chars()
                    .take(cursor.col)
                    .map(unicode_display_width)
                    .sum();
                if (col as u16) < text_width {
                    let x = text_x + col as u16;
                    let y = area.y + (cursor.row - offset) as u16;
                    buf[(x, y)]
                        .set_style(Style::default().fg(self.colors.bg).bg(self.colors.cursor));
                }
            }
        }
    }
}

/// Longest prompt shown in the panel title before it is cut.
const MAX_TITLE_CHARS: usize = 60;

/// First line of `text`, collapsed to one line and cut with an ellipsis.
fn truncate_title(text: &str) -> String {
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX_TITLE_CHARS {
        return single_line;
    }
    let cut: String = single_line.chars().take(MAX_TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

fn unicode_display_width(c: char) -> usize {
    // Wide East Asian and emoji ranges take two cells; everything else one.
    // Good enough for cursor placement in a prompt box.
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
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
    let mut session = match Session::create(dir?, cwd) {
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
    match session.and_then(Session::current_model) {
        Some(recorded) if !recorded.id.is_empty() => ModelSpec {
            id: recorded.id,
            context_window: recorded.context_window.unwrap_or(configured.context_window),
            ..configured.clone()
        },
        _ => configured.clone(),
    }
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
    persist_rule: Option<PersistFn>,
    extra_hooks: Option<&HooksFactory>,
    backend: Option<&BackendFactory>,
    session: Option<&Session>,
) -> Spawned {
    let cancel = CancelToken::new();
    let (prompter, permission_rx) = permission_channel(cancel.clone());
    let mut hooks = PermissionHooks::new(rules, Box::new(prompter));
    let mode = hooks.mode_handle();
    if let Some(persist) = persist_rule {
        hooks = hooks.with_persist(Box::new(persist) as PersistRule);
    }

    let mut transcript = Transcript::default();
    let messages = session
        .map(|s| s.context_messages_with(compaction_prompts))
        .unwrap_or_default();
    for message in &messages {
        push_history(&mut transcript, message);
    }

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
    // Command hooks run first: one may block or approve before anyone is
    // asked, and its rewritten arguments are what the rules then judge.
    let hooks: Box<dyn Hooks> = match extra_hooks {
        Some(factory) => Box::new(ChainedHooks::new(vec![factory(), Box::new(hooks)])),
        None => Box::new(hooks),
    };
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
/// reopened.
fn push_history(transcript: &mut Transcript, message: &Message) {
    match message {
        Message::User(user) => transcript.push(Item::User {
            text: user.plain_text(),
        }),
        Message::Assistant(assistant) => {
            for call in assistant.tool_calls() {
                transcript.push(Item::Tool {
                    call: call.clone(),
                    result: None,
                    live: None,
                    expanded: false,
                });
            }
            transcript.push(Item::Assistant {
                text: assistant.plain_text(),
                thinking_chars: 0,
                streaming: false,
                error: assistant.error_message.clone(),
            });
        }
        Message::ToolResult(result) => {
            let id = result.tool_call_id.clone();
            transcript.with_tool(&id, |item| {
                if let Item::Tool { result: slot, .. } = item {
                    *slot = Some(result.clone());
                }
            });
        }
    }
}

impl Panel for AgentPanel {
    fn name(&self) -> &'static str {
        "agent"
    }

    /// `Agent: <name>` for a named conversation, else `Agent: <first
    /// prompt>`, else `Agent: <working directory>`. The renderer shortens
    /// further from the left when the panel is narrow, so only a long name or
    /// prompt is cut here.
    fn title(&self) -> String {
        let t = termide_i18n::t();
        let named = self
            .session
            .as_ref()
            .and_then(Session::name)
            .map(truncate_title);
        let subject = named
            .or_else(|| {
                self.transcript.items().iter().find_map(|item| match item {
                    Item::User { text } => Some(truncate_title(text)),
                    _ => None,
                })
            })
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned());
        format!("{}: {subject}", t.panel_agent())
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
            AGENT_ACTION => vec![self.agent_picker()],
            MODEL_ACTION | MODE_ACTION if self.external => {
                self.notice(PromptError::Unsupported.to_string(), NoticeKind::Warn);
                vec![PanelEvent::NeedsRedraw]
            }
            MODEL_ACTION => self.request_model_list(),
            MODE_ACTION => vec![self.mode_picker()],
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
        // The agent's question sits between the separator and the input;
        // when the panel is too short for the card, the keys still answer.
        let form_rows = self
            .pending_permission
            .as_ref()
            .map_or(0, |(_, form)| form.height())
            .min(area.height.saturating_sub(input_rows + 2));
        let has_separator = area.height > input_rows + form_rows + 1;
        let transcript_height = area
            .height
            .saturating_sub(input_rows + form_rows)
            .saturating_sub(u16::from(has_separator));
        self.transcript_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: transcript_height,
        };
        self.input_area = Rect {
            x: area.x,
            y: area.y + area.height - input_rows,
            width: area.width,
            height: input_rows,
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
        let total = self.transcript.lines(text_width, &colors, is_light).len();
        let max_top = total.saturating_sub(transcript_height as usize);
        if self.follow {
            self.top = max_top;
        } else {
            self.top = self.top.min(max_top);
        }
        let lines = self.transcript.lines(text_width, &colors, is_light);
        for row in 0..transcript_height as usize {
            let Some(line) = lines.get(self.top + row) else {
                break;
            };
            buf.set_line(area.x, area.y + row as u16, line, text_width);
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
        self.render_input(input_area, buf, ctx.is_focused);
        if form_rows >= 3 {
            if let Some((_, form)) = &mut self.pending_permission {
                form.render(form_area, buf, &colors, ctx.is_focused);
            }
        }
        if ctx.is_focused && has_separator {
            // The completion list overlays the bottom of the transcript,
            // right above the separator.
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
        if let Some((_, form)) = &mut self.pending_permission {
            let action = if ctrl || alt {
                ChoiceAction::NotHandled
            } else {
                form.handle_key(key)
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
            if self.apply_permission_action(action.clone()) {
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
                let typed = self.input.text();
                let exact = key.code == KeyCode::Enter
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

        match key.code {
            KeyCode::Esc => {
                if self.is_busy() {
                    self.abort();
                } else if !self.input.is_empty() {
                    self.input = TextArea::new();
                    self.after_edit();
                } else {
                    return vec![];
                }
            }
            KeyCode::Enter if shift || alt => {
                self.input.insert_newline();
                self.after_edit();
            }
            KeyCode::Char('j') if ctrl => {
                self.input.insert_newline();
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
                if !self.input.move_up() && !self.recall(true) {
                    return vec![];
                }
            }
            KeyCode::Down => {
                if !self.input.move_down() && !self.recall(false) {
                    return vec![];
                }
            }
            KeyCode::Left => {
                self.input.move_left();
            }
            KeyCode::Right => {
                self.input.move_right();
            }
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Backspace => {
                self.input.backspace();
                self.after_edit();
            }
            KeyCode::Delete => {
                self.input.delete();
                self.after_edit();
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.input.insert(c);
                self.after_edit();
            }
            _ => return vec![],
        }
        vec![PanelEvent::NeedsRedraw]
    }

    fn captures_escape(&self) -> bool {
        self.pending_permission.is_some()
            || self.completion.is_some()
            || self.is_busy()
            || !self.input.is_empty()
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
                if let Some((_, form)) = &mut self.pending_permission {
                    let action = form.click(event.column, event.row);
                    if self.apply_permission_action(action) {
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
                    return vec![];
                }
                let line = self.top + (event.row - area.y) as usize;
                match self.transcript.item_at_line(line) {
                    Some(index) if self.transcript.toggle_expanded(index) => {}
                    _ => return vec![],
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
        if changed || !events.is_empty() {
            events.push(PanelEvent::NeedsRedraw);
        }
        events
    }

    fn handle_command(&mut self, cmd: PanelCommand<'_>) -> CommandResult {
        match cmd {
            PanelCommand::PasteText { text } => {
                self.input.insert_str(&text);
                self.after_edit();
                CommandResult::NeedsRedraw(true)
            }
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
            segments.push(StatusSegment::new("Context: ", SegmentKind::Label));
            let window = format_tokens(self.model.context_window);
            if self.context_tokens > 0 {
                let percent = self.context_tokens * 100 / self.model.context_window;
                let kind = if percent >= 80 {
                    SegmentKind::Warn
                } else {
                    SegmentKind::Value
                };
                segments.push(StatusSegment::new(format!("{percent}% of {window}"), kind));
            } else {
                segments.push(StatusSegment::new(window, SegmentKind::Value));
            }
        }
        if self.is_busy() {
            segments.push(sep());
            segments.push(StatusSegment::new("working", SegmentKind::Active));
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
            on_event(StreamEvent::TextDelta(reply.plain_text()));
            reply
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, String> {
            self.models.clone()
        }
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
            persist_rule: None,
            session_dir: None,
            session: None,
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
        assert!(matches!(&items[0], Item::User { text } if text == "hi there"));
        assert!(items.iter().any(|item| matches!(
            item,
            Item::Assistant { text, streaming: false, .. } if text == "Hello from the model"
        )));
        let rows = render_text(&mut panel, 40, 8);
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
            " Mode: ask │ Model: m │ Agent: default │ Context: 12% of 1k"
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
                "Show system prompt"
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
        assert!(matches!(&items[0], Item::User { text } if text == "first task"));
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
            if panel.pending_permission.is_some() {
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
            let (_, form) = panel.pending_permission.as_ref().unwrap();
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
        assert!(panel.pending_permission.is_none());
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
            while panel.pending_permission.is_none() {
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
            let (_, form) = panel.pending_permission.as_ref().unwrap();
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
        assert_eq!(options.len(), 3);
        assert!(options[1].starts_with("● accept-edits"), "{:?}", options[1]);
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

        // Cycling wraps, and a rebuilt agent starts in the chosen mode.
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
                provider: "scripted".into(),
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
        assert_eq!(
            labels.last().map(String::as_str),
            Some("Show system prompt")
        );

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
    fn slash_commands_expand_prompt_templates() {
        let mut expanding = panel(vec![reply("done")]);
        type_text(&mut expanding, "/review src/x.rs");
        expanding.handle_key(chord(KeyCode::Enter, KeyModifiers::NONE));
        settle(&mut expanding);
        let items = expanding.transcript().items();
        assert!(
            matches!(&items[0], Item::User { text } if text == "Review src/x.rs carefully."),
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
        assert!(matches!(&items[0], Item::User { text } if text == "hello"));
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
            Some(Item::User { text }) if text == "Review  carefully."
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
}
