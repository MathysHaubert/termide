//! Copying archive entries out to the local filesystem.
//!
//! Invariant: symlinks from the archive are created only after every file
//! has been written, so no extracted file is ever written through a link the
//! archive planted. Link targets that lexically leave the extracted subtree
//! are not created at all.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use super::index::{symlink_target_key, EntrySource, Node, NodeKind};
use super::source::copy_error;
use super::OpenArchive;
use crate::error::{VfsError, VfsResult};
use crate::types::{DownloadProgress, VfsMetadata};

const COPY_BUFFER: usize = 256 * 1024;

/// Pause and cancel flags shared with the operation handle.
pub(crate) struct Control {
    pub(crate) pause: Arc<AtomicBool>,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl Control {
    pub(crate) fn none() -> Self {
        Self {
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Block while paused; fail once cancelled.
    fn checkpoint(&self) -> VfsResult<()> {
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(VfsError::Cancelled);
            }
            if !self.pause.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// One archive entry and every local file that receives its bytes (more
/// than one when hard links point at it).
struct Job {
    size: u64,
    dests: Vec<(PathBuf, VfsMetadata)>,
}

struct Progress<'a> {
    tx: Option<&'a Sender<DownloadProgress>>,
    state: DownloadProgress,
}

impl Progress<'_> {
    fn start_file(&mut self, name: &Path, size: u64) {
        self.state.current_file = name.file_name().map(|n| n.to_string_lossy().into_owned());
        self.state.current_file_bytes = 0;
        self.state.current_file_total = size;
        self.send();
    }

    fn advance(&mut self, bytes: u64) {
        self.state.bytes_downloaded += bytes;
        self.state.current_file_bytes += bytes;
        self.send();
    }

    fn finish_file(&mut self) {
        self.state.files_downloaded += 1;
        self.send();
    }

    fn send(&self) {
        if let Some(tx) = self.tx {
            let _ = tx.send(self.state.clone());
        }
    }
}

/// Extract the entry at inner path `key` (a file or a whole directory) to
/// the local path `dest`, which names the extracted item itself.
pub(crate) fn extract(
    archive: &OpenArchive,
    key: &Path,
    dest: &Path,
    control: &Control,
    progress_tx: Option<&Sender<DownloadProgress>>,
) -> VfsResult<PathBuf> {
    let (top, node) = archive
        .index
        .resolve(key)
        .ok_or_else(|| VfsError::NotFound {
            path: key.to_path_buf(),
        })?;

    let mut jobs: BTreeMap<EntrySource, Job> = BTreeMap::new();
    let mut dirs = Vec::new();
    let mut links = Vec::new();

    match node.kind {
        NodeKind::File(source) => add_job(&mut jobs, source, node, dest.to_path_buf()),
        NodeKind::Dir => {
            for (path, node) in archive.index.subtree(&top) {
                let rel = path.strip_prefix(&top).unwrap_or(Path::new(""));
                let target = dest.join(rel);
                match &node.kind {
                    NodeKind::Dir => dirs.push(target),
                    NodeKind::File(source) => add_job(&mut jobs, *source, node, target),
                    NodeKind::HardLink(_) => match archive.index.resolve(path) {
                        Some((_, resolved)) => match resolved.kind {
                            NodeKind::File(source) => add_job(&mut jobs, source, resolved, target),
                            _ => log::warn!("archive: hard link to a non-file {}", path.display()),
                        },
                        None => log::warn!("archive: dangling hard link {}", path.display()),
                    },
                    NodeKind::Symlink(link) => match symlink_target_key(path, link) {
                        Some(resolved) if resolved.starts_with(&top) => {
                            links.push((target, link.clone()))
                        }
                        _ => log::warn!(
                            "archive: not creating {} -> {link}, it leaves the extracted tree",
                            path.display()
                        ),
                    },
                }
            }
        }
        NodeKind::Symlink(_) | NodeKind::HardLink(_) => unreachable!("resolve follows links"),
    }

    let mut progress = Progress {
        tx: progress_tx,
        state: DownloadProgress {
            bytes_downloaded: 0,
            total_bytes: jobs.values().map(|j| j.size * j.dests.len() as u64).sum(),
            current_file: None,
            files_downloaded: 0,
            total_files: jobs.values().map(|j| j.dests.len()).sum(),
            current_file_bytes: 0,
            current_file_total: 0,
        },
    };
    progress.send();

    for dir in &dirs {
        fs::create_dir_all(dir)?;
    }

    let sources: Vec<EntrySource> = jobs.keys().copied().collect();
    archive.backend.read_sources(&sources, |source, reader| {
        let job = &jobs[&source];
        let (first, first_meta) = &job.dests[0];
        progress.start_file(first, job.size);
        write_entry(reader, first, job.size, control, &mut progress)?;
        apply_metadata(first, first_meta);
        progress.finish_file();
        for (copy, meta) in &job.dests[1..] {
            control.checkpoint()?;
            progress.start_file(copy, job.size);
            if let Some(parent) = copy.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(first, copy)?;
            apply_metadata(copy, meta);
            progress.advance(job.size);
            progress.finish_file();
        }
        Ok(())
    })?;

    for (target, link) in links {
        create_symlink(&link, &target);
    }
    Ok(dest.to_path_buf())
}

/// Read one whole file entry into memory.
pub(crate) fn read_to_vec(archive: &OpenArchive, key: &Path) -> VfsResult<Vec<u8>> {
    let (_, node) = archive
        .index
        .resolve(key)
        .ok_or_else(|| VfsError::NotFound {
            path: key.to_path_buf(),
        })?;
    let NodeKind::File(source) = node.kind else {
        return Err(VfsError::InvalidPath(format!(
            "{} is a directory",
            key.display()
        )));
    };
    let size = node.meta.size;
    let mut data = Vec::new();
    archive.backend.read_sources(&[source], |_, reader| {
        let mut limited = reader.take(size.saturating_add(1));
        limited.read_to_end(&mut data).map_err(copy_error)?;
        check_size(data.len() as u64, size)
    })?;
    Ok(data)
}

fn add_job(jobs: &mut BTreeMap<EntrySource, Job>, source: EntrySource, node: &Node, dest: PathBuf) {
    jobs.entry(source)
        .or_insert_with(|| Job {
            size: node.meta.size,
            dests: Vec::new(),
        })
        .dests
        .push((dest, node.meta.clone()));
}

/// Stream one entry into `dest`, never writing more than its declared size:
/// an entry that decompresses to more than it claims (a zip bomb) fails
/// instead of filling the disk. A partial file is removed on failure.
fn write_entry(
    reader: &mut dyn Read,
    dest: &Path,
    size: u64,
    control: &Control,
    progress: &mut Progress<'_>,
) -> VfsResult<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let result = (|| {
        let mut out = File::create(dest)?;
        let mut buf = vec![0u8; COPY_BUFFER];
        let mut written = 0u64;
        loop {
            control.checkpoint()?;
            let n = reader.read(&mut buf).map_err(copy_error)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            check_size_bound(written, size)?;
            out.write_all(&buf[..n])?;
            progress.advance(n as u64);
        }
        check_size(written, size)
    })();
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

fn check_size_bound(read: u64, declared: u64) -> VfsResult<()> {
    if read > declared {
        return Err(VfsError::Archive(format!(
            "entry is larger than its declared size of {declared} bytes"
        )));
    }
    Ok(())
}

fn check_size(read: u64, declared: u64) -> VfsResult<()> {
    check_size_bound(read, declared)?;
    if read < declared {
        return Err(VfsError::Archive(format!(
            "entry is truncated: {read} of {declared} bytes"
        )));
    }
    Ok(())
}

/// Best-effort: restore the modification time and the permission bits
/// (setuid, setgid and sticky are dropped).
fn apply_metadata(path: &Path, meta: &VfsMetadata) {
    if let Some(modified) = meta.modified {
        if let Ok(file) = File::options().write(true).open(path) {
            let _ = file.set_modified(modified);
        }
    }
    #[cfg(unix)]
    if let Some(mode) = meta.permissions {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777));
    }
}

#[cfg(unix)]
fn create_symlink(link: &str, target: &Path) {
    if let Err(e) = std::os::unix::fs::symlink(link, target) {
        log::warn!(
            "archive: cannot create symlink {} -> {link}: {e}",
            target.display()
        );
    }
}

#[cfg(not(unix))]
fn create_symlink(link: &str, target: &Path) {
    log::warn!(
        "archive: symlinks are not extracted on this platform: {} -> {link}",
        target.display()
    );
}
