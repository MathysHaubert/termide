//! Opening the coding agent panel: everything the panel needs is resolved
//! from configuration here, so the panel crate stays free of config and
//! filesystem policy.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use termide_agent_core::{
    build_system_prompt, discover_context_files, Decision, ModelSpec, PromptOptions, Session,
};
use termide_agent_providers::{Compat, OpenAiCompatProvider};
use termide_agent_tools::builtin_tools;
use termide_config::AgentSettings;
use termide_panel_agent::{AgentPanel, AgentPanelSetup};

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

        let cwd = std::env::current_dir().unwrap_or_default();
        let panel = AgentPanel::new(agent_setup(&settings, cwd, None));
        self.add_panel(Box::new(panel));
        self.auto_save_session();
        Ok(())
    }
}

/// Rebuild an agent panel saved in a project layout. `None` when no model
/// is configured any more; a session log that has gone missing starts a
/// fresh session in the same project.
pub(crate) fn restore_agent_panel(
    settings: &AgentSettings,
    cwd: PathBuf,
    session: Option<PathBuf>,
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
    Some(AgentPanel::new(agent_setup(settings, cwd, session)))
}

/// Everything the panel needs, resolved from `settings` for a project at
/// `cwd`: the provider, the model, the tools, the system prompt and where the
/// session logs live.
fn agent_setup(
    settings: &AgentSettings,
    cwd: PathBuf,
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
    let model = ModelSpec {
        provider: "agent".to_string(),
        id: settings.model.clone(),
        context_window: settings.context_window,
        max_tokens: settings.max_tokens,
        reasoning: settings.reasoning,
    };

    let tools = builtin_tools();
    let global_context = termide_config::get_config_dir()
        .ok()
        .map(|dir| dir.join("AGENTS.md"));
    let context_files = discover_context_files(&cwd, global_context.as_deref());
    let system_prompt = build_system_prompt(&PromptOptions::new(&cwd, &tools, &context_files));

    // Agent sessions live beside the layout of the same project, under
    // `<data>/projects/<project>/agent/`.
    let session_dir = termide_project::Session::get_project_dir(&cwd)
        .ok()
        .map(|dir| dir.join("agent"));

    AgentPanelSetup {
        cwd,
        provider,
        model,
        tools,
        rules: settings.permissions.clone(),
        system_prompt,
        compaction: settings.compaction,
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
        assert!(restore_agent_panel(&settings, PathBuf::from("/tmp"), None).is_none());
    }
}
