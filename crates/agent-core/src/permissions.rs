//! Permission rules and the hook that enforces them.
//!
//! Rules live per tool as `pattern = decision` tables; among the rules that
//! match a call the strictest wins (`deny` over `ask` over `allow`). Calls no
//! rule covers fall to the mode: `ask` prompts, `accept-edits` lets file
//! tools work inside the project, `auto` allows everything. Reading inside
//! the project and a short list of read-only shell commands never prompt.
//!
//! Shell commands are split on `&&`, `||`, `;`, `|` and newlines; every part
//! must be allowed for the whole to pass, while a `deny` or `ask` on any part
//! applies to the whole. Command substitution (`$(...)`, backticks) is never
//! auto-allowed by an `allow` rule.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{Hooks, ToolDecision};
use crate::message::ToolCall;
use crate::tool::ToolContext;

/// What happens to calls no rule covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Prompt for everything except project reads and read-only commands.
    #[default]
    Ask,
    /// Also let `edit` and `write` inside the project run without a prompt.
    AcceptEdits,
    /// Allow everything; for containers and unattended runs.
    Auto,
}

/// Ordered from most to least permissive so `max` yields the strictest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// `[agent.permissions]`: a mode plus one `pattern = decision` table per tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRules {
    #[serde(default)]
    pub mode: Mode,
    #[serde(flatten, default)]
    pub tools: BTreeMap<String, BTreeMap<String, Decision>>,
}

impl PermissionRules {
    pub fn add(&mut self, tool: &str, pattern: &str, decision: Decision) {
        self.tools
            .entry(tool.to_string())
            .or_default()
            .insert(pattern.to_string(), decision);
    }

    /// Strictest decision among the rules of `tool` that match `subject`.
    #[must_use]
    pub fn evaluate(&self, tool: &str, subject: &str) -> Option<Decision> {
        self.tools
            .get(tool)?
            .iter()
            .filter(|(pattern, _)| wildcard_match(pattern, subject))
            .map(|(_, decision)| *decision)
            .max()
    }
}

/// Glob-style match: `*` spans any text (slashes included), `?` one
/// character, a leading `**/` is optional so `**/.env` also matches `.env`.
#[must_use]
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix("**/") {
        if wildcard_match(rest, text) {
            return true;
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            while p < pattern.len() && pattern[p] == '*' {
                p += 1;
            }
            star = Some((p, t));
        } else if let Some((sp, st)) = star {
            p = sp;
            t = st + 1;
            star = Some((sp, t));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// What the user is asked about.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub tool: String,
    /// The command line or the project-relative path being acted on.
    pub subject: String,
    pub call: ToolCall,
    /// Rule pattern offered for "allow for the session" and "allow always".
    pub suggested_pattern: String,
}

/// The four answers of a permission prompt, as in ACP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAnswer {
    AllowOnce,
    AllowSession,
    AllowAlways,
    Deny,
}

/// Blocks on the agent thread until the user answers.
pub trait PermissionPrompter: Send {
    fn ask(&mut self, request: &PermissionRequest) -> PermissionAnswer;
}

/// Called when the user chose "allow always", so the host can persist the
/// rule to the project configuration.
pub type PersistRule = Box<dyn FnMut(&str, &str, Decision) + Send>;

/// [`Hooks`] implementation that evaluates rules and prompts through a
/// [`PermissionPrompter`].
pub struct PermissionHooks {
    rules: PermissionRules,
    session: PermissionRules,
    prompter: Box<dyn PermissionPrompter>,
    persist: Option<PersistRule>,
}

impl PermissionHooks {
    #[must_use]
    pub fn new(rules: PermissionRules, prompter: Box<dyn PermissionPrompter>) -> Self {
        Self {
            rules,
            session: PermissionRules::default(),
            prompter,
            persist: None,
        }
    }

    #[must_use]
    pub fn with_persist(mut self, persist: PersistRule) -> Self {
        self.persist = Some(persist);
        self
    }

    #[must_use]
    pub fn rules(&self) -> &PermissionRules {
        &self.rules
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.rules.mode = mode;
    }

    /// The verdict before any prompt: rules, then session grants, then the
    /// mode and the built-in safe defaults.
    #[must_use]
    pub fn decide(&self, call: &ToolCall, ctx: &ToolContext) -> Decision {
        let subject = subject_of(call, ctx);
        let rule = |text: &str| {
            self.rules
                .evaluate(&call.name, text)
                .into_iter()
                .chain(self.session.evaluate(&call.name, text))
                .max()
        };
        let mode = self.rules.mode;

        if call.name == "bash" {
            let parsed = split_shell(&subject);
            let mut verdict = Decision::Allow;
            // Whole-command rules can tighten but never loosen a multi-part
            // command: each part must earn its own allow.
            if parsed.parts.len() > 1 {
                if let Some(whole) = rule(&subject).filter(|d| *d != Decision::Allow) {
                    verdict = verdict.max(whole);
                }
            }
            for part in &parsed.parts {
                let decision = match rule(part) {
                    Some(Decision::Allow) if parsed.has_substitution => Decision::Ask,
                    Some(decision) => decision,
                    None if mode == Mode::Auto => Decision::Allow,
                    None if !parsed.has_substitution && is_read_only_command(part) => {
                        Decision::Allow
                    }
                    None => Decision::Ask,
                };
                verdict = verdict.max(decision);
            }
            return verdict;
        }

        if let Some(decision) = rule(&subject) {
            return decision;
        }
        match call.name.as_str() {
            _ if mode == Mode::Auto => Decision::Allow,
            "read" if inside_project(call, ctx) => Decision::Allow,
            "edit" | "write" if mode == Mode::AcceptEdits && inside_project(call, ctx) => {
                Decision::Allow
            }
            _ => Decision::Ask,
        }
    }
}

impl Hooks for PermissionHooks {
    fn before_tool_call(&mut self, call: &ToolCall, ctx: &ToolContext) -> ToolDecision {
        match self.decide(call, ctx) {
            Decision::Allow => ToolDecision::Allow,
            Decision::Deny => ToolDecision::Block {
                reason: "denied by the permission rules".into(),
            },
            Decision::Ask => {
                let subject = subject_of(call, ctx);
                let request = PermissionRequest {
                    tool: call.name.clone(),
                    suggested_pattern: suggested_pattern(&call.name, &subject),
                    subject,
                    call: call.clone(),
                };
                match self.prompter.ask(&request) {
                    PermissionAnswer::AllowOnce => ToolDecision::Allow,
                    PermissionAnswer::AllowSession => {
                        self.session.add(
                            &request.tool,
                            &request.suggested_pattern,
                            Decision::Allow,
                        );
                        ToolDecision::Allow
                    }
                    PermissionAnswer::AllowAlways => {
                        self.rules
                            .add(&request.tool, &request.suggested_pattern, Decision::Allow);
                        if let Some(persist) = &mut self.persist {
                            persist(&request.tool, &request.suggested_pattern, Decision::Allow);
                        }
                        ToolDecision::Allow
                    }
                    PermissionAnswer::Deny => ToolDecision::Block {
                        reason: "denied by the user".into(),
                    },
                }
            }
        }
    }
}

/// The text rules are matched against: the command for `bash`, the
/// project-relative path for file tools, empty otherwise.
#[must_use]
pub fn subject_of(call: &ToolCall, ctx: &ToolContext) -> String {
    match call.name.as_str() {
        "bash" => call.arguments["command"].as_str().unwrap_or("").to_string(),
        "read" | "edit" | "write" => {
            let raw = call.arguments["path"].as_str().unwrap_or("");
            relative_to_project(raw, &ctx.cwd)
        }
        _ => match &call.arguments {
            Value::Object(map) => map
                .values()
                .find_map(Value::as_str)
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        },
    }
}

fn relative_to_project(raw: &str, cwd: &Path) -> String {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = normalize(&absolute);
    match normalized.strip_prefix(normalize(cwd)) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().into_owned(),
        Ok(_) => ".".to_string(),
        Err(_) => normalized.to_string_lossy().into_owned(),
    }
}

/// Resolve `.` and `..` lexically; the file need not exist.
fn normalize(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn inside_project(call: &ToolCall, ctx: &ToolContext) -> bool {
    let subject = subject_of(call, ctx);
    !Path::new(&subject).is_absolute() && !subject.starts_with("..")
}

/// Pattern offered when the user allows a call for longer than once: the
/// command's leading words for `bash`, the exact path for file tools.
#[must_use]
pub fn suggested_pattern(tool: &str, subject: &str) -> String {
    if tool != "bash" {
        return if subject.is_empty() {
            "*".to_string()
        } else {
            subject.to_string()
        };
    }
    let parts = split_shell(subject);
    let first = parts.parts.first().map_or(subject, |p| p.as_str());
    let mut words = first.split_whitespace();
    let Some(head) = words.next() else {
        return "*".to_string();
    };
    const SUBCOMMAND_TOOLS: [&str; 14] = [
        "git", "cargo", "npm", "pnpm", "yarn", "go", "docker", "kubectl", "make", "python", "pip",
        "uv", "brew", "gh",
    ];
    match words.next() {
        Some(sub) if SUBCOMMAND_TOOLS.contains(&head) && !sub.starts_with('-') => {
            format!("{head} {sub} *")
        }
        _ => format!("{head} *"),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedShell {
    parts: Vec<String>,
    has_substitution: bool,
}

/// Split a command line into its simple commands, honouring quotes.
fn split_shell(command: &str) -> ParsedShell {
    let mut parsed = ParsedShell::default();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && (c == '`' || (c == '$' && chars.get(i + 1) == Some(&'('))) {
                // Double quotes still expand substitutions.
                parsed.has_substitution = true;
            }
            current.push(c);
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
            }
            '`' => {
                parsed.has_substitution = true;
                current.push(c);
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                parsed.has_substitution = true;
                current.push(c);
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                push_part(&mut parsed.parts, &mut current);
                i += 1;
            }
            '|' => {
                push_part(&mut parsed.parts, &mut current);
                if chars.get(i + 1) == Some(&'|') {
                    i += 1;
                }
            }
            ';' | '\n' => push_part(&mut parsed.parts, &mut current),
            _ => current.push(c),
        }
        i += 1;
    }
    push_part(&mut parsed.parts, &mut current);
    if parsed.parts.is_empty() {
        parsed.parts.push(command.trim().to_string());
    }
    parsed
}

fn push_part(parts: &mut Vec<String>, current: &mut String) {
    let part = current.trim().to_string();
    if !part.is_empty() {
        parts.push(part);
    }
    current.clear();
}

/// Commands that only read state and cannot write files even with unusual
/// flags; redirections disqualify a command.
#[must_use]
pub fn is_read_only_command(part: &str) -> bool {
    if part.contains('>') || part.contains("<(") {
        return false;
    }
    let mut words = part.split_whitespace();
    let Some(head) = words.next() else {
        return false;
    };
    const PLAIN: [&str; 30] = [
        "ls", "cat", "head", "tail", "wc", "pwd", "echo", "rg", "grep", "egrep", "fgrep", "which",
        "file", "stat", "tree", "du", "sort", "uniq", "cut", "tr", "basename", "dirname",
        "realpath", "env", "printenv", "date", "whoami", "uname", "true", "false",
    ];
    if PLAIN.contains(&head) {
        return true;
    }
    match head {
        "git" => matches!(
            words.next(),
            Some(
                "status"
                    | "diff"
                    | "log"
                    | "show"
                    | "branch"
                    | "blame"
                    | "remote"
                    | "rev-parse"
                    | "ls-files"
            )
        ),
        "find" => !part.contains("-delete") && !part.contains("-exec") && !part.contains("-ok"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: PathBuf::from("/proj"),
        }
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
        }
    }

    fn bash(command: &str) -> ToolCall {
        call("bash", json!({ "command": command }))
    }

    fn rules(toml_text: &str) -> PermissionRules {
        toml::from_str(toml_text).unwrap()
    }

    /// Records requests and replays scripted answers.
    struct Scripted {
        answers: Vec<PermissionAnswer>,
        asked: Arc<Mutex<Vec<PermissionRequest>>>,
    }

    impl PermissionPrompter for Scripted {
        fn ask(&mut self, request: &PermissionRequest) -> PermissionAnswer {
            self.asked.lock().unwrap().push(request.clone());
            if self.answers.is_empty() {
                PermissionAnswer::Deny
            } else {
                self.answers.remove(0)
            }
        }
    }

    fn hooks(
        rules: PermissionRules,
        answers: Vec<PermissionAnswer>,
    ) -> (PermissionHooks, Arc<Mutex<Vec<PermissionRequest>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let hooks = PermissionHooks::new(
            rules,
            Box::new(Scripted {
                answers,
                asked: asked.clone(),
            }),
        );
        (hooks, asked)
    }

    #[test]
    fn toml_shape_round_trips() {
        let rules = rules(
            r#"
            mode = "accept-edits"
            [bash]
            "git status*" = "allow"
            "git push*" = "ask"
            "rm -rf *" = "deny"
            [edit]
            "src/**" = "allow"
            ".env" = "deny"
            "#,
        );
        assert_eq!(rules.mode, Mode::AcceptEdits);
        assert_eq!(
            rules.evaluate("bash", "git push origin main"),
            Some(Decision::Ask)
        );
        assert_eq!(rules.evaluate("edit", "src/lib.rs"), Some(Decision::Allow));
        assert_eq!(rules.evaluate("edit", "README.md"), None);
        let text = toml::to_string(&rules).unwrap();
        assert_eq!(toml::from_str::<PermissionRules>(&text).unwrap(), rules);
        assert_eq!(PermissionRules::default().mode, Mode::Ask);
    }

    #[test]
    fn strictest_matching_rule_wins() {
        let mut rules = PermissionRules::default();
        rules.add("bash", "git *", Decision::Allow);
        rules.add("bash", "git push*", Decision::Ask);
        rules.add("bash", "*--force*", Decision::Deny);
        assert_eq!(rules.evaluate("bash", "git status"), Some(Decision::Allow));
        assert_eq!(
            rules.evaluate("bash", "git push origin"),
            Some(Decision::Ask)
        );
        assert_eq!(
            rules.evaluate("bash", "git push --force"),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn wildcard_semantics() {
        assert!(wildcard_match("src/**", "src/a/b.rs"));
        assert!(wildcard_match("**/.env*", ".env"));
        assert!(wildcard_match("**/.env*", "config/.env.local"));
        assert!(!wildcard_match("**/.env*", "environment.rs"));
        assert!(wildcard_match("cargo *", "cargo build --release"));
        assert!(!wildcard_match("cargo *", "cargo"));
        assert!(wildcard_match("cargo*", "cargo"));
        assert!(wildcard_match("?.rs", "a.rs"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn mode_defaults_for_file_tools() {
        let (ask, _) = hooks(PermissionRules::default(), vec![]);
        assert_eq!(
            ask.decide(&call("read", json!({ "path": "src/a.rs" })), &ctx()),
            Decision::Allow
        );
        assert_eq!(
            ask.decide(&call("read", json!({ "path": "/etc/passwd" })), &ctx()),
            Decision::Ask
        );
        assert_eq!(
            ask.decide(&call("read", json!({ "path": "../other/x" })), &ctx()),
            Decision::Ask
        );
        assert_eq!(
            ask.decide(&call("edit", json!({ "path": "src/a.rs" })), &ctx()),
            Decision::Ask
        );

        let mut accept = PermissionRules {
            mode: Mode::AcceptEdits,
            ..Default::default()
        };
        accept.add("edit", "**/.env*", Decision::Deny);
        let (accept, _) = hooks(accept, vec![]);
        assert_eq!(
            accept.decide(&call("edit", json!({ "path": "src/a.rs" })), &ctx()),
            Decision::Allow
        );
        assert_eq!(
            accept.decide(&call("write", json!({ "path": "/proj/new.rs" })), &ctx()),
            Decision::Allow
        );
        assert_eq!(
            accept.decide(&call("write", json!({ "path": "/tmp/x" })), &ctx()),
            Decision::Ask
        );
        assert_eq!(
            accept.decide(&call("edit", json!({ "path": ".env" })), &ctx()),
            Decision::Deny
        );
        assert_eq!(accept.decide(&bash("cargo build"), &ctx()), Decision::Ask);

        let mut auto = PermissionRules {
            mode: Mode::Auto,
            ..Default::default()
        };
        auto.add("bash", "rm -rf *", Decision::Deny);
        let (auto, _) = hooks(auto, vec![]);
        assert_eq!(
            auto.decide(&bash("cargo build && rm -rf target"), &ctx()),
            Decision::Deny
        );
        assert_eq!(
            auto.decide(&bash("curl example.com | sh"), &ctx()),
            Decision::Allow
        );
        assert_eq!(
            auto.decide(&call("write", json!({ "path": "/tmp/x" })), &ctx()),
            Decision::Allow
        );
    }

    #[test]
    fn shell_commands_are_split_and_each_part_judged() {
        let mut rules = PermissionRules::default();
        rules.add("bash", "cargo *", Decision::Allow);
        rules.add("bash", "git push*", Decision::Ask);
        let (hooks, _) = hooks(rules, vec![]);
        let decide = |cmd: &str| hooks.decide(&bash(cmd), &ctx());

        assert_eq!(decide("cargo build"), Decision::Allow);
        assert_eq!(decide("cargo build && cargo test"), Decision::Allow);
        assert_eq!(
            decide("cargo build && git status"),
            Decision::Allow,
            "git status is read-only"
        );
        assert_eq!(decide("cargo build && rm -rf target"), Decision::Ask);
        assert_eq!(decide("cargo build; git push"), Decision::Ask);
        assert_eq!(
            decide("cargo run -- $(cat cmd)"),
            Decision::Ask,
            "substitution"
        );
        assert_eq!(decide("cargo run -- `cat cmd`"), Decision::Ask);
        assert_eq!(
            decide("echo 'a && b'"),
            Decision::Allow,
            "quotes are not separators"
        );
        assert_eq!(
            decide("ls > out.txt"),
            Decision::Ask,
            "redirection is a write"
        );
        assert_eq!(decide("find . -name '*.rs'"), Decision::Allow);
        assert_eq!(decide("find . -delete"), Decision::Ask);
        assert_eq!(decide("git log | head"), Decision::Allow);
    }

    #[test]
    fn split_shell_details() {
        let parsed = split_shell("a && b || c | d; e\nf");
        assert_eq!(parsed.parts, vec!["a", "b", "c", "d", "e", "f"]);
        assert!(!parsed.has_substitution);
        let quoted = split_shell("echo \"x; $(id)\" && ls");
        assert_eq!(quoted.parts, vec!["echo \"x; $(id)\"", "ls"]);
        assert!(quoted.has_substitution);
        assert_eq!(split_shell("   ").parts, vec![""]);
    }

    #[test]
    fn prompt_answers_drive_grants_and_persistence() {
        let persisted = Arc::new(Mutex::new(Vec::new()));
        let sink = persisted.clone();
        let (hooks, asked) = hooks(
            PermissionRules::default(),
            vec![
                PermissionAnswer::AllowOnce,
                PermissionAnswer::AllowSession,
                PermissionAnswer::AllowAlways,
                PermissionAnswer::Deny,
            ],
        );
        let mut hooks = hooks.with_persist(Box::new(move |tool, pattern, decision| {
            sink.lock()
                .unwrap()
                .push((tool.to_string(), pattern.to_string(), decision));
        }));

        // 1. allow once: asked again next time
        assert_eq!(
            hooks.before_tool_call(&bash("cargo build"), &ctx()),
            ToolDecision::Allow
        );
        // 2. allow for the session: "cargo build *" is granted in memory
        assert_eq!(
            hooks.before_tool_call(&bash("cargo build"), &ctx()),
            ToolDecision::Allow
        );
        assert_eq!(
            hooks.decide(&bash("cargo build --release"), &ctx()),
            Decision::Allow
        );
        assert!(persisted.lock().unwrap().is_empty());
        // 3. allow always: persisted through the callback
        assert_eq!(
            hooks.before_tool_call(&bash("npm test"), &ctx()),
            ToolDecision::Allow
        );
        assert_eq!(
            *persisted.lock().unwrap(),
            vec![(
                "bash".to_string(),
                "npm test *".to_string(),
                Decision::Allow
            )]
        );
        assert_eq!(
            hooks.rules().evaluate("bash", "npm test -- x"),
            Some(Decision::Allow)
        );
        // 4. deny
        let blocked = hooks.before_tool_call(&call("write", json!({ "path": "a" })), &ctx());
        assert!(matches!(blocked, ToolDecision::Block { reason } if reason.contains("user")));

        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 4);
        assert_eq!(asked[0].suggested_pattern, "cargo build *");
        assert_eq!(asked[3].subject, "a");
        assert_eq!(asked[3].suggested_pattern, "a");
    }

    #[test]
    fn rule_denials_block_without_prompting() {
        let mut rules = PermissionRules::default();
        rules.add("read", "**/.env*", Decision::Deny);
        let (mut hooks, asked) = hooks(rules, vec![]);
        let blocked = hooks.before_tool_call(
            &call("read", json!({ "path": "/proj/config/.env" })),
            &ctx(),
        );
        assert!(matches!(blocked, ToolDecision::Block { reason } if reason.contains("rules")));
        assert!(asked.lock().unwrap().is_empty());
    }

    #[test]
    fn suggested_patterns() {
        assert_eq!(
            suggested_pattern("bash", "git push origin main"),
            "git push *"
        );
        assert_eq!(suggested_pattern("bash", "ls -la"), "ls *");
        assert_eq!(suggested_pattern("bash", "git -C x status"), "git *");
        assert_eq!(
            suggested_pattern("bash", "cargo test && cargo fmt"),
            "cargo test *"
        );
        assert_eq!(suggested_pattern("edit", "src/lib.rs"), "src/lib.rs");
        assert_eq!(suggested_pattern("other", ""), "*");
    }

    #[test]
    fn subjects_are_project_relative() {
        assert_eq!(
            subject_of(
                &call("edit", json!({ "path": "/proj/src/../a.rs" })),
                &ctx()
            ),
            "a.rs"
        );
        assert_eq!(
            subject_of(&call("edit", json!({ "path": "./b.rs" })), &ctx()),
            "b.rs"
        );
        assert_eq!(
            subject_of(&call("edit", json!({ "path": "/etc/x" })), &ctx()),
            "/etc/x"
        );
        assert_eq!(
            subject_of(&call("read", json!({ "path": "/proj" })), &ctx()),
            "."
        );
        assert_eq!(
            subject_of(&call("mcp_tool", json!({ "query": "q" })), &ctx()),
            "q"
        );
    }
}
