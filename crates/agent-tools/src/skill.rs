//! `skill`: load a skill's instructions by name.
//!
//! The system prompt lists skills by name and description only; the body of
//! `SKILL.md` enters the context when the model asks for it, so a skill costs
//! one line per request until it is needed. A name is cheaper and safer for
//! the model than a path — the list is an enum in the schema — and the tool
//! returns the body verbatim, without `read`'s line numbers, together with
//! the files that come with the skill, which the model reads by path.

use std::path::Path;

use serde_json::{json, Value};
use termide_agent_core::{
    split_front_matter, CancelToken, SkillInfo, Tool, ToolCall, ToolContext, ToolResultMessage,
    ToolUpdate,
};

use crate::args::required_str;

const DESCRIPTION: &str = "Load a skill: step-by-step instructions for a kind of task. The \
system prompt lists the available skills with a one-line description each; call this with a \
skill's name before starting on a task it covers. Returns the skill's text and the files that \
come with it.";

pub struct SkillTool {
    skills: Vec<SkillInfo>,
}

impl SkillTool {
    #[must_use]
    pub fn new(skills: Vec<SkillInfo>) -> Self {
        Self { skills }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "enum": names, "description": "Skill name as listed in the system prompt" }
            },
            "required": ["name"]
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("load a skill's instructions by name")
    }

    fn execute(
        &self,
        call: &ToolCall,
        _ctx: &ToolContext,
        _on_update: &mut dyn FnMut(ToolUpdate),
        _cancel: &CancelToken,
    ) -> ToolResultMessage {
        match self.load(call) {
            Ok((text, details)) => ToolResultMessage::text(call, text).with_details(details),
            Err(message) => ToolResultMessage::error(call, message),
        }
    }
}

impl SkillTool {
    fn load(&self, call: &ToolCall) -> Result<(String, Value), String> {
        let name = required_str(call, "name")?;
        let Some(skill) = self.skills.iter().find(|s| s.name == name) else {
            let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
            return Err(format!(
                "no skill named {name}; available: {}",
                names.join(", ")
            ));
        };
        let raw = std::fs::read_to_string(&skill.path)
            .map_err(|error| format!("cannot read {}: {error}", skill.path.display()))?;
        let (_, body) = split_front_matter(&raw);
        let dir = skill.path.parent().unwrap_or(Path::new("."));
        let files = companion_files(dir);
        let mut text = body.trim_end().to_string();
        if !files.is_empty() {
            text.push_str(&format!(
                "\n\nFiles of this skill, under {}:\n",
                dir.display()
            ));
            for file in &files {
                text.push_str(&format!("- {file}\n"));
            }
        }
        Ok((
            text,
            json!({ "name": skill.name, "path": skill.path, "files": files }),
        ))
    }
}

/// Every file under `dir` except `SKILL.md`, as paths relative to `dir`,
/// sorted, so the model can `read` the ones it needs.
fn companion_files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if let Ok(relative) = path.strip_prefix(base) {
                let relative = relative.to_string_lossy().replace('\\', "/");
                if relative != termide_agent_core::SKILL_FILE {
                    out.push(relative);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run(tool: &SkillTool, args: Value) -> ToolResultMessage {
        let call = ToolCall {
            id: "c".into(),
            name: "skill".into(),
            arguments: args,
        };
        let ctx = ToolContext {
            cwd: PathBuf::from("/"),
        };
        tool.execute(&call, &ctx, &mut |_| {}, &CancelToken::new())
    }

    #[test]
    fn a_skill_loads_without_front_matter_and_lists_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("deploy");
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\n# Deploy\n\nRun the script.\n",
        )
        .unwrap();
        std::fs::write(skill.join("scripts/release.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(skill.join("checklist.md"), "- tag\n").unwrap();
        let tool = SkillTool::new(vec![SkillInfo {
            name: "deploy".into(),
            description: "Ship it".into(),
            path: skill.join("SKILL.md"),
        }]);

        assert_eq!(
            tool.parameters()["properties"]["name"]["enum"],
            json!(["deploy"])
        );
        let result = run(&tool, json!({ "name": "deploy" }));
        assert!(!result.is_error, "{}", result.plain_text());
        let text = result.plain_text();
        assert!(text.starts_with("# Deploy\n\nRun the script."), "{text}");
        assert!(!text.contains("description:"));
        assert!(
            text.contains("- checklist.md\n- scripts/release.sh\n"),
            "{text}"
        );
        assert_eq!(
            result.details.unwrap()["files"],
            json!(["checklist.md", "scripts/release.sh"])
        );

        let missing = run(&tool, json!({ "name": "nope" }));
        assert!(missing.is_error);
        assert!(missing.plain_text().contains("available: deploy"));
    }
}
