//! The list of detached sessions, kept as one `<id>.info` sidecar per socket.
//!
//! There is no central index file: a session is whatever has a live socket in
//! the runtime directory. That keeps the registry self-healing — a daemon that
//! dies without cleaning up leaves a socket whose pid no longer exists, and
//! [`prune_dead`] removes it on the next listing.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;

/// What `--list-sessions` shows for one detached session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    /// Pid of the daemon, not of the hosted termide: the daemon is what owns
    /// the socket, so its liveness is what decides whether the session exists.
    pub pid: i32,
    pub project: PathBuf,
    /// Seconds since the Unix epoch.
    pub started: u64,
    pub attached: bool,
}

impl SessionInfo {
    /// How long the session has been up, as a compact `3d 4h` / `5m` string.
    pub fn uptime(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(self.started);
        let secs = now.saturating_sub(self.started);

        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3_600;
        let minutes = (secs % 3_600) / 60;

        if days > 0 {
            format!("{days}d {hours}h")
        } else if hours > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{minutes}m")
        }
    }

    fn serialise(&self) -> String {
        format!(
            "pid={}\nproject={}\nstarted={}\nattached={}\n",
            self.pid,
            self.project.display(),
            self.started,
            u8::from(self.attached)
        )
    }

    fn parse(id: &str, text: &str) -> Option<Self> {
        let mut pid = None;
        let mut project = None;
        let mut started = None;
        let mut attached = false;

        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "pid" => pid = value.parse().ok(),
                "project" => project = Some(PathBuf::from(value)),
                "started" => started = value.parse().ok(),
                "attached" => attached = value == "1",
                _ => {}
            }
        }

        Some(SessionInfo {
            id: id.to_string(),
            pid: pid?,
            project: project?,
            started: started.unwrap_or(0),
            attached,
        })
    }
}

/// Current wall-clock time as seconds since the epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write (or overwrite) the sidecar for a session.
pub fn write_info(info: &SessionInfo) -> Result<()> {
    let path = paths::info_path(&info.id)?;
    std::fs::write(&path, info.serialise())
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// Read one session's sidecar, if it is present and parsable.
pub fn read_info(id: &str) -> Option<SessionInfo> {
    let path = paths::info_path(id).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    SessionInfo::parse(id, &text)
}

/// Mark a session attached or detached, leaving the rest of the sidecar alone.
pub fn set_attached(id: &str, attached: bool) -> Result<()> {
    if let Some(mut info) = read_info(id) {
        info.attached = attached;
        write_info(&info)?;
    }
    Ok(())
}

/// Delete every file belonging to a session.
///
/// All three of them: a leftover `.term` is small but permanent, and since
/// `prune_dead` routes through here, anything this function forgets accretes
/// in the runtime directory for as long as the machine lives.
pub fn remove(id: &str) {
    for path in [
        paths::socket_path(id),
        paths::info_path(id),
        paths::term_path(id),
    ]
    .into_iter()
    .flatten()
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a process is still around.
///
/// `kill(pid, 0)` asks the kernel without touching the daemon, which is why
/// liveness is probed this way rather than by connecting to the socket — a
/// connect would arrive at the daemon as a client and have to be rejected.
#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // Err(EPERM) means the process exists under another uid, which cannot
    // happen for our own runtime directory but still counts as alive.
    !matches!(
        kill(Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[cfg(not(unix))]
fn process_is_alive(_pid: i32) -> bool {
    false
}

/// Drop sockets whose daemon is gone. Returns the ids that were removed.
pub fn prune_dead() -> Result<Vec<String>> {
    let mut pruned = Vec::new();
    for info in scan()? {
        if !process_is_alive(info.pid) {
            remove(&info.id);
            pruned.push(info.id);
        }
    }
    Ok(pruned)
}

/// Every session with a socket in the runtime directory, dead ones included.
fn scan() -> Result<Vec<SessionInfo>> {
    let dir = paths::runtime_dir()?;
    let mut found = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", dir.display())),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match read_info(id) {
            Some(info) => found.push(info),
            // A socket with no readable sidecar is unusable — nothing can be
            // reported about it and nothing can attach — so treat it as dead.
            None => remove(id),
        }
    }

    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

/// Live sessions, with dead entries pruned as a side effect.
pub fn list() -> Result<Vec<SessionInfo>> {
    prune_dead()?;
    scan()
}

/// The session `--attach` should pick when the user names none.
///
/// The most recently started one: with a single session it is unambiguous,
/// and with several it matches "the one I just detached from".
pub fn most_recent() -> Result<Option<SessionInfo>> {
    Ok(list()?.into_iter().max_by_key(|s| s.started))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionInfo {
        SessionInfo {
            id: "termide".to_string(),
            pid: 4242,
            project: PathBuf::from("/home/u/termide"),
            started: 1_700_000_000,
            attached: true,
        }
    }

    // Regression: `remove` used to delete the socket and the sidecar but not
    // the `.term` handover file, so every session that ever ran left one
    // behind — for ever, since pruning goes through this same function.
    #[test]
    fn remove_deletes_every_file_a_session_owns() {
        let id = "test-remove-all-files";
        let paths = [
            paths::socket_path(id).unwrap(),
            paths::info_path(id).unwrap(),
            paths::term_path(id).unwrap(),
        ];
        for path in &paths {
            std::fs::write(path, b"x").unwrap();
        }

        remove(id);

        for path in &paths {
            assert!(!path.exists(), "{} should be gone", path.display());
        }
    }

    #[test]
    fn info_round_trips_through_the_sidecar_format() {
        let info = sample();
        let parsed = SessionInfo::parse("termide", &info.serialise()).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn parsing_needs_a_pid_and_a_project() {
        assert!(SessionInfo::parse("x", "pid=1\n").is_none());
        assert!(SessionInfo::parse("x", "project=/tmp\n").is_none());
        assert!(SessionInfo::parse("x", "pid=1\nproject=/tmp\n").is_some());
    }

    #[test]
    fn unknown_keys_are_ignored_so_the_format_can_grow() {
        let text = "pid=7\nproject=/tmp\nstarted=5\nattached=1\nfuture=yes\n";
        let parsed = SessionInfo::parse("x", text).unwrap();
        assert_eq!(parsed.pid, 7);
        assert!(parsed.attached);
    }

    #[test]
    fn uptime_is_reported_in_the_largest_useful_unit() {
        let mut info = sample();
        info.started = now_unix().saturating_sub(90);
        assert_eq!(info.uptime(), "1m");

        info.started = now_unix().saturating_sub(3 * 3_600 + 300);
        assert_eq!(info.uptime(), "3h 5m");

        info.started = now_unix().saturating_sub(2 * 86_400 + 4 * 3_600);
        assert_eq!(info.uptime(), "2d 4h");
    }
}
