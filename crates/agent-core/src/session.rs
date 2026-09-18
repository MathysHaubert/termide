//! Append-only JSONL session log.
//!
//! One file per session. The first line is the header, every later line an
//! entry with an `id` and a `parent_id`, so the file is a tree even though
//! the UI only ever walks one path (leaf to root) today. Moving the leaf to
//! an earlier entry and appending starts a new branch without rewriting
//! anything.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::compaction::summary_message;
use crate::context::file_timestamp;
use crate::message::{now_millis, Message};

/// Bumped when a line shape changes incompatibly.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// First line of a session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub created: u64,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Payload of a tree entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    Message {
        message: Message,
    },
    /// The model the branch runs on from here. The context window is the
    /// figure the panel knew at the time (configured or reported by the
    /// endpoint), so a resume needs nothing but the log.
    ModelChange {
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
    },
    /// The `keep_last` messages before this entry stay verbatim; everything
    /// earlier on the branch is replaced by `summary`.
    Compaction {
        summary: String,
        tokens_before: u64,
        keep_last: usize,
    },
    /// A name the user gave this conversation. The last one on the branch
    /// wins, so renaming is just another entry.
    SessionName {
        name: String,
    },
}

/// The model a session last recorded, see [`Session::current_model`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModel {
    pub provider: String,
    pub id: String,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    #[serde(flatten)]
    pub kind: EntryKind,
}

/// One line of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Line {
    Header(HeaderLine),
    Entry(Entry),
}

/// The header carries `"type": "session"` on disk so a reader can tell the
/// lines apart without positional rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeaderLine {
    #[serde(rename = "type")]
    kind: HeaderTag,
    #[serde(flatten)]
    header: SessionHeader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HeaderTag {
    Session,
}

#[derive(Debug)]
pub struct Session {
    path: PathBuf,
    header: SessionHeader,
    entries: Vec<Entry>,
    leaf: Option<String>,
    file: File,
}

impl Session {
    /// Create a new session file in `dir` (created if missing).
    pub fn create(dir: &Path, cwd: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let created = now_millis();
        let id = new_id();
        let header = SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: id.clone(),
            created,
            cwd: cwd.to_path_buf(),
            name: None,
        };
        let path = dir.join(format!("{}_{id}.jsonl", file_timestamp(created)));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        write_line(
            &mut file,
            &Line::Header(HeaderLine {
                kind: HeaderTag::Session,
                header: header.clone(),
            }),
        )?;
        Ok(Self {
            path,
            header,
            entries: Vec::new(),
            leaf: None,
            file,
        })
    }

    /// Load an existing session; the leaf is the last entry. Malformed lines
    /// are skipped with a warning so one bad write does not lose a session.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let reader = BufReader::new(File::open(path)?);
        let mut header = None;
        let mut entries = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Line>(&line) {
                Ok(Line::Header(h)) if header.is_none() => header = Some(h.header),
                Ok(Line::Header(_)) => {
                    log::warn!("{}: duplicate header at line {}", path.display(), index + 1)
                }
                Ok(Line::Entry(entry)) => entries.push(entry),
                Err(error) => log::warn!(
                    "{}: skipping malformed line {}: {error}",
                    path.display(),
                    index + 1
                ),
            }
        }
        let header = header.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} has no session header", path.display()),
            )
        })?;
        let leaf = entries.last().map(|e| e.id.clone());
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            header,
            entries,
            leaf,
            file,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.header.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[must_use]
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// Move the leaf. `None` restarts from the root; an unknown id is an
    /// error and leaves the leaf unchanged.
    pub fn set_leaf(&mut self, id: Option<&str>) -> Result<(), String> {
        if let Some(id) = id {
            if !self.entries.iter().any(|e| e.id == id) {
                return Err(format!("no entry with id {id}"));
            }
        }
        self.leaf = id.map(str::to_string);
        Ok(())
    }

    pub fn append_message(&mut self, message: &Message) -> std::io::Result<String> {
        self.append(EntryKind::Message {
            message: message.clone(),
        })
    }

    pub fn append_model_change(
        &mut self,
        provider: &str,
        model: &str,
        context_window: Option<u64>,
    ) -> std::io::Result<String> {
        self.append(EntryKind::ModelChange {
            provider: provider.to_string(),
            model: model.to_string(),
            context_window,
        })
    }

    fn append(&mut self, kind: EntryKind) -> std::io::Result<String> {
        let entry = Entry {
            id: new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_millis(),
            kind,
        };
        write_line(&mut self.file, &Line::Entry(entry.clone()))?;
        self.leaf = Some(entry.id.clone());
        let id = entry.id.clone();
        self.entries.push(entry);
        Ok(id)
    }

    /// Entries on the path from the root to the leaf, oldest first.
    #[must_use]
    pub fn branch(&self) -> Vec<&Entry> {
        let by_id: HashMap<&str, &Entry> =
            self.entries.iter().map(|e| (e.id.as_str(), e)).collect();
        let mut path = Vec::new();
        let mut current = self.leaf.as_deref();
        while let Some(id) = current {
            let Some(entry) = by_id.get(id) else {
                log::warn!("{}: dangling parent id {id}", self.path.display());
                break;
            };
            path.push(*entry);
            current = entry.parent_id.as_deref();
        }
        path.reverse();
        path
    }

    /// The transcript the model should see for the current branch.
    #[must_use]
    pub fn context_messages(&self) -> Vec<Message> {
        let mut messages: Vec<Message> = Vec::new();
        for entry in self.branch() {
            match &entry.kind {
                EntryKind::Message { message } => messages.push(message.clone()),
                EntryKind::ModelChange { .. } | EntryKind::SessionName { .. } => {}
                EntryKind::Compaction {
                    summary, keep_last, ..
                } => {
                    let start = messages.len().saturating_sub(*keep_last);
                    let tail = messages.split_off(start);
                    messages = vec![summary_message(summary)];
                    messages.extend(tail);
                }
            }
        }
        messages
    }

    /// The conversation's name: the last one set on the current branch, or
    /// the one the file was created with.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::SessionName { name } => Some(name.as_str()),
                _ => None,
            })
            .or(self.header.name.as_deref())
    }

    /// Name (or rename) the conversation. An empty name clears it.
    pub fn set_name(&mut self, name: &str) -> std::io::Result<String> {
        let name = name.trim().to_string();
        self.header.name = (!name.is_empty()).then(|| name.clone());
        self.append(EntryKind::SessionName { name })
    }

    pub fn append_compaction(
        &mut self,
        summary: &str,
        tokens_before: u64,
        keep_last: usize,
    ) -> std::io::Result<String> {
        self.append(EntryKind::Compaction {
            summary: summary.to_string(),
            tokens_before,
            keep_last,
        })
    }

    /// Model recorded last on the current branch.
    #[must_use]
    pub fn current_model(&self) -> Option<SessionModel> {
        self.branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::ModelChange {
                    provider,
                    model,
                    context_window,
                } => Some(SessionModel {
                    provider: provider.clone(),
                    id: model.clone(),
                    context_window: *context_window,
                }),
                EntryKind::Message { .. }
                | EntryKind::Compaction { .. }
                | EntryKind::SessionName { .. } => None,
            })
    }

    /// Summaries of every session in `dir`, newest first.
    pub fn list(dir: &Path) -> std::io::Result<Vec<SessionSummary>> {
        let mut summaries = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Ok(summaries);
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            match Session::open(&path) {
                Ok(session) => summaries.push(SessionSummary::from(&session)),
                Err(error) => log::warn!("skipping {}: {error}", path.display()),
            }
        }
        summaries.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.created.cmp(&a.created)));
        Ok(summaries)
    }
}

impl SessionSummary {
    /// One line for a session picker: the name if it has one, else the first
    /// prompt, else the session id.
    #[must_use]
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| {
                self.first_prompt
                    .as_ref()
                    .map(|prompt| prompt.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|prompt| !prompt.is_empty())
            })
            .unwrap_or_else(|| self.id.clone())
    }
}

/// What a session picker shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub name: Option<String>,
    pub created: u64,
    /// Timestamp of the last entry, or `created` for an empty session.
    pub modified: u64,
    /// The first user prompt, as a title fallback.
    pub first_prompt: Option<String>,
    pub message_count: usize,
}

impl From<&Session> for SessionSummary {
    fn from(session: &Session) -> Self {
        let messages = session.entries.iter().filter_map(|e| match &e.kind {
            EntryKind::Message { message } => Some(message),
            EntryKind::ModelChange { .. }
            | EntryKind::Compaction { .. }
            | EntryKind::SessionName { .. } => None,
        });
        let first_prompt = messages.clone().find_map(|m| match m {
            Message::User(user) => Some(user.plain_text()),
            _ => None,
        });
        Self {
            path: session.path.clone(),
            id: session.header.id.clone(),
            name: session.name().map(str::to_string),
            created: session.header.created,
            modified: session
                .entries
                .last()
                .map_or(session.header.created, |e| e.timestamp),
            first_prompt,
            message_count: messages.count(),
        }
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Eight hex characters unique enough for one machine: time, a process-wide
/// counter and the pid go through the standard hasher.
#[must_use]
pub fn new_id() -> String {
    let mut hasher = DefaultHasher::new();
    now_millis().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

fn write_line(file: &mut File, line: &Line) -> std::io::Result<()> {
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    file.write_all(text.as_bytes())?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::text_reply;
    use crate::message::UserMessage;

    #[test]
    fn create_append_reopen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("/work")).unwrap();
        session
            .append_model_change("local", "qwen", Some(32_000))
            .unwrap();
        session
            .append_message(&Message::User(UserMessage::text("hi")))
            .unwrap();
        session
            .append_message(&Message::Assistant(text_reply("hello")))
            .unwrap();

        let reopened = Session::open(session.path()).unwrap();
        assert_eq!(reopened.id(), session.id());
        assert_eq!(reopened.header().cwd, PathBuf::from("/work"));
        assert_eq!(reopened.entries().len(), 3);
        assert_eq!(reopened.leaf_id(), session.leaf_id());
        assert_eq!(
            reopened.current_model(),
            Some(SessionModel {
                provider: "local".into(),
                id: "qwen".into(),
                context_window: Some(32_000),
            })
        );
        let messages = reopened.context_messages();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::User(u) if u.plain_text() == "hi"));

        let first_line = std::fs::read_to_string(session.path())
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(first_line.starts_with("{\"type\":\"session\""));
    }

    #[test]
    fn moving_the_leaf_starts_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("/w")).unwrap();
        let first = session
            .append_message(&Message::User(UserMessage::text("one")))
            .unwrap();
        session
            .append_message(&Message::Assistant(text_reply("answer one")))
            .unwrap();
        session.set_leaf(Some(&first)).unwrap();
        session
            .append_message(&Message::Assistant(text_reply("answer two")))
            .unwrap();

        let branch = session.context_messages();
        assert_eq!(branch.len(), 2);
        assert!(matches!(&branch[1], Message::Assistant(a) if a.plain_text() == "answer two"));
        assert_eq!(session.entries().len(), 3, "nothing was rewritten");
        assert!(session.set_leaf(Some("nope")).is_err());
        session.set_leaf(None).unwrap();
        assert!(session.context_messages().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_and_missing_header_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("/w")).unwrap();
        session
            .append_message(&Message::User(UserMessage::text("ok")))
            .unwrap();
        let path = session.path().to_path_buf();
        drop(session);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{ this is not json").unwrap();
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 1);

        let headerless = dir.path().join("bad.jsonl");
        std::fs::write(&headerless, "{\"id\":\"x\",\"parent_id\":null,\"timestamp\":1,\"type\":\"model_change\",\"provider\":\"p\",\"model\":\"m\"}\n").unwrap();
        assert!(Session::open(&headerless).is_err());
    }

    #[test]
    fn compaction_entry_rebuilds_the_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("/w")).unwrap();
        for text in ["one", "two", "three"] {
            session
                .append_message(&Message::User(UserMessage::text(text)))
                .unwrap();
            session
                .append_message(&Message::Assistant(text_reply(&format!("answer {text}"))))
                .unwrap();
        }
        session.append_compaction("the gist", 5000, 2).unwrap();
        session
            .append_message(&Message::User(UserMessage::text("four")))
            .unwrap();

        let reopened = Session::open(session.path()).unwrap();
        let context = reopened.context_messages();
        assert_eq!(context.len(), 4);
        assert!(matches!(
            &context[0],
            Message::User(u) if u.plain_text().contains("the gist")
        ));
        assert!(matches!(&context[1], Message::User(u) if u.plain_text() == "three"));
        assert!(matches!(&context[3], Message::User(u) if u.plain_text() == "four"));
        assert_eq!(SessionSummary::from(&reopened).message_count, 7);
    }

    #[test]
    fn list_sorts_newest_first_with_first_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut older = Session::create(dir.path(), Path::new("/w")).unwrap();
        older
            .append_message(&Message::User(UserMessage::text("first task")))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut newer = Session::create(dir.path(), Path::new("/w")).unwrap();
        newer
            .append_message(&Message::User(UserMessage::text("second task")))
            .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        newer.set_name("the important one").unwrap();

        let list = Session::list(dir.path()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].first_prompt.as_deref(), Some("second task"));
        assert_eq!(list[1].first_prompt.as_deref(), Some("first task"));
        assert_eq!(list[0].message_count, 1);
        // A named session shows its name; an unnamed one its first prompt.
        assert_eq!(list[0].label(), "the important one");
        assert_eq!(list[1].label(), "first task");
        assert!(Session::list(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ids_are_unique() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.iter().all(|id| id.len() == 8));
    }
}
