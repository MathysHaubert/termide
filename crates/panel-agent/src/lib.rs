//! The coding agent panel: a transcript above a multi-line input, tool calls
//! collapsed to one line each, permission prompts routed through termide's
//! selection modal.
//!
//! The panel owns an [`AgentRuntime`] and mirrors its events into a
//! [`Transcript`] from `tick()`, so it never blocks the UI thread. Every
//! transcript change also goes to the JSONL [`Session`] when one is attached.

mod prompter;
mod transcript;

use std::any::Any;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use termide_agent_core::{
    civil_date, Agent, AgentEvent, AgentRuntime, CancelToken, CompactionPolicy, Decision, Message,
    ModelSpec, PermissionAnswer, PermissionHooks, PermissionRules, PersistRule, Provider, Session,
    SessionSummary, StreamEvent, ToolRegistry, ToolUpdate, UserMessage,
};
use termide_config::Config;
use termide_core::{
    CommandResult, InputAction, KeyChord, Panel, PanelCommand, PanelEvent, RenderContext,
    ScrollAxis, ScrollBars, SegmentKind, SelectAction, StatusSegment, ThemeColors, WidthPreference,
};
use termide_theme::Theme;
use termide_ui::textarea::TextArea;
use termide_ui::ScrollBar;

use prompter::PermissionEnvelope;
pub use transcript::{Item, NoticeKind, Transcript};

/// Prefix of the `SelectAction::Custom` payload for permission prompts.
const PERMISSION_ACTION_PREFIX: &str = "agent-permission:";
/// Labels of the four permission answers, in the order the modal shows them.
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

/// Everything the app resolves from configuration before opening the panel.
///
/// The panel keeps these so it can rebuild its agent when the user switches
/// to another session.
pub struct AgentPanelSetup {
    pub cwd: PathBuf,
    pub provider: Arc<dyn Provider>,
    pub model: ModelSpec,
    pub tools: ToolRegistry,
    pub rules: PermissionRules,
    pub system_prompt: String,
    pub compaction: CompactionPolicy,
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

pub struct AgentPanel {
    runtime: AgentRuntime,
    permission_rx: Receiver<PermissionEnvelope>,
    pending_permission: Option<PermissionEnvelope>,
    session: Option<Session>,
    session_dir: Option<PathBuf>,
    /// Sessions offered by the last picker, in the order they were shown.
    session_choices: Vec<SessionSummary>,
    cwd: PathBuf,
    model: ModelSpec,
    mode_label: &'static str,

    // Kept to rebuild the agent when switching sessions.
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    rules: PermissionRules,
    system_prompt: String,
    compaction: CompactionPolicy,
    persist_rule: Option<PersistFn>,

    transcript: Transcript,
    input: TextArea,
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
        let mode_label = match setup.rules.mode {
            termide_agent_core::Mode::Ask => "ask",
            termide_agent_core::Mode::AcceptEdits => "accept-edits",
            termide_agent_core::Mode::Auto => "auto",
        };
        let session = setup.session.or_else(|| {
            let dir = setup.session_dir.as_ref()?;
            match Session::create(dir, &setup.cwd) {
                Ok(session) => Some(session),
                Err(error) => {
                    log::warn!("cannot start an agent session log: {error}");
                    None
                }
            }
        });
        let (runtime, permission_rx, transcript) = spawn_runtime(
            &setup.provider,
            &setup.tools,
            &setup.model,
            &setup.cwd,
            &setup.system_prompt,
            setup.rules.clone(),
            setup.compaction,
            setup.persist_rule,
            session.as_ref(),
        );
        Self {
            runtime,
            permission_rx,
            pending_permission: None,
            session,
            session_dir: setup.session_dir,
            session_choices: Vec::new(),
            cwd: setup.cwd,
            model: setup.model,
            mode_label,
            provider: setup.provider,
            tools: setup.tools,
            rules: setup.rules,
            system_prompt: setup.system_prompt,
            compaction: setup.compaction,
            persist_rule: setup.persist_rule,
            transcript,
            input: TextArea::new(),
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
            let dir = self.session_dir.as_ref()?;
            match Session::create(dir, &self.cwd) {
                Ok(session) => Some(session),
                Err(error) => {
                    log::warn!("cannot start an agent session log: {error}");
                    None
                }
            }
        });
        let (runtime, permission_rx, transcript) = spawn_runtime(
            &self.provider,
            &self.tools,
            &self.model,
            &self.cwd,
            &self.system_prompt,
            self.rules.clone(),
            self.compaction,
            self.persist_rule,
            session.as_ref(),
        );
        // Dropping the old runtime cancels it and asks its worker to stop.
        self.runtime = runtime;
        self.permission_rx = permission_rx;
        self.pending_permission = None;
        self.transcript = transcript;
        self.session = session;
        self.input = TextArea::new();
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
        self.input = TextArea::new();
        self.follow = true;
        let message = UserMessage::text(text);
        if self.is_busy() {
            self.runtime.steer(message);
            self.queued = self.runtime.queues().lens();
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
                self.queued = self.runtime.queues().lens();
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
            // No transcript notice: the modal is app-global and the tool
            // line already shows the call as pending, so a notice would only
            // linger misleadingly once the prompt is answered.
            let request = &envelope.request;
            events.push(PanelEvent::ShowSelect {
                title: format!("Agent wants to run {}: {}", request.tool, request.subject),
                options: PERMISSION_OPTIONS
                    .iter()
                    .enumerate()
                    .map(|(index, label)| {
                        if index == 2 {
                            format!("{label} ({})", request.suggested_pattern)
                        } else {
                            label.to_string()
                        }
                    })
                    .collect(),
                on_select: SelectAction::Custom(format!(
                    "{PERMISSION_ACTION_PREFIX}{}",
                    envelope.id
                )),
            });
            self.pending_permission = Some(envelope);
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

    /// Answer the outstanding prompt; `true` when `action` was ours.
    pub fn answer_permission(&mut self, action: &str, index: usize) -> bool {
        let Some(id) = action.strip_prefix(PERMISSION_ACTION_PREFIX) else {
            return false;
        };
        let Some(pending) = self.pending_permission.take() else {
            return false;
        };
        if id != pending.id.to_string() {
            self.pending_permission = Some(pending);
            return false;
        }
        let answer = match index {
            0 => PermissionAnswer::AllowOnce,
            1 => PermissionAnswer::AllowSession,
            2 => PermissionAnswer::AllowAlways,
            _ => PermissionAnswer::Deny,
        };
        let _ = pending.reply.send(answer);
        true
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

/// Spawn an agent worker, returning it with its permission channel and a
/// transcript mirroring `session`'s history.
#[allow(clippy::too_many_arguments)]
fn spawn_runtime(
    provider: &Arc<dyn Provider>,
    tools: &ToolRegistry,
    model: &ModelSpec,
    cwd: &std::path::Path,
    system_prompt: &str,
    rules: PermissionRules,
    compaction: CompactionPolicy,
    persist_rule: Option<PersistFn>,
    session: Option<&Session>,
) -> (AgentRuntime, Receiver<PermissionEnvelope>, Transcript) {
    let cancel = CancelToken::new();
    let (prompter, permission_rx) = prompter::channel(cancel.clone());
    let mut hooks = PermissionHooks::new(rules, Box::new(prompter));
    if let Some(persist) = persist_rule {
        hooks = hooks.with_persist(Box::new(persist) as PersistRule);
    }
    let mut agent = Agent::new(
        Arc::clone(provider),
        tools.clone(),
        model.clone(),
        cwd.to_path_buf(),
    )
    .with_system_prompt(system_prompt)
    .with_compaction(compaction);

    let mut transcript = Transcript::default();
    if let Some(session) = session {
        let messages = session.context_messages();
        for message in &messages {
            push_history(&mut transcript, message);
        }
        agent = agent.with_messages(messages);
    }
    let runtime = AgentRuntime::spawn_with_cancel(agent, Box::new(hooks), cancel);
    (runtime, permission_rx, transcript)
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
        let has_separator = area.height > input_rows + 1;
        let transcript_height = area
            .height
            .saturating_sub(input_rows)
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
            let y = self.input_area.y - 1;
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
    }

    fn handle_key(&mut self, chord: KeyChord) -> Vec<PanelEvent> {
        let key = chord.raw;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = (self.viewport_height() as i32 - 1).max(1);

        match key.code {
            KeyCode::Esc => {
                if self.is_busy() {
                    self.abort();
                } else if !self.input.is_empty() {
                    self.input = TextArea::new();
                } else {
                    return vec![];
                }
            }
            KeyCode::Enter if shift || alt => self.input.insert_newline(),
            KeyCode::Char('j') if ctrl => self.input.insert_newline(),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('o') if ctrl => {
                let expand = !self.transcript.any_expanded();
                self.transcript.set_all_expanded(expand);
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
                self.input.move_up();
            }
            KeyCode::Down => {
                self.input.move_down();
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
            }
            KeyCode::Delete => {
                self.input.delete();
            }
            KeyCode::Char(c) if !ctrl && !alt => self.input.insert(c),
            _ => return vec![],
        }
        vec![PanelEvent::NeedsRedraw]
    }

    fn captures_escape(&self) -> bool {
        self.is_busy() || !self.input.is_empty()
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
        if changed || !events.is_empty() {
            events.push(PanelEvent::NeedsRedraw);
        }
        events
    }

    fn handle_command(&mut self, cmd: PanelCommand<'_>) -> CommandResult {
        match cmd {
            PanelCommand::PasteText { text } => {
                self.input.insert_str(&text);
                CommandResult::NeedsRedraw(true)
            }
            PanelCommand::SelectionMade { action, index } if action == RESUME_ACTION => {
                CommandResult::Handled(self.resume_choice(index))
            }
            PanelCommand::SelectionMade { action, index } => {
                CommandResult::Handled(self.answer_permission(&action, index))
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
        let mut segments = vec![
            StatusSegment::new(" ", SegmentKind::Label),
            StatusSegment::new("Mode: ", SegmentKind::Label),
            StatusSegment::new(self.mode_label, SegmentKind::Value),
            sep(),
            StatusSegment::new("Model: ", SegmentKind::Label),
            StatusSegment::new(self.model.id.clone(), SegmentKind::Value),
        ];
        if self.model.context_window > 0 && self.context_tokens > 0 {
            let percent = self.context_tokens * 100 / self.model.context_window;
            let kind = if percent >= 80 {
                SegmentKind::Warn
            } else {
                SegmentKind::Value
            };
            segments.push(sep());
            segments.push(StatusSegment::new("Context: ", SegmentKind::Label));
            segments.push(StatusSegment::new(format!("{percent}%"), kind));
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
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use termide_agent_core::PermissionPrompter;
    use termide_agent_core::{AssistantContent, AssistantMessage, Request, StopReason, Usage};
    use termide_core::PanelConfig;

    /// Replays one scripted assistant message per model call.
    struct Scripted(Mutex<Vec<AssistantMessage>>);

    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn stream(
            &self,
            _request: &Request<'_>,
            on_event: &mut dyn FnMut(StreamEvent),
            _cancel: &CancelToken,
        ) -> AssistantMessage {
            let mut replies = self.0.lock().unwrap();
            if replies.is_empty() {
                return AssistantMessage::failed("scripted", "m", StopReason::Error, "exhausted");
            }
            let reply = replies.remove(0);
            on_event(StreamEvent::TextDelta(reply.plain_text()));
            reply
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
        AgentPanelSetup {
            cwd: PathBuf::from("/tmp"),
            provider: Arc::new(Scripted(Mutex::new(replies))),
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
        assert_eq!(chips, " Mode: ask │ Model: m │ Context: 12%");
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
            vec!["Rename session", "New session", "Open session"]
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
    fn permission_prompt_round_trips_through_the_selection_command() {
        let mut panel = panel(vec![]);
        let (mut prompter, rx) = prompter::channel(CancelToken::new());
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

        let deadline = Instant::now() + Duration::from_secs(5);
        let action = loop {
            let events = panel.tick();
            if let Some(PanelEvent::ShowSelect {
                on_select,
                options,
                title,
            }) = events
                .iter()
                .find(|e| matches!(e, PanelEvent::ShowSelect { .. }))
            {
                assert!(title.contains("git push"));
                assert_eq!(options[2], "Allow always (git push *)");
                let SelectAction::Custom(action) = on_select else {
                    panic!("custom action expected");
                };
                break action.clone();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(action.starts_with(PERMISSION_ACTION_PREFIX));
        assert!(
            !panel.answer_permission("agent-permission:999", 0),
            "wrong id is ignored"
        );
        let result = panel.handle_command(PanelCommand::SelectionMade { action, index: 1 });
        assert!(matches!(result, CommandResult::Handled(true)));
        assert_eq!(worker.join().unwrap(), PermissionAnswer::AllowSession);
    }
}
