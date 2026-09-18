//! Opening the coding agent panel: everything the panel needs is resolved
//! from configuration here, so the panel crate stays free of config and
//! filesystem policy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use termide_agent_acp::AcpRuntime;
use termide_agent_core::{
    build_system_prompt, discover_context_files, ensure_global_layout, AgentDirs, Decision,
    ModelSpec, PromptOptions, Session, DEFAULT_AGENT, GLOBAL_AGENT_DIR, SESSIONS_DIR,
};
use termide_agent_hooks::CommandHooks;
use termide_agent_mcp::Connections;
use termide_agent_providers::{Compat, OpenAiCompatProvider};
use termide_agent_tools::{builtin_tools, SkillTool};
use termide_config::AgentSettings;
use termide_panel_agent::{
    AgentCatalog, AgentEntry, AgentPanel, AgentPanelSetup, AgentProfile, BackendFactory,
    HooksFactory,
};

use super::App;

impl App {
    /// Open the agent panel (singleton). Reports a status message instead of
    /// opening when no model is configured.
    pub(in crate::app) fn handle_open_agent(&mut self) -> Result<()> {
        self.close_help_panels();
        if self.find_and_focus_panel_by_name("agent") {
            return Ok(());
        }

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
    let session = session.and_then(|path| match Session::open(&path) {
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
        if let Some(allowed) = &definition.spec.tools {
            for unknown in allowed.iter().filter(|name| tools.get(name).is_none()) {
                log::warn!("agent {name}: no tool named {unknown}");
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
        // Skills are instructions, not a capability, so an agent's `tools`
        // list does not govern them: the tool comes with the skills.
        let skills = self.dirs.skills();
        if !skills.is_empty() && backend.is_none() {
            tools.insert(Arc::new(SkillTool::new(skills.clone())));
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
    let provider = Arc::new(
        OpenAiCompatProvider::new("agent", settings.base_url.clone())
            .with_api_key(api_key)
            .with_compat(Compat {
                reasoning_effort: settings.reasoning,
                ..Compat::default()
            }),
    );

    let catalog = FsCatalog::new(&cwd, project_root);
    let compaction_prompts = catalog.dirs.compaction_prompts();
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
        context_window: settings.context_window,
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
        persist_rule: Some(persist_rule),
        session_dir,
        session,
    }
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
