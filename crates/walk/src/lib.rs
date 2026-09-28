//! Walking a project tree the way git sees it: what `.gitignore` leaves out
//! stays out. Shared by the file manager's project search, the open prompt's
//! suggestions and the agent's `@`-file completion.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::WalkBuilder;
pub use ignore::{DirEntry, Walk};

/// Every entry under `base`, hidden ones included. With `respect_gitignore`
/// the walk leaves out what git ignores and the `.git` directory itself, as
/// a project-wide search wants; without it, it sees everything, as a search
/// of the directory on screen does.
pub fn walker(base: &Path, respect_gitignore: bool) -> Walk {
    let mut builder = WalkBuilder::new(base);
    builder
        .hidden(false)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore);
    if respect_gitignore {
        builder.filter_entry(|entry| entry.file_name() != ".git");
    }
    builder.build()
}

/// The files under `root` a project-wide search offers, as paths relative to
/// `root`: what git ignores and `.git` left out, at most `limit` of them.
pub fn project_files(root: &Path, cancel: &AtomicBool, limit: usize) -> Vec<String> {
    let mut files = Vec::new();
    for entry in walker(root, true) {
        if cancel.load(Ordering::Relaxed) || files.len() >= limit {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            files.push(relative.display().to_string());
        }
    }
    files
}

/// The files and directories under `root` an `@`-mention offers, as paths
/// relative to `root` with a `/` after a directory: what git ignores, `.git`
/// and `.termide` left out, hidden entries too unless `hidden`, at most
/// `limit` visited — cheap enough to run on every keystroke in a large tree.
pub fn project_entries(root: &Path, hidden: bool, limit: usize) -> Vec<String> {
    WalkBuilder::new(root)
        .hidden(!hidden)
        .filter_entry(|entry| {
            let name = entry.file_name();
            name != ".git" && name != ".termide"
        })
        .build()
        .flatten()
        .filter(|entry| entry.depth() > 0)
        .take(limit)
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?;
            let mut path = relative.to_string_lossy().replace('\\', "/");
            // `Path::is_dir` follows a symlink to a directory.
            if entry.path().is_dir() {
                path.push('/');
            }
            Some(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_files_leave_out_what_git_ignores_and_the_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in [
            ".git/HEAD",
            ".github/workflows/ci.yml",
            "src/main.rs",
            "target/debug/app",
            "notes.log",
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();

        let never = AtomicBool::new(false);
        let mut files = project_files(root, &never, 100);
        files.sort();
        assert_eq!(
            files,
            [".github/workflows/ci.yml", ".gitignore", "src/main.rs"],
            "hidden files stay, ignored ones and .git go"
        );
        assert_eq!(project_files(root, &never, 1).len(), 1, "capped");
        assert!(project_files(root, &AtomicBool::new(true), 100).is_empty());
    }

    #[test]
    fn project_entries_mark_directories_and_hide_what_they_should() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in [
            ".git/HEAD",
            ".termide/config.toml",
            ".env",
            "src/main.rs",
            "target/app",
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let mut entries = project_entries(root, false, 100);
        entries.sort();
        assert_eq!(entries, ["src/", "src/main.rs"]);
        let mut entries = project_entries(root, true, 100);
        entries.sort();
        assert_eq!(entries, [".env", ".gitignore", "src/", "src/main.rs"]);
        assert_eq!(project_entries(root, true, 1).len(), 1, "capped");
    }
}
