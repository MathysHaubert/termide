//! Where the agent's own files live.
//!
//! Three roots, highest priority first: `.termide/ai/` in the directory the
//! panel works in, the same in the directory termide was opened in (when that
//! is another directory), and `ai/` in the user's configuration directory.
//! A file is taken from the first root that has it; a directory of named
//! entries (agents, skills, prompts) is the union of all roots, a name from a
//! higher root hiding the same name below.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::SEED_TEMPLATE;
use crate::hooks::{HookConfig, HOOKS_FILE};
use crate::mcp::{McpServerConfig, MCP_FILE};
use crate::permissions::Mode;

/// The `ai` directory inside the configuration directory.
pub const GLOBAL_AGENT_DIR: &str = "ai";
/// The `ai` directory inside a project or working directory.
pub const PROJECT_AGENT_DIR: &str = ".termide/ai";
/// The system prompt template of the default agent, at the root of an `ai`
/// directory; also what a custom agent without a `SOUL.md` uses.
pub const ROOT_SOUL_FILE: &str = "AGENTS.md";
/// The system prompt template of a custom agent: `agents/<name>/SOUL.md`.
pub const SOUL_FILE: &str = "SOUL.md";
/// The settings of an agent: `agents/<name>/agent.toml`.
pub const SPEC_FILE: &str = "agent.toml";
/// The agent used when none is chosen.
pub const DEFAULT_AGENT: &str = "default";
/// Session logs under the `ai` directory: `sessions/<working directory>/`.
pub const SESSIONS_DIR: &str = "sessions";
/// Skills under an `ai` directory: `skills/<name>/SKILL.md`.
pub const SKILLS_DIR: &str = "skills";
/// The cross-agent skills directory of a project (agentskills.io), read
/// beside termide's own so skills written for other agents work unchanged.
pub const SHARED_SKILLS_DIR: &str = ".agents/skills";
/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";
/// Prompt templates under an `ai` directory: `prompts/<name>.md`, typed as
/// `/<name>` in the panel.
pub const PROMPTS_DIR: &str = "prompts";

/// One prompt template: `/<name> args` in the input expands to `body` with
/// `$ARGUMENTS` and `$1`…`$9` filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    /// What to type after the name, for the picker (`argument-hint`).
    pub argument_hint: String,
    pub body: String,
}

impl PromptTemplate {
    /// The body with `args` substituted: `$ARGUMENTS` takes the whole
    /// string, `$1`…`$9` its whitespace-separated words. A body without any
    /// placeholder gets non-empty `args` appended on a line of their own, so
    /// `/review src/x.rs` works with a template that never mentions
    /// arguments (Claude Code's and Codex's behaviour).
    #[must_use]
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let words: Vec<&str> = args.split_whitespace().collect();
        let mut out = String::with_capacity(self.body.len() + args.len());
        let mut used = false;
        let mut rest = self.body.as_str();
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let tail = &rest[i + 1..];
            if let Some(after) = tail.strip_prefix("ARGUMENTS") {
                out.push_str(args);
                used = true;
                rest = after;
            } else if let Some(digit) = tail.chars().next().filter(char::is_ascii_digit) {
                let index = digit.to_digit(10).unwrap_or(0) as usize;
                if index >= 1 {
                    out.push_str(words.get(index - 1).copied().unwrap_or(""));
                    used = true;
                } else {
                    out.push_str("$0");
                }
                rest = &tail[1..];
            } else {
                out.push('$');
                rest = tail;
            }
        }
        out.push_str(rest);
        let mut out = out.trim_end().to_string();
        if !used && !args.is_empty() {
            out.push_str("\n\n");
            out.push_str(args);
        }
        out
    }
}

/// One skill as the prompt lists it: name, one-line description and where
/// its `SKILL.md` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Split the YAML front matter (`---` fenced `key: value` lines) off a
/// Markdown file. Returns the fields and the body; a file without front
/// matter is all body.
#[must_use]
pub fn split_front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut fields = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---") else {
        return (fields, text);
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (fields, text);
    };
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        fields.insert(key.trim().to_string(), value.to_string());
    }
    let body = &rest[end + 4..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    (fields, body)
}

/// Lay out the `ai` directory of the configuration so the prompt is a file
/// one can read and edit: `AGENTS.md` seeded from the shipped template when
/// no such file exists yet, plus empty `agents/`, `skills/` and `prompts/`.
/// Files already there are left alone, so the call is safe on every start.
pub fn ensure_global_layout(global: &Path) -> std::io::Result<()> {
    for dir in ["agents", "skills", "prompts"] {
        std::fs::create_dir_all(global.join(dir))?;
    }
    let soul = global.join(ROOT_SOUL_FILE);
    if !soul.exists() {
        std::fs::write(&soul, SEED_TEMPLATE)?;
    }
    Ok(())
}

/// `agents/<name>/agent.toml`: what sets an agent apart from the configured
/// defaults. Every field is optional; an absent one keeps the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    /// One line for the agent picker.
    #[serde(default)]
    pub description: String,
    /// Model id at the configured endpoint.
    #[serde(default)]
    pub model: Option<String>,
    /// Permission mode the agent starts in.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Tools the agent may use, by name; all built-in tools when absent.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

/// An agent as the roots define it: its prompt template and its settings,
/// each from the highest root that has the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub soul: Option<String>,
    pub spec: AgentSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDirs {
    roots: Vec<PathBuf>,
    /// Skill directories in priority order: each level's `ai/skills`
    /// followed by its `.agents/skills`.
    skill_roots: Vec<PathBuf>,
}

impl AgentDirs {
    /// `cwd` is where the panel works, `project_root` where termide was
    /// opened, `global` the agent directory under the configuration
    /// directory. Roots that do not exist yet are kept: they say where a
    /// file would be looked for.
    #[must_use]
    pub fn new(cwd: &Path, project_root: Option<&Path>, global: Option<&Path>) -> Self {
        let mut roots = vec![cwd.join(PROJECT_AGENT_DIR)];
        let mut skill_roots = vec![
            cwd.join(PROJECT_AGENT_DIR).join(SKILLS_DIR),
            cwd.join(SHARED_SKILLS_DIR),
        ];
        if let Some(root) = project_root {
            let dir = root.join(PROJECT_AGENT_DIR);
            if !roots.contains(&dir) {
                roots.push(dir.clone());
                skill_roots.push(dir.join(SKILLS_DIR));
                skill_roots.push(root.join(SHARED_SKILLS_DIR));
            }
        }
        if let Some(global) = global {
            roots.push(global.to_path_buf());
            skill_roots.push(global.join(SKILLS_DIR));
        }
        Self { roots, skill_roots }
    }

    /// Command hooks from every root's `hooks.toml`, by name; a higher root's
    /// table for a name wins, and `enabled = false` there drops the hook.
    /// They run in name order.
    #[must_use]
    pub fn hooks(&self) -> BTreeMap<String, HookConfig> {
        let mut hooks: BTreeMap<String, HookConfig> = BTreeMap::new();
        for root in &self.roots {
            let path = root.join(HOOKS_FILE);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed: BTreeMap<String, HookConfig> = match toml::from_str(&text) {
                Ok(parsed) => parsed,
                Err(error) => {
                    log::warn!("ignoring {}: {error}", path.display());
                    continue;
                }
            };
            for (name, config) in parsed {
                hooks.entry(name).or_insert(config);
            }
        }
        hooks.retain(|_, config| config.enabled);
        hooks
    }

    /// MCP servers from every root's `mcp.toml`, by name; a higher root's
    /// table for a name wins, and `enabled = false` there drops the server.
    /// A file that does not parse is reported and skipped.
    #[must_use]
    pub fn mcp_servers(&self) -> BTreeMap<String, McpServerConfig> {
        let mut servers: BTreeMap<String, McpServerConfig> = BTreeMap::new();
        for root in &self.roots {
            let path = root.join(MCP_FILE);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed: BTreeMap<String, McpServerConfig> = match toml::from_str(&text) {
                Ok(parsed) => parsed,
                Err(error) => {
                    log::warn!("ignoring {}: {error}", path.display());
                    continue;
                }
            };
            for (name, config) in parsed {
                servers.entry(name).or_insert(config);
            }
        }
        servers.retain(|_, config| config.enabled);
        servers
    }

    /// Every prompt template the roots define (`prompts/<name>.md`), by
    /// name, sorted; a higher root hides a lower one. The name is the file
    /// name, description and argument hint come from the front matter.
    #[must_use]
    pub fn prompts(&self) -> Vec<PromptTemplate> {
        self.merged_entries(PROMPTS_DIR)
            .into_iter()
            .filter_map(|(file, path)| {
                let name = file.strip_suffix(".md")?.to_string();
                if name.is_empty() || !path.is_file() {
                    return None;
                }
                let text = std::fs::read_to_string(&path).ok()?;
                let (fields, body) = split_front_matter(&text);
                Some(PromptTemplate {
                    name,
                    description: fields.get("description").cloned().unwrap_or_default(),
                    argument_hint: fields.get("argument-hint").cloned().unwrap_or_default(),
                    body: body.trim().to_string(),
                })
            })
            .collect()
    }

    /// Every skill the roots define, by name, sorted; a name in a higher
    /// root hides the same name below. A skill is a directory holding a
    /// `SKILL.md`; the name and description come from its front matter, the
    /// directory name standing in for a missing name.
    #[must_use]
    pub fn skills(&self) -> Vec<SkillInfo> {
        let mut skills: BTreeMap<String, SkillInfo> = BTreeMap::new();
        for root in &self.skill_roots {
            let Ok(read_dir) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let path = entry.path().join(SKILL_FILE);
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (fields, _) = split_front_matter(&text);
                let dir_name = entry.file_name().to_string_lossy().into_owned();
                let name = fields
                    .get("name")
                    .filter(|n| !n.is_empty())
                    .cloned()
                    .unwrap_or(dir_name);
                skills.entry(name.clone()).or_insert(SkillInfo {
                    name,
                    description: fields.get("description").cloned().unwrap_or_default(),
                    path,
                });
            }
        }
        skills.into_values().collect()
    }

    /// Roots in priority order.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The first root that has a file at `relative`.
    #[must_use]
    pub fn find_file(&self, relative: impl AsRef<Path>) -> Option<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.join(relative.as_ref()))
            .find(|path| path.is_file())
    }

    /// Entries of the directory `relative` across all roots, by name. A name
    /// present in several roots resolves to the highest one.
    #[must_use]
    pub fn merged_entries(&self, relative: impl AsRef<Path>) -> BTreeMap<String, PathBuf> {
        let mut entries = BTreeMap::new();
        for root in &self.roots {
            let Ok(read_dir) = std::fs::read_dir(root.join(relative.as_ref())) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                entries.entry(name).or_insert_with(|| entry.path());
            }
        }
        entries
    }

    /// The settings of `agent`; defaults when no root has an `agent.toml`
    /// or the file does not parse (which is logged).
    #[must_use]
    pub fn spec(&self, agent: &str) -> AgentSpec {
        let Some(path) = self.find_file(Path::new("agents").join(agent).join(SPEC_FILE)) else {
            return AgentSpec::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                log::warn!("cannot read {}: {error}", path.display());
                return AgentSpec::default();
            }
        };
        match toml::from_str(&text) {
            Ok(spec) => spec,
            Err(error) => {
                log::warn!("ignoring {}: {error}", path.display());
                AgentSpec::default()
            }
        }
    }

    /// Prompt template and settings of `agent` together.
    #[must_use]
    pub fn agent(&self, agent: &str) -> AgentDefinition {
        AgentDefinition {
            name: agent.to_string(),
            soul: self.soul(agent),
            spec: self.spec(agent),
        }
    }

    /// Names of the agents any root defines, plus the default one, which
    /// exists even without files.
    #[must_use]
    pub fn agents(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .merged_entries("agents")
            .into_iter()
            .filter(|(_, path)| path.is_dir())
            .map(|(name, _)| name)
            .collect();
        if !names.iter().any(|n| n == DEFAULT_AGENT) {
            names.insert(0, DEFAULT_AGENT.to_string());
        }
        names
    }

    /// The system prompt template of `agent`: its own `agents/<name>/SOUL.md`
    /// when a root has one, else the root `AGENTS.md` of the highest root
    /// that has it (which is all the default agent has).
    #[must_use]
    pub fn soul(&self, agent: &str) -> Option<String> {
        let own = (agent != DEFAULT_AGENT)
            .then(|| self.find_file(Path::new("agents").join(agent).join(SOUL_FILE)))
            .flatten();
        let path = own.or_else(|| self.find_file(ROOT_SOUL_FILE))?;
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => None,
            Err(error) => {
                log::warn!("cannot read {}: {error}", path.display());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_come_from_the_highest_root_and_directories_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj/sub");
        let project = tmp.path().join("proj");
        let global = tmp.path().join("config/agent");
        for (root, name, body) in [
            (&cwd, "AGENTS.md", "sub soul"),
            (&global, "AGENTS.md", "global soul"),
            (&project, "agents/review/SOUL.md", "review"),
            (
                &project,
                "agents/bare/agent.toml",
                "description = \"no soul\"",
            ),
            (&cwd, "skills/a/SKILL.md", "a from sub"),
            (&project, "skills/a/SKILL.md", "a from project"),
            (&project, "skills/b/SKILL.md", "b"),
            (&global, "skills/c/SKILL.md", "c"),
        ] {
            let base = if root == &global {
                root.clone()
            } else {
                root.join(PROJECT_AGENT_DIR)
            };
            let path = base.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        let dirs = AgentDirs::new(&cwd, Some(&project), Some(&global));
        assert_eq!(dirs.roots().len(), 3);
        assert_eq!(dirs.soul(DEFAULT_AGENT).as_deref(), Some("sub soul"));
        assert_eq!(dirs.soul("review").as_deref(), Some("review"));
        // An agent without a SOUL.md speaks with the root template.
        assert_eq!(dirs.soul("bare").as_deref(), Some("sub soul"));
        assert_eq!(
            AgentDirs::new(&project, None, None).soul(DEFAULT_AGENT),
            None
        );

        let skills = dirs.merged_entries("skills");
        let names: Vec<&String> = skills.keys().collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(skills["a"].starts_with(cwd.join(PROJECT_AGENT_DIR)));
        assert!(skills["b"].starts_with(project.join(PROJECT_AGENT_DIR)));

        // The same directory twice is one root; without a global there are two
        // roots at most.
        assert_eq!(
            AgentDirs::new(&project, Some(&project), None).roots().len(),
            1
        );
        let agents = dirs.merged_entries("agents");
        assert_eq!(agents.keys().collect::<Vec<_>>(), ["bare", "review"]);
    }
    #[test]
    fn agent_settings_come_from_agent_toml_and_default_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("ai");
        let review = global.join("agents/review");
        std::fs::create_dir_all(&review).unwrap();
        std::fs::write(
            review.join(SPEC_FILE),
            "description = \"Reviews diffs\"\nmodel = \"big\"\nmode = \"accept-edits\"\ntools = [\"read\", \"bash\"]\n",
        )
        .unwrap();
        let broken = global.join("agents/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(SPEC_FILE), "mode = 42\n").unwrap();

        let dirs = AgentDirs::new(tmp.path(), None, Some(&global));
        let spec = dirs.spec("review");
        assert_eq!(spec.description, "Reviews diffs");
        assert_eq!(spec.model.as_deref(), Some("big"));
        assert_eq!(spec.mode, Some(Mode::AcceptEdits));
        assert_eq!(
            spec.tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        assert_eq!(dirs.spec("broken"), AgentSpec::default());
        assert_eq!(dirs.spec(DEFAULT_AGENT), AgentSpec::default());
        assert_eq!(dirs.agents(), ["default", "broken", "review"]);
        let definition = dirs.agent("review");
        assert_eq!(definition.name, "review");
        assert!(definition.soul.is_none());
    }
    #[test]
    fn the_global_layout_is_created_once_and_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("ai");
        ensure_global_layout(&global).unwrap();
        for dir in ["agents", "skills", "prompts"] {
            assert!(global.join(dir).is_dir(), "{dir}");
        }
        let soul = global.join(ROOT_SOUL_FILE);
        assert_eq!(std::fs::read_to_string(&soul).unwrap(), SEED_TEMPLATE);

        std::fs::write(&soul, "mine").unwrap();
        ensure_global_layout(&global).unwrap();
        assert_eq!(std::fs::read_to_string(&soul).unwrap(), "mine");
        let dirs = AgentDirs::new(tmp.path(), None, Some(&global));
        assert_eq!(dirs.soul(DEFAULT_AGENT).as_deref(), Some("mine"));
    }
    #[test]
    fn skills_merge_termide_and_shared_directories_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj/sub");
        let project = tmp.path().join("proj");
        let global = tmp.path().join("ai");
        let write = |dir: &Path, body: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(SKILL_FILE), body).unwrap();
        };
        write(
            &cwd.join(".termide/ai/skills/deploy"),
            "---\nname: deploy\ndescription: \"Ship it\"\n---\nSteps here.\n",
        );
        write(
            &project.join(".agents/skills/deploy"),
            "---\nname: deploy\ndescription: hidden\n---\n",
        );
        write(
            &project.join(".agents/skills/review"),
            "---\ndescription: Review a diff\nallowed-tools: read\n---\nHow to review.\n",
        );
        write(&global.join("skills/notes"), "No front matter at all.\n");
        std::fs::create_dir_all(global.join("skills/not-a-skill")).unwrap();

        let dirs = AgentDirs::new(&cwd, Some(&project), Some(&global));
        let skills = dirs.skills();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "notes", "review"]);
        assert_eq!(skills[0].description, "Ship it");
        assert!(skills[0].path.starts_with(cwd.join(".termide/ai/skills")));
        assert_eq!(skills[1].description, "");
        assert_eq!(skills[2].description, "Review a diff");

        let (fields, body) = split_front_matter("---\nname: x\n---\nbody\n");
        assert_eq!(fields["name"], "x");
        assert_eq!(body, "body\n");
        assert_eq!(split_front_matter("plain").1, "plain");
    }
    #[test]
    fn prompts_come_from_markdown_files_and_expand_their_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("ai");
        let project = tmp.path().join("proj");
        let write = |dir: &Path, file: &str, body: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(file), body).unwrap();
        };
        write(
            &global.join("prompts"),
            "review.md",
            "---\ndescription: Review a file\nargument-hint: <path>\n---\nReview $1 for bugs. Notes: $ARGUMENTS\n",
        );
        write(&global.join("prompts"), "notes.txt", "not a prompt");
        write(
            &project.join(".termide/ai/prompts"),
            "review.md",
            "Project review of $1.",
        );
        write(
            &project.join(".termide/ai/prompts"),
            "tests.md",
            "Write tests.",
        );

        let dirs = AgentDirs::new(&project, None, Some(&global));
        let prompts = dirs.prompts();
        let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["review", "tests"]);
        assert_eq!(prompts[0].body, "Project review of $1.");
        assert_eq!(
            prompts[0].expand("src/x.rs extra"),
            "Project review of src/x.rs."
        );
        // No placeholder: the arguments follow the body.
        assert_eq!(
            prompts[1].expand("for parser"),
            "Write tests.\n\nfor parser"
        );
        assert_eq!(prompts[1].expand(""), "Write tests.");

        let global_only = AgentDirs::new(tmp.path(), None, Some(&global)).prompts();
        assert_eq!(global_only[0].description, "Review a file");
        assert_eq!(global_only[0].argument_hint, "<path>");
        assert_eq!(
            global_only[0].expand("a.rs b.rs"),
            "Review a.rs for bugs. Notes: a.rs b.rs"
        );
        // Unknown dollar words and $0 pass through untouched.
        let odd = PromptTemplate {
            name: "odd".into(),
            description: String::new(),
            argument_hint: String::new(),
            body: "Cost $5, $HOME, $0, $2 end".into(),
        };
        assert_eq!(odd.expand("one"), "Cost , $HOME, $0,  end");
    }
    #[test]
    fn mcp_servers_merge_by_name_and_can_be_switched_off_above() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let global = tmp.path().join("ai");
        std::fs::create_dir_all(project.join(".termide/ai")).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join(MCP_FILE),
            "[github]\ncommand = \"npx\"\n\n[fs]\ncommand = \"fs-server\"\ntools = [\"read_file\"]\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".termide/ai").join(MCP_FILE),
            "[github]\ncommand = \"npx\"\nenabled = false\n\n[db]\ncommand = \"db-server\"\n",
        )
        .unwrap();
        let servers = AgentDirs::new(&project, None, Some(&global)).mcp_servers();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["db", "fs"]);
        assert_eq!(
            servers["fs"].tools.as_deref(),
            Some(&["read_file".to_string()][..])
        );
        std::fs::write(global.join(MCP_FILE), "not = toml = at all").unwrap();
        assert_eq!(
            AgentDirs::new(&project, None, Some(&global))
                .mcp_servers()
                .keys()
                .collect::<Vec<_>>(),
            ["db"]
        );
    }
    #[test]
    fn hooks_merge_by_name_like_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let global = tmp.path().join("ai");
        std::fs::create_dir_all(project.join(".termide/ai")).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join(HOOKS_FILE),
            "[audit]\nevent = \"after_tool_call\"\ncommand = \"audit.sh\"\n\n[guard]\nevent = \"before_tool_call\"\ncommand = \"guard.sh\"\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".termide/ai").join(HOOKS_FILE),
            "[audit]\nevent = \"after_tool_call\"\ncommand = \"x\"\nenabled = false\n",
        )
        .unwrap();
        let hooks = AgentDirs::new(&project, None, Some(&global)).hooks();
        assert_eq!(hooks.keys().collect::<Vec<_>>(), ["guard"]);
    }
}
