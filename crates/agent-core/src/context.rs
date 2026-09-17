//! System prompt composition and project instruction discovery.
//!
//! The prompt is short on purpose: identity, the tool list with one-line
//! snippets, merged guidelines, an environment block and the project's
//! instruction files. Instruction files follow the cross-agent `AGENTS.md`
//! convention with `CLAUDE.md` as a fallback in the same directory, walked
//! from the filesystem root down to the working directory so the most
//! specific file comes last.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tool::ToolRegistry;

/// Instruction files larger than this are skipped with a warning.
pub const MAX_CONTEXT_FILE_BYTES: u64 = 32 * 1024;

/// Names tried in each directory, in order; the first that exists wins.
const CONTEXT_FILE_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

const DEFAULT_IDENTITY: &str = "You are a coding agent working inside termide, a terminal IDE. \
You help with software tasks in the current project: you read code, make targeted edits, run \
commands and report what you did and what you found.";

const BASE_GUIDELINES: [&str; 3] = [
    "Read a file before you change it, and keep edits small and targeted.",
    "Name file paths clearly when you talk about files.",
    "Be concise.",
];

/// One project instruction file, in prompt order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// Find instruction files: the optional global file first, then one per
/// ancestor of `cwd` from the root down to `cwd` itself.
#[must_use]
pub fn discover_context_files(cwd: &Path, global: Option<&Path>) -> Vec<ContextFile> {
    let mut files = Vec::new();
    if let Some(global) = global {
        if let Some(file) = load_context_file(global) {
            files.push(file);
        }
    }
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for dir in ancestors {
        let Some(path) = CONTEXT_FILE_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
        else {
            continue;
        };
        if global.is_some_and(|g| same_file(g, &path)) {
            continue;
        }
        if let Some(file) = load_context_file(&path) {
            files.push(file);
        }
    }
    files
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn load_context_file(path: &Path) -> Option<ContextFile> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    if metadata.len() > MAX_CONTEXT_FILE_BYTES {
        log::warn!(
            "skipping instruction file {} ({} bytes, limit {})",
            path.display(),
            metadata.len(),
            MAX_CONTEXT_FILE_BYTES
        );
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    Some(ContextFile {
        path: path.to_path_buf(),
        content,
    })
}

/// Inputs of [`build_system_prompt`].
#[derive(Debug, Clone)]
pub struct PromptOptions<'a> {
    pub cwd: &'a Path,
    pub tools: &'a ToolRegistry,
    pub context_files: &'a [ContextFile],
    /// Replaces the default identity paragraph.
    pub identity: Option<&'a str>,
    /// Appended verbatim at the end.
    pub append: Option<&'a str>,
    /// Unix time in milliseconds for the date line; `None` uses the clock.
    pub now_millis: Option<u64>,
}

impl<'a> PromptOptions<'a> {
    #[must_use]
    pub fn new(cwd: &'a Path, tools: &'a ToolRegistry, context_files: &'a [ContextFile]) -> Self {
        Self {
            cwd,
            tools,
            context_files,
            identity: None,
            append: None,
            now_millis: None,
        }
    }
}

#[must_use]
pub fn build_system_prompt(options: &PromptOptions<'_>) -> String {
    let mut out = String::new();
    out.push_str(options.identity.unwrap_or(DEFAULT_IDENTITY));
    out.push_str("\n\n# Tools\n");
    let snippets: Vec<String> = options
        .tools
        .iter()
        .filter_map(|tool| {
            tool.prompt_snippet()
                .map(|snippet| format!("- {}: {snippet}", tool.name()))
        })
        .collect();
    if snippets.is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(&snippets.join("\n"));
        out.push('\n');
    }

    out.push_str("\n# Guidelines\n");
    let mut guidelines: Vec<&str> = Vec::new();
    for tool in options.tools.iter() {
        for guideline in tool.prompt_guidelines() {
            if !guidelines.contains(guideline) {
                guidelines.push(guideline);
            }
        }
    }
    for guideline in BASE_GUIDELINES {
        if !guidelines.contains(&guideline) {
            guidelines.push(guideline);
        }
    }
    for guideline in guidelines {
        out.push_str("- ");
        out.push_str(guideline);
        out.push('\n');
    }

    out.push_str("\n# Environment\n");
    out.push_str(&format!("- Working directory: {}\n", options.cwd.display()));
    out.push_str(&format!(
        "- Platform: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    let now = options.now_millis.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });
    out.push_str(&format!("- Date: {}\n", civil_date(now)));
    out.push_str(&format!(
        "- Git repository: {}\n",
        if in_git_repository(options.cwd) {
            "yes"
        } else {
            "no"
        }
    ));

    if !options.context_files.is_empty() {
        out.push_str("\n# Project instructions\n");
        for file in options.context_files {
            out.push_str(&format!("\n## {}\n\n", file.path.display()));
            out.push_str(file.content.trim_end());
            out.push('\n');
        }
    }

    if let Some(append) = options.append.filter(|a| !a.trim().is_empty()) {
        out.push('\n');
        out.push_str(append.trim_end());
        out.push('\n');
    }
    out
}

/// Whether `path` or one of its ancestors holds a `.git` entry.
#[must_use]
pub fn in_git_repository(path: &Path) -> bool {
    path.ancestors().any(|dir| dir.join(".git").exists())
}

/// `YYYY-MM-DD` for a Unix timestamp in milliseconds (UTC).
#[must_use]
pub fn civil_date(millis: u64) -> String {
    let (year, month, day) = civil_from_days((millis / 86_400_000) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `YYYY-MM-DDTHH-MM-SS` (UTC), safe for file names.
#[must_use]
pub fn file_timestamp(millis: u64) -> String {
    let seconds = millis / 1000;
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let rem = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
// algorithm), so no date crate is needed for two formatted strings.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{registry_with, EchoTool};
    use std::sync::Arc;

    #[test]
    fn dates_are_formatted_in_utc() {
        assert_eq!(civil_date(0), "1970-01-01");
        // 2026-09-17T18:41:36Z
        assert_eq!(civil_date(1_789_670_496_000), "2026-09-17");
        assert_eq!(file_timestamp(1_789_670_496_000), "2026-09-17T18-41-36");
        assert_eq!(civil_date(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn context_files_walk_from_root_to_cwd_with_claude_fallback() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let nested = project.join("crates").join("x");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("AGENTS.md"), "project rules").unwrap();
        std::fs::write(project.join("CLAUDE.md"), "ignored, AGENTS.md wins").unwrap();
        std::fs::write(nested.join("CLAUDE.md"), "nested rules").unwrap();
        std::fs::write(project.join("crates").join("AGENTS.md"), "   \n").unwrap();
        let global = root.path().join("global.md");
        std::fs::write(&global, "global rules").unwrap();

        let files = discover_context_files(&nested, Some(&global));
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["global rules", "project rules", "nested rules"]
        );
        assert!(files[1].path.ends_with("AGENTS.md"));
    }

    #[test]
    fn oversized_files_are_skipped_and_global_is_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("AGENTS.md");
        std::fs::write(&big, "x".repeat(MAX_CONTEXT_FILE_BYTES as usize + 1)).unwrap();
        assert!(discover_context_files(dir.path(), None).is_empty());

        std::fs::write(&big, "shared").unwrap();
        let files = discover_context_files(dir.path(), Some(&big));
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn prompt_has_tools_guidelines_environment_and_instructions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let tools = registry_with(Arc::new(EchoTool::default()));
        let files = vec![ContextFile {
            path: dir.path().join("AGENTS.md"),
            content: "Use conventional commits.\n".into(),
        }];
        let mut options = PromptOptions::new(dir.path(), &tools, &files);
        options.append = Some("Answer in Russian.");
        options.now_millis = Some(1_789_670_496_000);
        let prompt = build_system_prompt(&options);

        assert!(prompt.starts_with(DEFAULT_IDENTITY));
        assert!(prompt.contains("# Tools\n(none)"), "echo has no snippet");
        assert!(prompt.contains("- Be concise."));
        assert!(prompt.contains(&format!("- Working directory: {}", dir.path().display())));
        assert!(prompt.contains("- Date: 2026-09-17"));
        assert!(prompt.contains("- Git repository: yes"));
        assert!(prompt.contains("# Project instructions"));
        assert!(prompt.contains("Use conventional commits."));
        assert!(prompt.ends_with("Answer in Russian.\n"));

        options.identity = Some("You are terse.");
        let custom = build_system_prompt(&options);
        assert!(custom.starts_with("You are terse.\n\n# Tools"));
    }
}
