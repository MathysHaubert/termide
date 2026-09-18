//! Command scripts: executables in `ai/commands/` that the user runs as
//! `/<name> args` and whose standard output becomes the request sent to the
//! model — a `/review` that gathers `git diff`, a `/fix-tests` that pastes
//! the failures, an `/issue 123` that fetches a ticket.
//!
//! Static aliases stay in `prompts/`; a directory of its own keeps "text I
//! send" apart from "code I run", and lets the panel ask before running a
//! script that came with a project rather than from the user's own
//! configuration.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Command scripts under an `ai` directory.
pub const COMMANDS_DIR: &str = "commands";

/// One executable command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandScript {
    /// The file name, typed as `/<name>`.
    pub name: String,
    /// `# description:` in the file's header, for the picker.
    pub description: String,
    /// `# argument-hint:` in the header.
    pub argument_hint: String,
    pub path: PathBuf,
    /// From the configuration level, the user's own files: runs unasked.
    /// A script that came with a project or a directory asks first.
    pub trusted: bool,
    /// `# timeout:` in the header, seconds; 60 by default.
    pub timeout_secs: u64,
}

impl CommandScript {
    /// Read the header of an executable at `path`; `None` when the file is
    /// not executable (reported) or not a file.
    pub fn from_file(path: &Path, trusted: bool) -> Option<Self> {
        if !path.is_file() {
            return None;
        }
        if !is_executable(path) {
            log::warn!(
                "{} is not executable and is skipped; chmod +x makes it a command",
                path.display()
            );
            return None;
        }
        let name = path.file_name()?.to_string_lossy().into_owned();
        let head = std::fs::read_to_string(path).unwrap_or_default();
        let mut script = Self {
            name,
            description: String::new(),
            argument_hint: String::new(),
            path: path.to_path_buf(),
            trusted,
            timeout_secs: 60,
        };
        for line in head.lines().take(12) {
            let Some(comment) = line.trim().strip_prefix('#') else {
                continue;
            };
            let Some((key, value)) = comment.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "description" => script.description = value.to_string(),
                "argument-hint" => script.argument_hint = value.to_string(),
                "timeout" => {
                    if let Ok(secs) = value.parse() {
                        script.timeout_secs = secs;
                    }
                }
                _ => {}
            }
        }
        Some(script)
    }

    /// Run the script in `cwd` with `args` split on whitespace; its standard
    /// output, trimmed, is the request. A non-zero exit, a timeout or a
    /// failure to start is an error carrying standard error.
    pub fn run(&self, args: &str, cwd: &Path) -> Result<String, String> {
        let mut child = Command::new(&self.path)
            .args(args.split_whitespace())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", self.path.display()))?;
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs.max(1));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "/{} gave no answer within {} s",
                        self.name, self.timeout_secs
                    ));
                }
                Err(error) => return Err(format!("/{}: {error}", self.name)),
            }
        };
        let collect = |handle: Option<std::thread::JoinHandle<String>>| {
            handle.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        let out = collect(stdout);
        let err = collect(stderr);
        if !status.success() {
            let detail = err.trim();
            return Err(if detail.is_empty() {
                format!("/{} exited with {}", self.name, status)
            } else {
                format!("/{}: {detail}", self.name)
            });
        }
        let text = out.trim().to_string();
        if text.is_empty() {
            return Err(format!("/{} printed nothing to send", self.name));
        }
        Ok(text)
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn header_output_failure_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let review = script(
            dir.path(),
            "review",
            "#!/bin/sh\n# description: Review the staged diff\n# argument-hint: [path]\n# timeout: 5\necho \"Review this: $1 $2\"\n",
        );
        let cmd = CommandScript::from_file(&review, false).unwrap();
        assert_eq!(cmd.name, "review");
        assert_eq!(cmd.description, "Review the staged diff");
        assert_eq!(cmd.argument_hint, "[path]");
        assert_eq!(cmd.timeout_secs, 5);
        assert!(!cmd.trusted);
        assert_eq!(
            cmd.run("src/x.rs now", dir.path()).unwrap(),
            "Review this: src/x.rs now"
        );

        let failing = script(
            dir.path(),
            "fail",
            "#!/bin/sh\necho 'no diff' >&2\nexit 3\n",
        );
        let cmd = CommandScript::from_file(&failing, true).unwrap();
        assert!(cmd.trusted);
        assert_eq!(cmd.run("", dir.path()).unwrap_err(), "/fail: no diff");

        let silent = script(dir.path(), "silent", "#!/bin/sh\nexit 0\n");
        let cmd = CommandScript::from_file(&silent, true).unwrap();
        assert!(cmd
            .run("", dir.path())
            .unwrap_err()
            .contains("printed nothing"));

        let slow = script(dir.path(), "slow", "#!/bin/sh\n# timeout: 1\nsleep 5\n");
        let cmd = CommandScript::from_file(&slow, true).unwrap();
        assert!(cmd
            .run("", dir.path())
            .unwrap_err()
            .contains("no answer within 1 s"));

        let plain = dir.path().join("notes");
        std::fs::write(&plain, "just text").unwrap();
        assert!(
            CommandScript::from_file(&plain, true).is_none(),
            "not executable"
        );
    }
}
