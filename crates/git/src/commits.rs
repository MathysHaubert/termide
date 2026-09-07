//! Git commit information and log functions.
//!
//! Provides types and functions for working with git commit history.

use std::path::Path;

use crate::command::git_command_stdout;

/// Commit information
#[derive(Debug, Clone)]
pub struct CommitInfo {
    /// Commit hash (short form)
    pub hash: String,
    /// Author name
    pub author: String,
    /// Commit date
    pub date: String,
    /// Commit message (first line)
    pub message: String,
    /// Graph line for display (if using --graph)
    pub graph: Option<String>,
    /// Refs pointing to this commit (HEAD, branches, tags)
    pub refs: Option<String>,
}

/// Get commit log with graph.
///
/// If `branch` is `Some(name)`, shows the log for that branch instead of HEAD.
pub fn get_log_with_graph(repo: &Path, count: usize, branch: Option<&str>) -> Vec<CommitInfo> {
    let count_flag = format!("-{}", count);

    // Use a special format that includes graph and refs
    // Format: hash, author, date, refs, message
    let mut args = vec![
        "log",
        count_flag.as_str(),
        "--graph",
        "--format=%h\t%an\t%ar\t%d\t%s",
    ];
    if let Some(b) = branch {
        args.push(b);
    }
    git_command_stdout(repo, &args)
        .map(|stdout| {
            stdout
                .lines()
                .filter_map(|line| {
                    // Graph lines start with *, |, /, \ or space
                    // Find where the actual commit info starts
                    let graph_end = line.find(|c: char| c.is_ascii_hexdigit()).unwrap_or(0);

                    let graph = if graph_end > 0 {
                        Some(line[..graph_end].to_string())
                    } else {
                        None
                    };

                    let info_part = &line[graph_end..];
                    let parts: Vec<&str> = info_part.splitn(5, '\t').collect();

                    if parts.len() == 5 {
                        let refs = if parts[3].is_empty() {
                            None
                        } else {
                            Some(parts[3].trim().to_string())
                        };
                        Some(CommitInfo {
                            hash: parts[0].to_string(),
                            author: parts[1].to_string(),
                            date: parts[2].to_string(),
                            message: parts[4].to_string(),
                            graph,
                            refs,
                        })
                    } else if !info_part.trim().is_empty() {
                        // Handle graph-only lines (merge indicators)
                        Some(CommitInfo {
                            hash: String::new(),
                            author: String::new(),
                            date: String::new(),
                            message: String::new(),
                            graph,
                            refs: None,
                        })
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Get commit log with a Unicode box-drawing graph computed from commit
/// parents (see [`crate::graph`]).
///
/// Unlike [`get_log_with_graph`], which restyles git's ASCII `--graph`, this
/// lays the graph out itself from `%p` (parent hashes), yielding proper
/// junctions (`● │ ├ ╮ ╯`). The result has the same shape: commit rows carry
/// metadata, connector rows carry only `graph` with empty metadata.
pub fn get_log_graph_unicode(repo: &Path, count: usize, branch: Option<&str>) -> Vec<CommitInfo> {
    let count_flag = format!("-{}", count);
    // Trailing %p adds the space-separated parent hashes for the layout engine.
    let mut args = vec![
        "log",
        count_flag.as_str(),
        "--format=%h\t%an\t%ar\t%d\t%s\t%p",
    ];
    if let Some(b) = branch {
        args.push(b);
    }
    let Some(stdout) = git_command_stdout(repo, &args) else {
        return Vec::new();
    };

    // One pass: metadata and parent lists stay index-aligned.
    let mut metas: Vec<CommitInfo> = Vec::new();
    let mut graph_commits: Vec<crate::graph::GraphCommit> = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(6, '\t').collect();
        if parts.len() != 6 {
            continue;
        }
        let refs = if parts[3].is_empty() {
            None
        } else {
            Some(parts[3].trim().to_string())
        };
        metas.push(CommitInfo {
            hash: parts[0].to_string(),
            author: parts[1].to_string(),
            date: parts[2].to_string(),
            message: parts[4].to_string(),
            graph: None,
            refs,
        });
        graph_commits.push(crate::graph::GraphCommit {
            hash: parts[0].to_string(),
            parents: parts[5].split_whitespace().map(str::to_string).collect(),
        });
    }

    let rows = crate::graph::render_graph(&graph_commits);
    assemble_graph_rows(rows, metas)
}

/// Fold layout rows and commit metadata into [`CommitInfo`]s.
///
/// Commit rows take their metadata from `metas[i]` and get their graph padded
/// to the widest row plus a one-space gutter (so hashes line up); connector
/// rows become metadata-less entries carrying only the graph.
fn assemble_graph_rows(
    rows: Vec<crate::graph::GraphRow>,
    metas: Vec<CommitInfo>,
) -> Vec<CommitInfo> {
    // Every graph glyph is one cell wide, so char count is the display width.
    let graph_w = rows
        .iter()
        .map(|r| r.graph.chars().count())
        .max()
        .unwrap_or(0);

    rows.into_iter()
        .map(|row| match row.commit {
            Some(i) => {
                let mut info = metas[i].clone();
                let pad = graph_w - row.graph.chars().count() + 1;
                info.graph = Some(format!("{}{}", row.graph, " ".repeat(pad)));
                info
            }
            None => CommitInfo {
                hash: String::new(),
                author: String::new(),
                date: String::new(),
                message: String::new(),
                graph: Some(row.graph),
                refs: None,
            },
        })
        .collect()
}

/// Get diff for a specific commit (with full patch)
pub fn get_commit_diff(repo: &Path, hash: &str) -> Option<String> {
    git_command_stdout(repo, &["show", "--stat", "--patch", hash])
}

/// Get file diff (for diff viewer)
pub fn get_file_diff(repo: &Path, file: &Path, staged: bool) -> Option<String> {
    let file_str = file.to_string_lossy();
    if staged {
        git_command_stdout(repo, &["diff", "--cached", "--", &file_str])
    } else {
        git_command_stdout(repo, &["diff", "--", &file_str])
    }
}

/// Detailed commit information for modal display.
#[derive(Debug, Clone)]
pub struct CommitDetails {
    /// Full commit hash.
    pub hash: String,
    /// Author as "Name <email>".
    pub author: String,
    /// Absolute date (ISO format).
    pub date: String,
    /// Full commit message (subject + body).
    pub message: String,
    /// Number of files changed.
    pub files_changed: usize,
    /// Number of insertions.
    pub insertions: usize,
    /// Number of deletions.
    pub deletions: usize,
    /// Number of files added.
    pub files_added: usize,
    /// Number of files deleted.
    pub files_deleted: usize,
    /// Number of files modified.
    pub files_modified: usize,
}

/// Get detailed information about a specific commit.
pub fn get_commit_details(repo: &Path, short_hash: &str) -> Option<CommitDetails> {
    let output = git_command_stdout(
        repo,
        &[
            "show",
            "--format=%H%n%an <%ae>%n%ai%n%B",
            "--shortstat",
            short_hash,
        ],
    )?;

    // The output has: format lines, then diff content, then shortstat line at the end.
    // Format: hash\nauthor\ndate\nmessage_lines...\n\n shortstat_line
    let mut lines = output.lines();

    let hash = lines.next()?.to_string();
    let author = lines.next()?.to_string();
    let date = lines.next()?.to_string();

    // Collect message lines until we hit an empty line followed by diff/shortstat.
    // The %B format ends with an empty line, then git show appends diff output.
    // We need to collect the message and find the shortstat line at the very end.
    let remaining: Vec<&str> = lines.collect();

    // The shortstat line is the last non-empty line.
    // Message is everything between current position and the diff/shortstat section.
    // Find shortstat line (contains "file(s) changed" or is empty for no changes).
    let mut files_changed = 0;
    let mut insertions = 0;
    let mut deletions = 0;
    let mut shortstat_idx = None;

    for (i, line) in remaining.iter().enumerate().rev() {
        let trimmed = line.trim();
        if trimmed.contains("changed") && (trimmed.contains("file") || trimmed.contains("files")) {
            // Parse shortstat: " 3 files changed, 10 insertions(+), 5 deletions(-)"
            for part in trimmed.split(',') {
                let part = part.trim();
                if part.contains("file") {
                    files_changed = part
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                } else if part.contains("insertion") {
                    insertions = part
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                } else if part.contains("deletion") {
                    deletions = part
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                }
            }
            shortstat_idx = Some(i);
            break;
        }
    }

    // Message: lines from start of remaining until diff content begins.
    // The message from %B ends with a trailing newline, so we trim trailing empty lines.
    // After the message, there may be diff lines before shortstat.
    // We take lines up to the first diff header ("diff --git") or shortstat.
    let message_end = remaining
        .iter()
        .position(|line| line.starts_with("diff --git"))
        .or(shortstat_idx)
        .unwrap_or(remaining.len());

    let message = remaining[..message_end].join("\n").trim_end().to_string();

    // Get per-file status breakdown (added/deleted/modified)
    let (files_added, files_deleted, files_modified) = if let Some(output) = git_command_stdout(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "-r",
            "--name-status",
            short_hash,
        ],
    ) {
        let mut a = 0;
        let mut d = 0;
        let mut m = 0;
        for line in output.lines() {
            match line.chars().next() {
                Some('A') => a += 1,
                Some('D') => d += 1,
                _ => m += 1, // M, R, T, etc. -> modified
            }
        }
        (a, d, m)
    } else {
        (0, 0, files_changed)
    };

    Some(CommitDetails {
        hash,
        author,
        date,
        message,
        files_changed,
        insertions,
        deletions,
        files_added,
        files_deleted,
        files_modified,
    })
}

/// Diff statistics for a file.
#[derive(Debug, Clone, Default)]
pub struct DiffStats {
    /// Number of lines added.
    pub additions: usize,
    /// Number of lines deleted.
    pub deletions: usize,
}

/// Get diff stats for a file (additions/deletions count).
pub fn get_file_diff_stats(repo: &Path, file: &Path, staged: bool) -> DiffStats {
    let file_str = file.to_string_lossy();
    let args: Vec<&str> = if staged {
        vec!["diff", "--cached", "--numstat", "--", &file_str]
    } else {
        vec!["diff", "--numstat", "--", &file_str]
    };

    let output = git_command_stdout(repo, &args);

    // Parse: "10\t5\tfilename" -> additions=10, deletions=5
    if let Some(text) = output {
        if let Some(line) = text.lines().next() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 2 {
                return DiffStats {
                    additions: parts[0].parse().unwrap_or(0),
                    deletions: parts[1].parse().unwrap_or(0),
                };
            }
        }
    }
    DiffStats::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphRow;

    fn meta(hash: &str) -> CommitInfo {
        CommitInfo {
            hash: hash.to_string(),
            author: "a".into(),
            date: "d".into(),
            message: "m".into(),
            graph: None,
            refs: None,
        }
    }

    #[test]
    fn assemble_aligns_commit_rows_and_keeps_connectors() {
        let rows = vec![
            GraphRow {
                graph: "●".into(),
                commit: Some(0),
            },
            GraphRow {
                graph: "├╮".into(),
                commit: None,
            },
            GraphRow {
                graph: "│●".into(),
                commit: Some(1),
            },
        ];
        let out = assemble_graph_rows(rows, vec![meta("aaa"), meta("bbb")]);

        // Connector row: untouched graph, empty metadata.
        assert_eq!(out[1].graph.as_deref(), Some("├╮"));
        assert!(out[1].hash.is_empty());

        // Commit rows: metadata mapped in order, graphs padded to a common
        // width (2) + a one-space gutter = 3 cells, so hashes line up.
        assert_eq!(out[0].hash, "aaa");
        assert_eq!(out[2].hash, "bbb");
        let w0 = out[0].graph.as_deref().unwrap().chars().count();
        let w2 = out[2].graph.as_deref().unwrap().chars().count();
        assert_eq!(w0, 3, "‘●’ padded to width+gutter");
        assert_eq!(w2, 3, "‘│●’ padded to width+gutter");
    }
}
