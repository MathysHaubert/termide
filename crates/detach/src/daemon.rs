//! The instance host: a daemonised process that owns a PTY, runs termide
//! inside it, and lets clients come and go on a unix socket.
//!
//! The hosted termide is an ordinary termide — it is not aware of being
//! multiplexed beyond re-entering its terminal modes when a client arrives.
//! Everything that makes a instance survive a disconnect follows from the
//! daemon outliving the client: the shells, LSP servers and watchers are the
//! hosted process's children, so nothing has to be serialised or restored.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::paths;
use crate::protocol::{ClientFrame, ServerFrame};
use crate::registry::{self, SessionInfo};

/// Environment variable naming the socket of the instance a termide is hosted
/// in. Its presence is also how the app knows to offer the detach action.
pub const SOCKET_ENV: &str = "TERMIDE_DETACH_SOCK";

/// Environment variable carrying the instance id into the hosted termide.
pub const ID_ENV: &str = "TERMIDE_DETACH_ID";

/// PTY size used between the daemon starting and the first client attaching.
///
/// The hosted termide lays out against it for a few milliseconds at most; the
/// first attach resizes to the real terminal and triggers a full redraw.
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

/// Start a detached instance and return its id.
///
/// Returns in the parent process. The daemon is a forked child, so this must
/// be called before any thread is spawned — `fork` only carries the calling
/// thread into the child, and a lock held by a thread that no longer exists
/// would deadlock the daemon.
pub fn spawn_detached(project_root: &Path, files: &[PathBuf]) -> Result<String> {
    registry::prune_dead()?;

    let id = paths::allocate_id(project_root)?;
    let socket = paths::socket_path(&id)?;

    // Bind before forking so that a `--attach` racing the returning parent
    // finds a socket rather than "no such instance".
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("Failed to bind {}", socket.display()))?;
    restrict_socket(&socket)?;

    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            registry::write_info(&SessionInfo {
                id: id.clone(),
                pid: child.as_raw(),
                project: project_root.to_path_buf(),
                started: registry::now_unix(),
                attached: false,
            })?;
            // The parent's copy of the listener is closed here; dropping a
            // UnixListener does not unlink the socket file, so the daemon's
            // copy stays usable.
            drop(listener);
            Ok(id)
        }
        Ok(nix::unistd::ForkResult::Child) => {
            // Never unwind past fork: the child shares the parent's atexit
            // handlers and buffered stdio, so it leaves by _exit only.
            let code = match run_daemon(&id, listener, project_root, files) {
                Ok(()) => 0,
                Err(e) => {
                    log::error!("Detached instance '{id}' failed: {e:#}");
                    1
                }
            };
            std::process::exit(code);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&socket);
            Err(anyhow::anyhow!("Failed to fork the instance daemon: {e}"))
        }
    }
}

/// Make the socket owner-only, belt and braces over the 0700 parent directory.
fn restrict_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to restrict permissions on {}", path.display()))
}

/// Detach from the controlling terminal and point stdio at `/dev/null`.
///
/// Without this the daemon keeps the launching terminal's tty open: closing
/// the SSH instance would then deliver SIGHUP to it, which is precisely the
/// death the feature exists to avoid.
fn detach_from_terminal() -> Result<()> {
    nix::unistd::setsid().context("setsid failed")?;

    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .context("Failed to open /dev/null")?;
    let fd = devnull.as_raw_fd();
    // SAFETY: fd is open for the duration of the calls; dup2 onto the three
    // standard descriptors is the documented way to disconnect a daemon.
    unsafe {
        libc::dup2(fd, libc::STDIN_FILENO);
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
    }
    Ok(())
}

/// Tear a client connection down for real.
///
/// Dropping the stream is not enough: `serve_connection` holds a second
/// descriptor for the same socket, so the peer would see neither EOF nor an
/// error and would hang attached to a instance that has already let go of it.
/// `shutdown` acts on the socket itself, so both ends agree.
fn close_client(stream: Option<UnixStream>) {
    if let Some(stream) = stream {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Everything the accept loop and the PTY pump share.
struct Instance {
    id: String,
    /// The attached client's socket, or `None` while detached. Writing to it
    /// is the only thing the PTY pump does with a client, so one mutex over
    /// the stream is enough.
    client: Mutex<Option<UnixStream>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    pty_writer: Mutex<Box<dyn Write + Send>>,
    /// Pid of the hosted termide, signalled on attach to force a redraw.
    hosted_pid: Mutex<Option<i32>>,
}

impl Instance {
    /// Send a frame to the attached client, dropping it if the socket is gone.
    fn send(&self, frame: &ServerFrame) {
        let mut guard = self.client.lock().unwrap();
        let failed = match guard.as_mut() {
            Some(stream) => frame.write_to(stream).is_err(),
            None => false,
        };
        if failed {
            Self::close(guard.take());
            let _ = registry::set_attached(&self.id, false);
        }
    }

    fn close(stream: Option<UnixStream>) {
        close_client(stream);
    }

    fn resize(&self, cols: u16, rows: u16) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Err(e) = self.master.lock().unwrap().resize(size) {
            log::warn!("Failed to resize the instance PTY: {e}");
        }
    }

    /// Ask the hosted termide to re-enter its terminal modes and repaint.
    ///
    /// A fresh client's terminal knows nothing of the alternate screen, mouse
    /// reporting or bracketed paste the hosted process switched on when it
    /// started, and its screen is blank. SIGUSR1 is the wake-up; the app side
    /// turns it into a full re-initialisation.
    fn signal_reattach(&self) {
        let Some(pid) = *self.hosted_pid.lock().unwrap() else {
            return;
        };
        use nix::sys::signal::{kill, Signal};
        if let Err(e) = kill(nix::unistd::Pid::from_raw(pid), Signal::SIGUSR1) {
            log::warn!("Failed to signal the hosted process: {e}");
        }
    }

    fn detach_client(&self) {
        let mut guard = self.client.lock().unwrap();
        let stream = guard.take();
        let had_client = stream.is_some();
        Self::close(stream);
        if had_client {
            let _ = registry::set_attached(&self.id, false);
        }
    }
}

fn run_daemon(
    id: &str,
    listener: UnixListener,
    project_root: &Path,
    files: &[PathBuf],
) -> Result<()> {
    detach_from_terminal()?;

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("Failed to open a PTY for the detached instance")?;

    let exe = std::env::current_exe().context("Failed to locate the termide binary")?;
    let mut cmd = CommandBuilder::new(exe);
    for file in files {
        cmd.arg(file);
    }
    cmd.cwd(project_root);
    cmd.env(SOCKET_ENV, paths::socket_path(id)?);
    cmd.env(ID_ENV, id);

    // Block SIGUSR1 before spawning: the mask is inherited across fork and
    // exec, so the hosted termide starts with the reattach signal blocked
    // rather than fatal. Without this, a client attaching during the first
    // second of startup would kill the instance it just connected to.
    crate::reattach::block_signal()?;

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("Failed to start termide inside the instance PTY")?;

    // The daemon itself has no use for SIGUSR1.
    {
        let mut mask = nix::sys::signal::SigSet::empty();
        mask.add(nix::sys::signal::Signal::SIGUSR1);
        let _ = mask.thread_unblock();
    }
    // The daemon must not hold the slave open: with it open, the PTY never
    // reports EOF when the hosted process exits and the pump would block for
    // ever on a instance that is already over.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .context("Failed to clone the PTY reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("Failed to take the PTY writer")?;

    let instance = Arc::new(Instance {
        id: id.to_string(),
        client: Mutex::new(None),
        master: Mutex::new(pair.master),
        pty_writer: Mutex::new(writer),
        hosted_pid: Mutex::new(child.process_id().map(|p| p as i32)),
    });

    // Pump PTY output to whoever is attached. This thread runs even while
    // detached and discards what it reads: an unread master fills its buffer
    // within a page or two of output and would then block the hosted termide
    // on write, freezing a instance that is supposed to keep working.
    {
        let instance = Arc::clone(&instance);
        std::thread::spawn(move || pump_pty_output(instance, reader));
    }

    // Reap the hosted process and end the instance with it.
    {
        let instance = Arc::clone(&instance);
        let id = id.to_string();
        std::thread::spawn(move || {
            let status = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            instance.send(&ServerFrame::Exited(status));
            registry::remove(&id);
            std::process::exit(0);
        });
    }

    accept_loop(instance, listener)
}

fn pump_pty_output(instance: Arc<Instance>, mut reader: Box<dyn Read + Send>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => instance.send(&ServerFrame::Output(buf[..n].to_vec())),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn accept_loop(instance: Arc<Instance>, listener: UnixListener) -> Result<()> {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let instance = Arc::clone(&instance);
        std::thread::spawn(move || {
            if let Err(e) = serve_connection(instance, stream) {
                log::warn!("Instance connection ended with an error: {e:#}");
            }
        });
    }
    Ok(())
}

fn serve_connection(instance: Arc<Instance>, stream: UnixStream) -> Result<()> {
    let mut reader = stream.try_clone()?;

    let Some(first) = ClientFrame::read_from(&mut reader)? else {
        return Ok(());
    };

    match first {
        // The hosted termide asking to be released. It is a one-shot
        // connection: no attach, no stream to keep.
        ClientFrame::RequestDetach => {
            instance.detach_client();
            return Ok(());
        }
        ClientFrame::Attach {
            cols,
            rows,
            term,
            caps,
        } => {
            {
                let mut guard = instance.client.lock().unwrap();
                if guard.is_some() {
                    let mut stream = stream;
                    let _ = ServerFrame::Busy.write_to(&mut stream);
                    return Ok(());
                }
                *guard = Some(stream.try_clone()?);
            }

            // One line the hosted process parses on reattach: the terminal it
            // is now being looked at through.
            let _ = std::fs::write(
                paths::term_path(&instance.id)?,
                format!(
                    "{term}\nkitty={}\nssh={}\nvs16={}\n",
                    u8::from(caps.kitty),
                    u8::from(caps.via_ssh),
                    u8::from(caps.vs16_wide)
                ),
            );
            let _ = registry::set_attached(&instance.id, true);

            instance.send(&ServerFrame::Attached);
            instance.resize(cols, rows);
            instance.signal_reattach();
        }
        // Anything else before an attach is a confused peer; ignoring it costs
        // nothing and keeps a stray connect from disturbing the instance.
        _ => return Ok(()),
    }

    let result = client_loop(&instance, &mut reader);
    instance.detach_client();
    result
}

fn client_loop(instance: &Arc<Instance>, reader: &mut UnixStream) -> Result<()> {
    while let Some(frame) = ClientFrame::read_from(reader)? {
        match frame {
            ClientFrame::Input(bytes) => {
                let mut writer = instance.pty_writer.lock().unwrap();
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
            ClientFrame::Resize { cols, rows } => instance.resize(cols, rows),
            ClientFrame::Detach => break,
            ClientFrame::RequestDetach => break,
            ClientFrame::Attach { .. } => {}
        }
    }
    Ok(())
}

/// Ask the daemon hosting this process to drop its client.
///
/// Called by the in-app detach action. Returns `Ok(false)` when termide is not
/// running inside a detached instance, so the caller can tell the user why
/// nothing happened.
pub fn request_detach_from_host() -> Result<bool> {
    let Some(socket) = std::env::var_os(SOCKET_ENV) else {
        return Ok(false);
    };
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("Failed to reach the instance daemon at {socket:?}"))?;
    ClientFrame::RequestDetach.write_to(&mut stream)?;
    Ok(true)
}

/// The instance id this process is hosted in, if any.
pub fn hosted_instance_id() -> Option<String> {
    std::env::var(ID_ENV).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Regression: an in-app detach must reach the client even though the
    /// daemon still holds a second descriptor for the same socket in
    /// `serve_connection`. Dropping the stream alone leaves the peer blocked
    /// on a read for ever, attached to a instance that has released it.
    #[test]
    fn closing_a_client_is_visible_to_the_peer_despite_a_duplicate_fd() {
        let (daemon_side, mut peer) = UnixStream::pair().unwrap();
        let duplicate = daemon_side.try_clone().unwrap();

        close_client(Some(daemon_side));

        peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(peer.read(&mut buf).unwrap(), 0, "peer should see EOF");

        drop(duplicate);
    }
}
