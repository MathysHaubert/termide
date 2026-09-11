//! Filesystem layout for detached sessions.
//!
//! Sockets and their sidecar `.info` files live in a per-user runtime
//! directory, never in a shared `/tmp`: a world-writable directory invites
//! symlink races on a multi-user host, and a stale socket there would let
//! another account impersonate a session.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Directory holding one `<id>.sock` + `<id>.info` pair per detached session.
///
/// `dirs::runtime_dir()` is `$XDG_RUNTIME_DIR` on Linux/BSD and `None` on
/// macOS, where the data directory is the closest per-user equivalent that
/// survives for the lifetime of the login session.
pub fn runtime_dir() -> Result<PathBuf> {
    let base = match dirs::runtime_dir() {
        Some(dir) => dir.join("termide"),
        None => dirs::data_dir()
            .map(|p| p.join("termide").join("run"))
            .context("Failed to determine a runtime directory for detached sessions")?,
    };
    std::fs::create_dir_all(&base)
        .with_context(|| format!("Failed to create {}", base.display()))?;
    restrict_to_owner(&base)?;
    Ok(base)
}

/// Make a directory owner-only (`0700`).
///
/// Sockets inherit the directory's protection: a connect() needs search
/// permission on every component of the path, so an owner-only parent is
/// what keeps another local user from attaching to the session.
#[cfg(unix)]
fn restrict_to_owner(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, perms)
        .with_context(|| format!("Failed to restrict permissions on {}", dir.display()))
}

#[cfg(not(unix))]
fn restrict_to_owner(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Path of the control socket for session `id`.
pub fn socket_path(id: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{id}.sock")))
}

/// Path of the metadata sidecar for session `id`.
pub fn info_path(id: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{id}.info")))
}

/// Turn a project directory into a session id stem.
///
/// The stem is the directory name reduced to characters that are safe both
/// as a filename and as something the user retypes into `--attach`.
pub fn id_stem_for_project(project_root: &Path) -> String {
    let raw = project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

/// First free id for `project_root`: the bare stem, then `stem-2`, `stem-3`, …
///
/// "Free" means no socket file with that name exists. A socket left behind by
/// a crashed daemon is reclaimed by [`crate::registry::prune_dead`], which the
/// caller runs first, so this only ever skips ids that are genuinely live.
pub fn allocate_id(project_root: &Path) -> Result<String> {
    let stem = id_stem_for_project(project_root);
    for suffix in 1..1000u32 {
        let candidate = if suffix == 1 {
            stem.clone()
        } else {
            format!("{stem}-{suffix}")
        };
        if !socket_path(&candidate)?.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("Too many detached sessions for project '{stem}'")
}

/// Path of the `$TERM` handover file for session `id`.
///
/// Written by the daemon on every attach and read by the hosted termide when
/// it refreshes its capabilities: the hosted process inherited its own `TERM`
/// from whichever terminal started the session, which says nothing about the
/// terminal now looking at it.
pub fn term_path(id: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{id}.term")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_stem_uses_the_directory_name() {
        assert_eq!(id_stem_for_project(Path::new("/home/u/termide")), "termide");
    }

    #[test]
    fn id_stem_sanitises_and_lowercases() {
        assert_eq!(
            id_stem_for_project(Path::new("/home/u/My Project!")),
            "my-project"
        );
    }

    #[test]
    fn id_stem_falls_back_when_there_is_no_name() {
        assert_eq!(id_stem_for_project(Path::new("/")), "session");
        assert_eq!(id_stem_for_project(Path::new("...")), "session");
    }
}
