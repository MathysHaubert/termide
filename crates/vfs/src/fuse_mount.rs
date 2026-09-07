//! NFS VFS provider via FUSE mount.
//!
//! This provider mounts NFS shares using fuse-nfs or mount.nfs and then
//! accesses them through the local filesystem provider.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::error::{VfsError, VfsResult};
use crate::local::LocalFileSystem;
use crate::traits::VfsProvider;
use crate::types::{
    AuthMethod, ConnectOptions, ConnectionState, VfsEntry, VfsMetadata, VfsOperation, VfsPath,
    VfsProtocol,
};

/// Default NFS port.
const DEFAULT_PORT: u16 = 2049;

/// NFS provider that mounts via FUSE.
///
/// This provider requires either `fuse-nfs` or `mount.nfs` to be installed.
pub struct NfsProvider {
    /// NFS server hostname.
    host: String,
    /// NFS export path.
    export_path: String,
    /// NFS port (usually 2049).
    port: u16,
    /// Local mount point.
    mount_point: Option<PathBuf>,
    /// Current connection state.
    state: ConnectionState,
    /// Local filesystem provider for accessing mounted directory.
    local: Option<LocalFileSystem>,
    /// Child process for FUSE mount.
    mount_process: Arc<Mutex<Option<std::process::Child>>>,
}

impl NfsProvider {
    /// Create a new NFS provider.
    pub fn new(host: &str, export_path: &str, port: Option<u16>) -> Self {
        Self {
            host: host.to_string(),
            export_path: export_path.to_string(),
            port: port.unwrap_or(DEFAULT_PORT),
            mount_point: None,
            state: ConnectionState::Disconnected,
            local: None,
            mount_process: Arc::new(Mutex::new(None)),
        }
    }

    /// Get the mount base directory.
    fn mount_base_dir() -> PathBuf {
        // Try XDG_RUNTIME_DIR first, then fall back to /tmp
        std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("termide")
            .join("mounts")
    }

    /// Check if fusermount is available.
    fn check_fuse_available() -> bool {
        Command::new("fusermount")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Check if fuse-nfs is available.
    fn check_fuse_nfs_available() -> bool {
        Command::new("fuse-nfs")
            .arg("--help")
            .output()
            .map(|_| true)
            .unwrap_or(false)
    }

    /// Create a unique mount point directory.
    fn create_mount_point(&self) -> VfsResult<PathBuf> {
        let base = Self::mount_base_dir();
        std::fs::create_dir_all(&base).map_err(VfsError::Io)?;

        // Create a unique subdirectory for this mount
        let safe_host = self.host.replace(['.', ':'], "_");
        let safe_export = self.export_path.trim_start_matches('/').replace('/', "_");
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let mount_dir = base.join(format!("{}_{}_{}", safe_host, safe_export, timestamp));
        std::fs::create_dir_all(&mount_dir).map_err(VfsError::Io)?;

        Ok(mount_dir)
    }

    /// Mount NFS share using fuse-nfs.
    fn mount_with_fuse_nfs(&mut self, mount_point: &Path) -> VfsResult<std::process::Child> {
        // fuse-nfs -n nfs://server/export /mount/point
        let nfs_url = format!("nfs://{}:{}{}", self.host, self.port, self.export_path);

        log::info!("NFS: Mounting {} to {:?}", nfs_url, mount_point);

        let child = Command::new("fuse-nfs")
            .arg("-n")
            .arg(&nfs_url)
            .arg(mount_point)
            .spawn()
            .map_err(|e| VfsError::RemoteError {
                message: format!("Failed to run fuse-nfs: {}", e),
            })?;

        // Give it a moment to mount
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Check if mount point has content
        if std::fs::read_dir(mount_point).is_ok() {
            Ok(child)
        } else {
            Err(VfsError::RemoteError {
                message: "Mount appears to have failed - mount point is not accessible".to_string(),
            })
        }
    }

    /// Unmount the NFS share.
    fn unmount(&mut self) {
        if let Some(mount_point) = self.mount_point.take() {
            // Try fusermount first
            let _ = Command::new("fusermount")
                .arg("-u")
                .arg(&mount_point)
                .output();

            // Kill the fuse process if it's still running
            if let Ok(mut guard) = self.mount_process.lock() {
                if let Some(mut child) = guard.take() {
                    let _ = child.kill();
                }
            }

            // Remove the mount point directory
            let _ = std::fs::remove_dir(&mount_point);
        }

        self.local = None;
        self.state = ConnectionState::Disconnected;
    }

    /// Convert VfsPath to local path on mount point.
    fn to_local_path(&self, path: &VfsPath) -> VfsResult<PathBuf> {
        if !matches!(path.protocol, VfsProtocol::Nfs) {
            return Err(VfsError::InvalidPath(format!(
                "Expected NFS path, got: {}",
                path
            )));
        }

        let mount_point = self.mount_point.as_ref().ok_or(VfsError::NotConnected)?;

        // The path in VfsPath is relative to the export
        let relative_path = path
            .path
            .strip_prefix(&PathBuf::from(&self.export_path))
            .unwrap_or(&path.path);

        Ok(mount_point.join(relative_path))
    }

    /// Create a VfsPath for local operations.
    fn create_local_path(&self, path: &VfsPath) -> VfsResult<VfsPath> {
        let local_path = self.to_local_path(path)?;
        Ok(VfsPath::local(local_path))
    }
}

impl Drop for NfsProvider {
    fn drop(&mut self) {
        self.unmount();
    }
}

impl VfsProvider for NfsProvider {
    fn name(&self) -> &'static str {
        "nfs"
    }

    fn connection_state(&self) -> ConnectionState {
        self.state
    }

    fn connect(&mut self, _options: ConnectOptions) -> VfsOperation<()> {
        // Check prerequisites
        if !Self::check_fuse_available() {
            return VfsOperation::error(VfsError::NotSupported(
                "fusermount not found. Please install FUSE.".to_string(),
            ));
        }

        if !Self::check_fuse_nfs_available() {
            return VfsOperation::error(VfsError::NotSupported(
                "fuse-nfs not found. Please install fuse-nfs (libnfs-utils).".to_string(),
            ));
        }

        // Create mount point
        let mount_point = match self.create_mount_point() {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        // Mount the NFS share
        match self.mount_with_fuse_nfs(&mount_point) {
            Ok(child) => {
                self.mount_point = Some(mount_point);
                self.local = Some(LocalFileSystem::new());
                self.state = ConnectionState::Connected;

                // Store the child process
                if let Ok(mut guard) = self.mount_process.lock() {
                    *guard = Some(child);
                }

                log::info!(
                    "NFS: Successfully mounted {}:{}",
                    self.host,
                    self.export_path
                );
                VfsOperation::ready(Ok(()))
            }
            Err(e) => {
                // Cleanup mount point on failure
                let _ = std::fs::remove_dir(&mount_point);
                self.state = ConnectionState::Failed;
                VfsOperation::ready(Err(e))
            }
        }
    }

    fn disconnect(&mut self) {
        self.unmount();
    }

    fn list_dir(&self, path: &VfsPath) -> VfsOperation<Vec<VfsEntry>> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        // Get entries from local filesystem
        let result = local.list_dir(&local_path);

        // We need to transform the paths back to NFS paths
        let export_path = self.export_path.clone();
        let mount_point = match &self.mount_point {
            Some(p) => p.clone(),
            None => return VfsOperation::error(VfsError::NotConnected),
        };
        let host = self.host.clone();

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let entries_result = result.recv();

            let transformed = entries_result.map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| {
                        // Convert local path back to NFS path
                        let relative = entry
                            .path
                            .path
                            .strip_prefix(&mount_point)
                            .unwrap_or(&entry.path.path);
                        let nfs_path_str = format!("{}{}", export_path, relative.display());
                        let nfs_path =
                            VfsPath::remote(VfsProtocol::Nfs, host.clone(), nfs_path_str);

                        VfsEntry::new(entry.name, nfs_path, entry.metadata)
                    })
                    .collect()
            });

            let _ = tx.send(transformed);
        });

        VfsOperation::new(rx)
    }

    fn create_dir(&self, path: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.create_dir(&local_path)
    }

    fn create_dir_all(&self, path: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.create_dir_all(&local_path)
    }

    fn exists(&self, path: &VfsPath) -> VfsOperation<bool> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.exists(&local_path)
    }

    fn metadata(&self, path: &VfsPath) -> VfsOperation<VfsMetadata> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.metadata(&local_path)
    }

    fn read_file(&self, path: &VfsPath) -> VfsOperation<Vec<u8>> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.read_file(&local_path)
    }

    fn write_file(&self, path: &VfsPath, data: &[u8]) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.write_file(&local_path, data)
    }

    fn delete(&self, path: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.delete(&local_path)
    }

    fn delete_recursive(&self, path: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(path) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.delete_recursive(&local_path)
    }

    fn rename(&self, from: &VfsPath, to: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let from_local = match self.create_local_path(from) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        let to_local = match self.create_local_path(to) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.rename(&from_local, &to_local)
    }

    fn copy(&self, from: &VfsPath, to: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let from_local = match self.create_local_path(from) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        let to_local = match self.create_local_path(to) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.copy(&from_local, &to_local)
    }

    fn download(&self, remote: &VfsPath, local_dest: &Path) -> VfsOperation<PathBuf> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(remote) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.download(&local_path, local_dest)
    }

    fn upload(&self, local_src: &Path, remote: &VfsPath) -> VfsOperation<()> {
        let local = match &self.local {
            Some(l) => l,
            None => return VfsOperation::error(VfsError::NotConnected),
        };

        let local_path = match self.create_local_path(remote) {
            Ok(p) => p,
            Err(e) => return VfsOperation::error(e),
        };

        local.upload(local_src, &local_path)
    }

    fn supported_auth_methods(&self) -> Vec<AuthMethod> {
        // NFS typically uses system authentication (Kerberos, etc.)
        vec![AuthMethod::None]
    }

    fn supports_recursive(&self) -> bool {
        true
    }

    fn home_dir(&self) -> Option<VfsPath> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nfs_provider_creation() {
        let provider = NfsProvider::new("server", "/export/path", None);
        assert_eq!(provider.name(), "nfs");
        assert_eq!(provider.connection_state(), ConnectionState::Disconnected);
    }

    #[test]
    fn test_mount_base_dir() {
        let base = NfsProvider::mount_base_dir();
        assert!(base.ends_with("termide/mounts") || base.to_string_lossy().contains("termide"));
    }
}
