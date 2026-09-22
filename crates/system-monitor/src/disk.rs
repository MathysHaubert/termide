//! Disk space queries and device-name resolution.

use crate::DiskSpaceInfo;
#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::path::Path;

/// Resolve dm-X device to physical partition.
/// e.g., /dev/dm-0 -> /dev/nvme0n1p2
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn resolve_dm_device(device: &str) -> Option<String> {
    // Extract dm number (e.g., "dm-0" from "/dev/dm-0")
    let dm_name = device.strip_prefix("/dev/")?;
    if !dm_name.starts_with("dm-") {
        return None;
    }

    // Read /sys/block/dm-X/slaves/ to find physical partition
    let slaves_path = format!("/sys/block/{}/slaves", dm_name);
    let slaves_dir = std::fs::read_dir(&slaves_path).ok()?;

    // Get first slave (physical partition)
    for entry in slaves_dir.flatten() {
        if let Ok(name) = entry.file_name().into_string() {
            return Some(format!("/dev/{}", name));
        }
    }

    None
}

/// One entry of the system mount table: a backing device and where it is
/// mounted.
#[cfg(unix)]
pub(crate) struct MountEntry {
    pub device: String,
    pub mount_point: String,
}

/// Read the system mount table.
///
/// Linux exposes it as text in `/proc/mounts`; macOS has no `/proc` and
/// answers the same question through `getmntinfo(3)`. Everything downstream —
/// device resolution for a path and the all-devices listing — works off this
/// one list, so the platform difference is confined here.
#[cfg(target_os = "linux")]
fn read_mounts() -> Vec<MountEntry> {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };

    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some(MountEntry {
                device: parts.next()?.to_string(),
                mount_point: parts.next()?.to_string(),
            })
        })
        .collect()
}

/// `getmntinfo` returns a pointer into per-process static storage that the
/// next call overwrites, so callers are serialized and copy out under the lock.
#[cfg(target_os = "macos")]
static MNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
fn read_mounts() -> Vec<MountEntry> {
    let _guard = MNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut buf: *mut libc::statfs = std::ptr::null_mut();

    // SAFETY: `getmntinfo` points `buf` at a kernel-filled array of `count`
    // `statfs` structs and returns that count (0 on failure). The storage is
    // owned by libc and must not be freed; it stays valid until the next call,
    // which `MNT_LOCK` prevents while the slice below is alive.
    let count = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if count <= 0 || buf.is_null() {
        return Vec::new();
    }

    // SAFETY: `count` entries were just reported as written to `buf`.
    let entries = unsafe { std::slice::from_raw_parts(buf, count as usize) };

    entries
        .iter()
        .filter_map(|fs| {
            Some(MountEntry {
                device: c_chars_to_string(&fs.f_mntfromname)?,
                mount_point: c_chars_to_string(&fs.f_mntonname)?,
            })
        })
        .collect()
}

/// No known way to enumerate mounts on this platform; callers degrade to an
/// empty table rather than reporting wrong devices.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn read_mounts() -> Vec<MountEntry> {
    Vec::new()
}

/// Decode a fixed-size, NUL-padded C string field.
#[cfg(target_os = "macos")]
fn c_chars_to_string(field: &[libc::c_char]) -> Option<String> {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    if bytes.is_empty() {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Get the device backing a given path, by longest matching mount point.
#[cfg(unix)]
pub(crate) fn get_device_for_path(path: &Path) -> Option<String> {
    let mut best_match: Option<(String, usize)> = None;

    // Canonicalized once: it does not change across mount entries, and the
    // syscall used to run per entry.
    let canonical_path = path.canonicalize().ok()?;

    for entry in read_mounts() {
        // Check if this mount point is a prefix of our path
        if let Ok(canonical_mount) = Path::new(&entry.mount_point).canonicalize() {
            if canonical_path.starts_with(&canonical_mount) {
                let mount_len = canonical_mount.as_os_str().len();
                // Keep track of the longest matching mount point
                if best_match.as_ref().is_none_or(|b| mount_len > b.1) {
                    best_match = Some((entry.device.clone(), mount_len));
                }
            }
        }
    }

    best_match.and_then(|(device, _)| {
        // First try to resolve symlink (e.g., /dev/disk/by-uuid/... -> /dev/nvme0n1p2)
        let resolved = Path::new(&device)
            .canonicalize()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| device.clone());

        // If it's a dm device, resolve to physical partition
        if resolved.contains("/dm-") {
            resolve_dm_device(&resolved).or(Some(resolved))
        } else {
            Some(resolved)
        }
    })
}

/// Query a path with `statvfs` and return `(available, total)` in bytes.
///
/// `f_blocks` and `f_bavail` count *fundamental* blocks, so they must be
/// scaled by `f_frsize`. `f_bsize` is the optimal I/O size and can be much
/// larger: on macOS/APFS it is 1 MiB while `f_frsize` is 4 KiB, so scaling by
/// `f_bsize` inflates every figure 256x (a 1.8 TB volume reports 464 TB).
/// Some platforms leave `f_frsize` unset, hence the `f_bsize` fallback.
#[cfg(unix)]
pub(crate) fn statvfs_bytes(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path_cstr = CString::new(path.as_os_str().as_bytes()).ok()?;

    // SAFETY: `statvfs` is POSIX; it fills the zero-initialized struct and
    // returns 0 on success, at which point the fields are valid. `path_cstr`
    // is a valid NUL-terminated path built above.
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path_cstr.as_ptr(), &mut stat) != 0 {
            return None;
        }
        stat
    };

    let block_size = {
        let frsize = stat.f_frsize as u64;
        if frsize == 0 {
            stat.f_bsize as u64
        } else {
            frsize
        }
    };
    if block_size == 0 {
        return None;
    }

    let available = (stat.f_bavail as u64).checked_mul(block_size)?;
    let total = (stat.f_blocks as u64).checked_mul(block_size)?;

    Some((available, total))
}

/// Get disk space information for a given path.
///
/// Returns `DiskSpaceInfo` with device name, available and total space.
#[cfg(unix)]
pub fn get_disk_space_info(path: &Path) -> Option<DiskSpaceInfo> {
    // Get device name for this path
    let device = get_device_for_path(path);

    let (available, total) = statvfs_bytes(path)?;

    Some(DiskSpaceInfo {
        device,
        available,
        total,
    })
}

/// Get disk space information for all real mounted devices.
///
/// Reads the system mount table, filters for real devices (`/dev/`),
/// deduplicates by device path, and calls `statvfs` for each.
#[cfg(unix)]
pub fn get_all_disk_space_info() -> Vec<DiskSpaceInfo> {
    let mut seen_devices: HashMap<String, String> = HashMap::new(); // device -> mount_point

    for entry in read_mounts() {
        let device = entry.device.as_str();

        // Only real devices. Filters out Linux's proc/sysfs/cgroup entries and
        // macOS's devfs, `map auto_home` and network shares alike.
        if !device.starts_with("/dev/") {
            continue;
        }

        // Skip pseudo-devices
        if device.starts_with("/dev/loop") || device.starts_with("/dev/ram") {
            continue;
        }

        // Resolve device symlinks/dm
        let resolved = Path::new(device)
            .canonicalize()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| device.to_string());

        let resolved = if resolved.contains("/dm-") {
            resolve_dm_device(&resolved).unwrap_or(resolved)
        } else {
            resolved
        };

        // Keep only first mount point per device (usually the most relevant)
        seen_devices
            .entry(resolved)
            .or_insert_with(|| entry.mount_point.clone());
    }

    let mut result = Vec::new();
    for (device, mount_point) in &seen_devices {
        if let Some(info) = get_disk_space_info(Path::new(mount_point)) {
            result.push(DiskSpaceInfo {
                device: Some(device.clone()),
                ..info
            });
        }
    }

    // Sort by device name for consistent ordering
    result.sort_by(|a, b| a.device.cmp(&b.device));
    result
}

#[cfg(windows)]
pub fn get_disk_space_info(path: &std::path::Path) -> Option<DiskSpaceInfo> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let root = path.components().next()?;
    let root_str = format!("{}\\", root.as_os_str().to_string_lossy());

    let wide_path: Vec<u16> = OsStr::new(&root_str)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut free_bytes_available: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut _total_free_bytes: u64 = 0;

    let success = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut free_bytes_available,
            &mut total_bytes,
            &mut _total_free_bytes,
        )
    };

    if success != 0 {
        Some(DiskSpaceInfo {
            device: Some(root_str.trim_end_matches('\\').to_string()),
            available: free_bytes_available,
            total: total_bytes,
        })
    } else {
        None
    }
}

#[cfg(windows)]
pub fn get_all_disk_space_info() -> Vec<DiskSpaceInfo> {
    // Query drives A-Z using GetDiskFreeSpaceExW
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let mut result = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let wide_path: Vec<u16> = OsStr::new(&drive)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut free_bytes_available: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut _total_free_bytes: u64 = 0;

        let success = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide_path.as_ptr(),
                &mut free_bytes_available,
                &mut total_bytes,
                &mut _total_free_bytes,
            )
        };

        if success != 0 && total_bytes > 0 {
            result.push(DiskSpaceInfo {
                device: Some(format!("{}:", letter as char)),
                available: free_bytes_available,
                total: total_bytes,
            });
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_get_all_disk_space_info() {
        let disks = get_all_disk_space_info();
        // Should find at least one real disk on any Linux system
        assert!(!disks.is_empty());
        for disk in &disks {
            assert!(disk.device.is_some());
            assert!(disk.total > 0);
            // No virtual filesystems
            let dev = disk.device.as_ref().unwrap();
            assert!(dev.starts_with("/dev/"));
            assert!(!dev.starts_with("/dev/loop"));
        }
    }

    /// `statvfs` block counts are fundamental-block counts. Cross-check against
    /// a raw `statvfs` call, and assert the figure stays physically plausible:
    /// on macOS scaling by `f_bsize` (1 MiB) instead of `f_frsize` (4 KiB)
    /// inflated a 1.8 TB volume to ~464 TB.
    #[cfg(unix)]
    #[test]
    fn test_statvfs_bytes_matches_statvfs_and_is_plausible() {
        let path = std::path::Path::new("/");
        let (available, total) = statvfs_bytes(path).expect("statvfs on /");

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let c_path = std::ffi::CString::new("/").unwrap();
        assert_eq!(unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) }, 0);
        let block = {
            let frsize = stat.f_frsize as u64;
            if frsize == 0 {
                stat.f_bsize as u64
            } else {
                frsize
            }
        };

        assert_eq!(total, stat.f_blocks as u64 * block);
        assert_eq!(available, stat.f_bavail as u64 * block);
        assert!(
            total < 64 * 1024u64.pow(4),
            "total {total} bytes is beyond any plausible single volume"
        );
        assert!(
            available > 0 && available < total,
            "available {available} outside 0..{total}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_statvfs_bytes_missing_path() {
        assert!(statvfs_bytes(std::path::Path::new("/nonexistent-volume-path")).is_none());
    }
}
