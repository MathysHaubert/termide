//! Opening the coding agent panel: everything the panel needs is resolved
//! from configuration here, so the panel crate stays free of config and
//! filesystem policy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use termide_agent_acp::AcpRuntime;
use termide_agent_core::{
    build_system_prompt, discover_context_files, ensure_global_layout, Agent, AgentDirs,
    AgentEvent, AutoDenyPrompter, CancelToken, CompactionPolicy, Decision, Message, ModelSpec,
    PermissionHooks, PermissionRules, PromptOptions, Provider, Session, StopReason, StreamEvent,
    ToolRegistry, UserMessage, DEFAULT_AGENT, GLOBAL_AGENT_DIR, SESSIONS_DIR,
};
use termide_agent_core::{subject_of, Mode, ToolContext};
use termide_agent_hooks::CommandHooks;
use termide_agent_mcp::Connections;
use termide_agent_providers::{AnthropicProvider, Compat, OpenAiCompatProvider};
use termide_agent_tools::{builtin_tools, SkillTool, SubagentRun, TaskTool};
use termide_config::AgentSettings;
use termide_panel_agent::{
    AgentCatalog, AgentEntry, AgentPanel, AgentPanelSetup, AgentProfile, BackendFactory,
    HooksFactory,
};

use super::App;

impl App {
    /// Open a new agent panel. Like a new terminal, each call opens another
    /// one, so several agents can work in a project at once. Reports a status
    /// message instead of opening when no model is configured.
    pub(in crate::app) fn handle_open_agent(&mut self) -> Result<()> {
        self.close_help_panels();

        let settings = self.state.config.agent.clone();
        if settings.model.trim().is_empty() {
            let t = termide_i18n::t();
            self.show_error_modal(t.agent_not_configured().to_string());
            return Ok(());
        }

        // Like a new terminal, the agent works where the focused panel is
        // (a file manager's directory, an editor's file); the project root
        // when the panel has no directory of its own.
        let project_root = self.state.project_root.clone();
        let cwd = self
            .layout_manager
            .active_panel_mut()
            .and_then(|p| p.get_working_directory())
            .unwrap_or_else(|| project_root.clone());
        let panel = AgentPanel::new(agent_setup(
            &settings,
            cwd,
            &project_root,
            DEFAULT_AGENT,
            None,
        ));
        self.add_panel(Box::new(panel));
        self.auto_save_session();
        Ok(())
    }
}

/// Rebuild an agent panel saved in a project layout. `None` when no model
/// is configured any more; a session log that has gone missing starts a
/// fresh session in the same project, an agent definition that has gone
/// missing falls back to the default one.
pub(crate) fn restore_agent_panel(
    settings: &AgentSettings,
    cwd: PathBuf,
    session: Option<PathBuf>,
    agent: Option<String>,
) -> Option<AgentPanel> {
    if settings.model.trim().is_empty() {
        log::warn!("agent panel not restored: no agent.model configured");
        return None;
    }
    // Exclusive: if this session is already open in another restored panel,
    // fall back to a fresh one rather than back two panels with one log.
    let session = session.and_then(|path| match Session::open_exclusive(&path) {
        Ok(session) => Some(session),
        Err(error) => {
            log::warn!("cannot reopen agent session {}: {error}", path.display());
            None
        }
    });
    // termide's project root is the directory it was started in; the layout
    // restore runs off the App, so read it from the same source.
    let project_root = std::env::current_dir().unwrap_or_else(|_| cwd.clone());
    Some(AgentPanel::new(agent_setup(
        settings,
        cwd,
        &project_root,
        agent.as_deref().unwrap_or(DEFAULT_AGENT),
        session,
    )))
}

/// The agent definitions of one panel: `agents/<name>/` across the agent
/// directories of the panel's working directory, the project and the
/// configuration, turned into prompts and tool sets.
struct FsCatalog {
    cwd: PathBuf,
    project_root: PathBuf,
    dirs: AgentDirs,
    /// The panel's MCP servers; connected once, shared by every agent.
    mcp: Arc<Connections>,
    /// Builds and runs a named agent as a subagent for the `task` tool;
    /// set once the provider and settings are known.
    subagents: Option<Arc<Subagents>>,
}

impl FsCatalog {
    fn new(cwd: &Path, project_root: &Path) -> Self {
        let global_agent_dir = termide_config::get_config_dir()
            .ok()
            .map(|dir| dir.join(GLOBAL_AGENT_DIR));
        if let Some(global) = &global_agent_dir {
            if let Err(error) = ensure_global_layout(global) {
                log::warn!("cannot lay out {}: {error}", global.display());
            }
        }
        Self::with_global(cwd, project_root, global_agent_dir)
    }

    fn with_global(cwd: &Path, project_root: &Path, global_agent_dir: Option<PathBuf>) -> Self {
        let dirs = AgentDirs::new(cwd, Some(project_root), global_agent_dir.as_deref());
        Self {
            cwd: cwd.to_path_buf(),
            project_root: project_root.to_path_buf(),
            mcp: Connections::new(dirs.mcp_servers()),
            dirs,
            subagents: None,
        }
    }
}

impl AgentCatalog for FsCatalog {
    fn prompts(&self) -> Vec<termide_agent_core::PromptTemplate> {
        self.dirs.prompts()
    }

    fn commands(&self) -> Vec<termide_agent_core::CommandScript> {
        self.dirs.commands()
    }

    fn list(&self) -> Vec<AgentEntry> {
        self.dirs
            .agents()
            .into_iter()
            .map(|name| AgentEntry {
                description: self.dirs.spec(&name).description,
                name,
            })
            .collect()
    }

    /// The default agent always resolves; another name only when a root
    /// defines it.
    fn resolve(&self, name: &str) -> Option<AgentProfile> {
        if name != DEFAULT_AGENT && !self.dirs.agents().iter().any(|n| n == name) {
            return None;
        }
        let definition = self.dirs.agent(name);
        // An external agent brings its own tools; ours would only confuse it.
        let backend: Option<BackendFactory> = definition.spec.acp.clone().map(|config| {
            let agent = name.to_string();
            Arc::new(move |setup: termide_agent_core::BackendSetup| {
                AcpRuntime::start(&agent, &config, setup)
                    .map(|runtime| Box::new(runtime) as Box<dyn termide_agent_core::Backend>)
            }) as BackendFactory
        });
        let mut tools = if backend.is_some() {
            termide_agent_core::ToolRegistry::new()
        } else {
            builtin_tools()
        };
        restrict_tools(&mut tools, &definition.spec.tools, name);
        // Skills are instructions, not a capability, so an agent's `tools`
        // list does not govern them: the tool comes with the skills.
        let skills = self.dirs.skills();
        if !skills.is_empty() && backend.is_none() {
            tools.insert(Arc::new(SkillTool::new(skills.clone())));
        }
        // The `task` tool lets this agent hand work to the others; only when
        // there are custom agents to delegate to, and never for an external
        // agent (it drives its own tools) or a subagent (no nesting: the
        // subagent build path adds no task tool).
        if let Some(subagents) = &self.subagents {
            if backend.is_none() {
                let delegates = self.delegatable(name);
                if !delegates.is_empty() {
                    let runner = Arc::clone(subagents);
                    let run: SubagentRun = Arc::new(move |agent, prompt, cancel, on_update| {
                        runner.run(agent, prompt, cancel, on_update)
                    });
                    tools.insert(Arc::new(TaskTool::new(delegates, run)));
                }
            }
        }
        // The configuration's `ai/AGENTS.md` is the prompt template itself,
        // not an instruction file, so no global file joins the chain.
        let context_files = discover_context_files(&self.cwd, Some(&self.project_root), None);
        let mut options = PromptOptions::new(&self.cwd, &tools, &context_files);
        options.skills = &skills;
        options.soul = definition.soul.as_deref();
        Some(AgentProfile {
            system_prompt: build_system_prompt(&options),
            tools,
            model: definition.spec.model,
            mode: definition.spec.mode,
            late_tools: (!self.mcp.is_empty() && backend.is_none()).then(|| self.mcp.subscribe()),
            backend,
        })
    }
}

impl FsCatalog {
    /// The agents `caller` can delegate to with the `task` tool: every
    /// built-in-loop agent, the default one included, except `caller`
    /// itself (delegating to yourself is pointless) and external ones.
    fn delegatable(&self, caller: &str) -> Vec<(String, String)> {
        let mut names: Vec<String> = std::iter::once(DEFAULT_AGENT.to_string())
            .chain(self.dirs.agents())
            .collect();
        names.dedup();
        names
            .into_iter()
            .filter(|name| name != caller && self.dirs.spec(name).acp.is_none())
            .map(|name| {
                let description = self.dirs.spec(&name).description;
                (name, description)
            })
            .collect()
    }
}

/// Drop every tool not in `allowed`, warning about names that match none;
/// `None` keeps them all. Shared by the catalog and the subagent builder so
/// an agent's `tools` list means the same in both.
fn restrict_tools(tools: &mut ToolRegistry, allowed: &Option<Vec<String>>, agent: &str) {
    let Some(allowed) = allowed else { return };
    for unknown in allowed.iter().filter(|name| tools.get(name).is_none()) {
        log::warn!("agent {agent}: no tool named {unknown}");
    }
    for tool in tools
        .names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>()
    {
        if !allowed.contains(&tool) {
            tools.remove(&tool);
        }
    }
}

/// Builds a named agent and runs it to completion as a subagent: the engine
/// behind the `task` tool. It shares the provider and the permission rules
/// with the panel, but has no one to prompt, so anything the rules and mode
/// do not already allow is refused.
struct Subagents {
    provider: Arc<dyn Provider>,
    dirs: AgentDirs,
    cwd: PathBuf,
    project_root: PathBuf,
    rules: PermissionRules,
    default_model: String,
    context_window: u64,
    max_tokens: u64,
    reasoning: bool,
    compaction: CompactionPolicy,
}

/// A runaway subagent is cut off after this many model calls.
const SUBAGENT_MAX_TURNS: usize = 50;

impl Subagents {
    fn run(
        &self,
        name: &str,
        prompt: &str,
        cancel: &CancelToken,
        on_update: &mut dyn FnMut(termide_agent_core::ToolUpdate),
    ) -> Result<String, String> {
        let definition = self.dirs.agent(name);
        if definition.spec.acp.is_some() {
            return Err(format!(
                "{name} is an external agent and cannot be run as a subagent"
            ));
        }
        let mut tools = builtin_tools();
        restrict_tools(&mut tools, &definition.spec.tools, name);
        let skills = self.dirs.skills();
        if !skills.is_empty() {
            tools.insert(Arc::new(SkillTool::new(skills.clone())));
        }
        let context_files = discover_context_files(&self.cwd, Some(&self.project_root), None);
        let mut options = PromptOptions::new(&self.cwd, &tools, &context_files);
        options.skills = &skills;
        options.soul = definition.soul.as_deref();
        let system_prompt = build_system_prompt(&options);

        let model = ModelSpec {
            provider: "agent".to_string(),
            id: definition
                .spec
                .model
                .clone()
                .unwrap_or_else(|| self.default_model.clone()),
            context_window: self.context_window,
            max_tokens: self.max_tokens,
            reasoning: self.reasoning,
        };
        let mut rules = self.rules.clone();
        if let Some(mode) = definition.spec.mode {
            rules.mode = mode;
        }
        let mut agent = Agent::new(Arc::clone(&self.provider), tools, model, self.cwd.clone())
            .with_system_prompt(system_prompt)
            .with_compaction(self.compaction);
        let mut hooks = PermissionHooks::new(
            rules,
            Box::new(AutoDenyPrompter::new(
                "a subagent cannot prompt; it may only do what the permission rules and mode already allow",
            )),
        );

        // Mirror the sub-run's own progress up as it goes, and stop a run
        // that will not stop itself. The parent's cancel aborts it too.
        let budget = CancelToken::new();
        let mut turns = 0usize;
        let mut progress = String::new();
        {
            let budget = budget.clone();
            let mut emit = |event: AgentEvent| {
                match &event {
                    AgentEvent::MessageStart => {
                        turns += 1;
                        if turns > SUBAGENT_MAX_TURNS {
                            budget.cancel();
                        }
                    }
                    AgentEvent::MessageEnd(Message::Assistant(message)) => {
                        let text = message.plain_text();
                        if !text.trim().is_empty() {
                            if !progress.is_empty() {
                                progress.push_str(
                                    "

",
                                );
                            }
                            progress.push_str(text.trim());
                            on_update(termide_agent_core::ToolUpdate::Output(progress.clone()));
                        }
                    }
                    _ => {}
                }
                if cancel.is_cancelled() {
                    budget.cancel();
                }
            };
            agent.run(UserMessage::text(prompt), &mut hooks, &budget, &mut emit);
        }

        let answer = agent
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => {
                    let text = assistant.plain_text();
                    (!text.trim().is_empty()).then(|| text.trim().to_string())
                }
                _ => None,
            });
        match answer {
            Some(text) if turns > SUBAGENT_MAX_TURNS => Ok(format!(
                "{text}

(subagent stopped after {SUBAGENT_MAX_TURNS} steps)"
            )),
            Some(text) => Ok(text),
            None if cancel.is_cancelled() => Err("the subagent was stopped".into()),
            None => Err("the subagent produced no answer".into()),
        }
    }
}

/// Everything the panel needs, resolved from `settings` for a panel working
/// in `cwd` inside the termide project at `project_root` as the agent named
/// `agent`: the provider, the model, the tools, the system prompt and where
/// the session logs live.
fn agent_setup(
    settings: &AgentSettings,
    cwd: PathBuf,
    project_root: &Path,
    agent: &str,
    session: Option<Session>,
) -> AgentPanelSetup {
    let api_key = if settings.api_key_env.is_empty() {
        None
    } else {
        std::env::var(&settings.api_key_env).ok()
    };
    let provider: Arc<dyn Provider> = build_provider(settings, api_key);

    let mut catalog = FsCatalog::new(&cwd, project_root);
    // The subagent runner shares the provider, the rules and the model
    // defaults, so a delegated agent runs like the panel would run it.
    catalog.subagents = Some(Arc::new(Subagents {
        provider: Arc::clone(&provider) as Arc<dyn Provider>,
        dirs: catalog.dirs.clone(),
        cwd: cwd.clone(),
        project_root: project_root.to_path_buf(),
        rules: settings.permissions.clone(),
        default_model: settings.model.clone(),
        context_window: settings.effective_context_window(),
        max_tokens: settings.max_tokens,
        reasoning: settings.reasoning,
        compaction: settings.compaction,
    }));
    let compaction_prompts = catalog.dirs.compaction_prompts();
    let plan_prompt = catalog.dirs.plan_prompt();
    let hooks: Option<HooksFactory> = {
        let configs = catalog.dirs.hooks();
        let hook_cwd = cwd.clone();
        (!configs.is_empty()).then(|| {
            Arc::new(move || {
                Box::new(CommandHooks::new(configs.clone(), hook_cwd.clone()))
                    as Box<dyn termide_agent_core::Hooks>
            }) as HooksFactory
        })
    };
    let (agent, profile) = match catalog.resolve(agent) {
        Some(profile) => (agent.to_string(), profile),
        None => {
            log::warn!("no agent named {agent}; using {DEFAULT_AGENT}");
            let profile = catalog
                .resolve(DEFAULT_AGENT)
                .expect("the default agent always resolves");
            (DEFAULT_AGENT.to_string(), profile)
        }
    };
    let model = ModelSpec {
        provider: "agent".to_string(),
        id: profile.model.unwrap_or_else(|| settings.model.clone()),
        context_window: settings.effective_context_window(),
        max_tokens: settings.max_tokens,
        reasoning: settings.reasoning,
    };
    let mut rules = settings.permissions.clone();
    if let Some(mode) = profile.mode {
        rules.mode = mode;
    }

    // Session logs are filed by the directory the panel works in, under the
    // same `ai/` directory as the agents: `<config>/ai/sessions/<path>/`.
    let session_dir = termide_config::get_config_dir().ok().map(|dir| {
        dir.join(GLOBAL_AGENT_DIR)
            .join(SESSIONS_DIR)
            .join(termide_project::project_key(&cwd))
    });

    AgentPanelSetup {
        cwd,
        agent,
        catalog: Arc::new(catalog),
        late_tools: profile.late_tools,
        hooks,
        backend: profile.backend,
        provider,
        model,
        tools: profile.tools,
        rules,
        system_prompt: profile.system_prompt,
        compaction: settings.compaction,
        compaction_prompts,
        plan_prompt,
        autofold: settings.autofold,
        persist_rule: Some(persist_rule),
        session_dir,
        session,
    }
}

/// Run one agent task without the UI and stream the answer to stdout, for
/// scripting and CI: `termide --agent "..."`. Text goes to stdout, tool
/// activity and errors to stderr. There is no one to answer a permission
/// prompt, so it runs under the configured rules and mode with everything
/// else refused (as a subagent does); set `mode = "auto"` or add allow rules
/// for unattended use. Returns the process exit code.
/// How a headless run reports its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessOutput {
    /// The answer streamed to stdout, tool activity to stderr.
    Text,
    /// One JSON object printed at the end: answer, usage, tool calls, status.
    Json,
    /// One JSON object per event (NDJSON): a `tool_use`/`tool_result` per
    /// tool, a `message` per assistant turn, a final `result`.
    StreamJson,
}

pub fn run_agent_headless(
    settings: &AgentSettings,
    cwd: &Path,
    project_root: &Path,
    agent_name: Option<&str>,
    prompt: &str,
    output: HeadlessOutput,
) -> i32 {
    use std::io::Write;
    // Both JSON forms suppress the plain text/stderr chatter.
    let quiet = output != HeadlessOutput::Text;
    let stream = output == HeadlessOutput::StreamJson;

    if settings.model.trim().is_empty() {
        eprintln!("termide: no agent model configured (set [agent].model)");
        return 1;
    }
    let api_key = (!settings.api_key_env.is_empty())
        .then(|| std::env::var(&settings.api_key_env).ok())
        .flatten();
    let provider = build_provider(settings, api_key);

    let global = termide_config::get_config_dir()
        .ok()
        .map(|dir| dir.join(GLOBAL_AGENT_DIR));
    if let Some(global) = &global {
        let _ = ensure_global_layout(global);
    }
    let dirs = AgentDirs::new(cwd, Some(project_root), global.as_deref());
    let name = agent_name.unwrap_or(DEFAULT_AGENT);
    if agent_name.is_some_and(|n| n != DEFAULT_AGENT && !dirs.agents().iter().any(|a| a == n)) {
        eprintln!("termide: no agent named {name}");
        return 2;
    }
    let definition = dirs.agent(name);
    if definition.spec.acp.is_some() {
        eprintln!("termide: headless mode cannot drive an external (ACP) agent");
        return 2;
    }

    let mut tools = builtin_tools();
    restrict_tools(&mut tools, &definition.spec.tools, name);
    let skills = dirs.skills();
    if !skills.is_empty() {
        tools.insert(Arc::new(SkillTool::new(skills.clone())));
    }
    let context_files = discover_context_files(cwd, Some(project_root), None);
    let mut options = PromptOptions::new(cwd, &tools, &context_files);
    options.skills = &skills;
    options.soul = definition.soul.as_deref();
    let system_prompt = build_system_prompt(&options);

    let model = ModelSpec {
        provider: "agent".to_string(),
        id: definition
            .spec
            .model
            .clone()
            .unwrap_or_else(|| settings.model.clone()),
        context_window: settings.effective_context_window(),
        max_tokens: settings.max_tokens,
        reasoning: settings.reasoning,
    };
    let mut rules = settings.permissions.clone();
    if let Some(mode) = definition.spec.mode {
        rules.mode = mode;
    }
    // Plan mode is a UI affordance (it waits for a card); headless has no
    // one to accept a plan, so treat it as ask.
    if rules.mode == Mode::Plan {
        eprintln!("termide: plan mode has no meaning without the panel; using ask");
        rules.mode = Mode::Ask;
    }

    let mut agent = Agent::new(Arc::clone(&provider), tools, model, cwd.to_path_buf())
        .with_system_prompt(system_prompt)
        .with_compaction(settings.compaction)
        .with_compaction_prompts(dirs.compaction_prompts());
    let mut hooks = PermissionHooks::new(
        rules,
        Box::new(AutoDenyPrompter::new(
            "running headless with no one to ask; allowed only what the rules and mode permit",
        )),
    );

    let cancel = CancelToken::new();
    let stdout = std::io::stdout();
    let mut wrote_text = false;
    // Tool calls in call order: (id, name, subject, is_error), for the JSON
    // report and, in text mode, the stderr activity lines.
    let mut tools: Vec<(String, String, String, bool)> = Vec::new();
    {
        let line = |value: &serde_json::Value| {
            let mut out = stdout.lock();
            let _ = writeln!(out, "{value}");
            let _ = out.flush();
        };
        let mut emit = |event: AgentEvent| match event {
            AgentEvent::MessageUpdate(StreamEvent::TextDelta(text)) => {
                if !quiet {
                    let mut out = stdout.lock();
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.flush();
                    wrote_text = true;
                }
            }
            AgentEvent::MessageEnd(Message::Assistant(message)) if stream => {
                let text = message.plain_text();
                if !text.trim().is_empty() {
                    line(&serde_json::json!({ "type": "message", "text": text }));
                }
            }
            AgentEvent::ToolExecutionStart { call } => {
                let subject = subject_of(
                    &call,
                    &ToolContext {
                        cwd: cwd.to_path_buf(),
                    },
                );
                if !quiet {
                    if subject.is_empty() {
                        eprintln!("· {}", call.name);
                    } else {
                        eprintln!("· {} {subject}", call.name);
                    }
                }
                if stream {
                    line(&serde_json::json!({
                        "type": "tool_use",
                        "name": call.name,
                        "subject": subject,
                    }));
                }
                tools.push((call.id.clone(), call.name.clone(), subject, false));
            }
            AgentEvent::ToolExecutionEnd { result } => {
                let name = tools
                    .iter_mut()
                    .find(|t| t.0 == result.tool_call_id)
                    .map(|entry| {
                        entry.3 = result.is_error;
                        entry.1.clone()
                    })
                    .unwrap_or_default();
                if !quiet && result.is_error {
                    eprintln!("  ! {}", result.plain_text());
                }
                if stream {
                    line(&serde_json::json!({
                        "type": "tool_result",
                        "name": name,
                        "error": result.is_error,
                    }));
                }
            }
            _ => {}
        };
        agent.run(UserMessage::text(prompt), &mut hooks, &cancel, &mut emit);
    }
    if wrote_text {
        println!();
    }

    let last = agent
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        });
    let code = match last {
        Some(last) if last.error_message.is_some() => 1,
        Some(last) if last.stop_reason == StopReason::Aborted => 130,
        Some(_) => 0,
        None => 1,
    };
    if quiet {
        let mut report = serde_json::json!({
            "ok": code == 0,
            "answer": last.map(termide_agent_core::AssistantMessage::plain_text).unwrap_or_default(),
            "stop_reason": last.map(|m| stop_label(m.stop_reason)),
            "model": last.map(|m| m.model.clone()),
            "provider": last.map(|m| m.provider.clone()),
            "usage": last.map(|m| serde_json::json!({
                "input": m.usage.input,
                "output": m.usage.output,
                "cache_read": m.usage.cache_read,
                "cache_write": m.usage.cache_write,
            })),
            "tools": tools.iter().map(|(_, name, subject, is_error)| serde_json::json!({
                "name": name,
                "subject": subject,
                "error": is_error,
            })).collect::<Vec<_>>(),
            "error": last.and_then(|m| m.error_message.clone())
                .or_else(|| (last.is_none()).then(|| "the agent produced no answer".to_string())),
        });
        // In stream mode the report is the terminal event; tag it.
        if stream {
            report["type"] = serde_json::json!("result");
        }
        println!("{report}");
    } else {
        match last {
            Some(last) if last.error_message.is_some() => eprintln!(
                "termide: {}",
                last.error_message.as_deref().unwrap_or("the run failed")
            ),
            None => eprintln!("termide: the agent produced no answer"),
            _ => {}
        }
    }
    code
}

/// The wire label for a stop reason, for the JSON report.
fn stop_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "tool_use",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

/// The provider named by `settings.provider`: the Anthropic Messages API, or
/// the OpenAI-compatible endpoint for everything else. An unknown name falls
/// back to OpenAI-compatible with a warning.
fn build_provider(settings: &AgentSettings, api_key: Option<String>) -> Arc<dyn Provider> {
    match settings.provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" => {
            // The default base URL points at a local OpenAI server, which is
            // not Anthropic's; use the API root unless the user set another.
            let mut anthropic = if settings.base_url == default_openai_base_url() {
                AnthropicProvider::new("agent")
            } else {
                AnthropicProvider::with_base_url("agent", settings.base_url.clone())
            };
            anthropic = anthropic.with_api_key(api_key);
            Arc::new(anthropic)
        }
        other => {
            if !other.is_empty() && other != "openai" && other != "openai-compatible" {
                log::warn!("unknown agent provider {other:?}; using the OpenAI-compatible one");
            }
            Arc::new(
                OpenAiCompatProvider::new("agent", settings.base_url.clone())
                    .with_api_key(api_key)
                    .with_compat(Compat {
                        reasoning_effort: settings.reasoning,
                        ..Compat::default()
                    }),
            )
        }
    }
}

/// The shipped default base URL, used to tell "left at default" from "set on
/// purpose" when picking a provider.
fn default_openai_base_url() -> String {
    AgentSettings::default().base_url
}

/// Append an "allow always" rule to the project's `.termide/config.toml`.
///
/// Written as a plain TOML fragment rather than through the config writer:
/// the file is user-owned, and appending keeps its comments and layout intact.
fn persist_rule(tool: &str, pattern: &str, decision: Decision) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let dir = cwd.join(".termide");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!("cannot create {}: {error}", dir.display());
        return;
    }
    let value = match decision {
        Decision::Allow => "allow",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    };
    let block = format!(
        "\n# added by the agent panel\n[agent.permissions.{tool}]\n{} = \"{value}\"\n",
        toml_key(pattern)
    );
    let path = dir.join("config.toml");
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(block.as_bytes()) {
                log::warn!("cannot write {}: {error}", path.display());
            }
        }
        Err(error) => log::warn!("cannot open {}: {error}", path.display()),
    }
}

/// Quote a rule pattern as a TOML basic string.
fn toml_key(pattern: &str) -> String {
    let escaped = pattern.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_quoted_for_toml() {
        assert_eq!(toml_key("git push *"), "\"git push *\"");
        assert_eq!(toml_key("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_key("c:\\tmp"), "\"c:\\\\tmp\"");
    }
    #[test]
    fn restore_is_skipped_without_a_configured_model() {
        let settings = AgentSettings::default();
        assert!(settings.model.is_empty());
        assert!(restore_agent_panel(&settings, PathBuf::from("/tmp"), None, None).is_none());
    }

    /// `agent.toml` narrows the tools and names a model and a mode; a name no
    /// root defines does not resolve, the default always does.
    #[test]
    fn the_catalog_turns_definitions_into_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("ai");
        let review = global.join("agents/review");
        std::fs::create_dir_all(&review).unwrap();
        std::fs::write(review.join("SOUL.md"), "You review.\n\n{{tools}}\n").unwrap();
        std::fs::write(global.join("AGENTS.md"), "Root template.\n\n{{tools}}\n").unwrap();
        std::fs::write(
            review.join("agent.toml"),
            "description = \"Reviews diffs\"\nmodel = \"big\"\nmode = \"auto\"\ntools = [\"read\", \"bash\", \"nope\"]\n",
        )
        .unwrap();
        let catalog = FsCatalog::with_global(tmp.path(), tmp.path(), Some(global.clone()));

        let names: Vec<String> = catalog.list().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["default", "review"]);
        let review = catalog.resolve("review").unwrap();
        assert!(review.system_prompt.starts_with("You review.\n\n- read:"));
        assert_eq!(review.tools.names(), ["read", "bash"]);
        assert_eq!(review.model.as_deref(), Some("big"));
        assert_eq!(review.mode, Some(termide_agent_core::Mode::Auto));

        let default = catalog.resolve(DEFAULT_AGENT).unwrap();
        assert_eq!(default.tools.len(), 4);
        assert!(default
            .system_prompt
            .starts_with("Root template.\n\n- read:"));
        assert!(default.model.is_none());
        assert!(catalog.resolve("missing").is_none());

        // A skill adds the `skill` tool, to every agent, and a prompt line.
        let skill = global.join("skills/deploy");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nSteps.\n",
        )
        .unwrap();
        std::fs::write(global.join("AGENTS.md"), "{{skills}}\n").unwrap();
        let review = catalog.resolve("review").unwrap();
        assert_eq!(review.tools.names(), ["read", "bash", "skill"]);

        // An [acp] table makes an external agent: no tools of ours, a backend.
        let outside = global.join("agents/outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("agent.toml"),
            "description = \"Claude Code\"\n[acp]\ncommand = \"npx\"\nargs = [\"-y\", \"@zed-industries/claude-code-acp\"]\n",
        )
        .unwrap();
        let outside = catalog.resolve("outside").unwrap();
        assert!(outside.backend.is_some());
        assert!(outside.tools.is_empty());
        assert!(outside.late_tools.is_none());
        let default = catalog.resolve(DEFAULT_AGENT).unwrap();
        assert_eq!(default.system_prompt, "- deploy: Ship it\n");
    }
}
