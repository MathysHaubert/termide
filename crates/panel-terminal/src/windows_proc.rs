//! Windows process queries for the terminal panel.
//!
//! Windows exposes no API for another process's working directory. It lives in
//! the process parameters block that the PEB points to, so it is read from the
//! shell's memory the way Process Explorer and WezTerm do it. The layouts
//! below are the leading fields of `PEB` and `RTL_USER_PROCESS_PARAMETERS`;
//! with pointer-sized fields they match the target's own bitness, so only a
//! shell of the same bitness as termide can be read. A 32-bit shell under a
//! 64-bit termide reports nothing and the caller falls back.

use std::ffi::c_void;
use std::path::PathBuf;

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, IsWow64Process, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ,
};

// The layouts below are only ever filled by the OS or by a raw memory read;
// most fields exist to put the one that is read at its offset.

#[repr(C)]
#[allow(dead_code)]
struct ProcessBasicInfo {
    exit_status: i32,
    peb_base_address: *mut c_void,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct PebPrefix {
    reserved1: [u8; 2],
    being_debugged: u8,
    reserved2: [u8; 1],
    reserved3: [*mut c_void; 2],
    ldr: *mut c_void,
    process_parameters: *mut c_void,
}

#[repr(C)]
#[allow(dead_code)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
#[allow(dead_code)]
struct ProcessParametersPrefix {
    maximum_length: u32,
    length: u32,
    flags: u32,
    debug_flags: u32,
    console_handle: *mut c_void,
    console_flags: u32,
    standard_input: *mut c_void,
    standard_output: *mut c_void,
    standard_error: *mut c_void,
    /// `CURDIR.DosPath`; the directory handle that follows is not needed.
    current_directory: UnicodeString,
}

/// Closes the process handle on every return path.
struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `OpenProcess`.
        unsafe { CloseHandle(self.0) };
    }
}

/// Read a `T` from the other process's memory at `address`.
///
/// # Safety
/// `T` must be plain data for which any bit pattern is valid.
unsafe fn read_remote<T>(process: HANDLE, address: *const c_void) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut read = 0usize;
    let ok = ReadProcessMemory(
        process,
        address,
        value.as_mut_ptr().cast(),
        std::mem::size_of::<T>(),
        &mut read,
    );
    (ok != 0 && read == std::mem::size_of::<T>()).then(|| value.assume_init())
}

/// Working directory of the shell started as `pid`.
///
/// Git for Windows' `bin\bash.exe` is a launcher that runs the real
/// `usr\bin\bash.exe` as its child and never changes directory itself, so the
/// directory is read from the innermost process down a chain of children with
/// the shell's own executable name. The same rule follows a `cmd` started
/// inside `cmd`, which is where the user is then typing.
pub fn shell_cwd(pid: u32) -> Option<PathBuf> {
    let pid = innermost_same_named(pid, &process_list());
    process_cwd(pid)
}

/// `(pid, parent pid, lowercased exe name)` of every running process.
fn process_list() -> Vec<(u32, u32, String)> {
    let mut list = Vec::new();
    // SAFETY: the snapshot handle is closed below; `entry` is a zeroed local
    // whose `dwSize` is set as the API requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == -1isize as HANDLE {
            return list;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                list.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    list
}

fn innermost_same_named(pid: u32, processes: &[(u32, u32, String)]) -> u32 {
    let Some((_, _, name)) = processes.iter().find(|(p, _, _)| *p == pid) else {
        return pid;
    };
    let mut current = pid;
    // Bounded: a reused pid could otherwise close a parent/child cycle.
    for _ in 0..8 {
        match processes
            .iter()
            .find(|(p, parent, n)| *parent == current && *p != current && n == name)
        {
            Some((child, _, _)) => current = *child,
            None => break,
        }
    }
    current
}

/// Working directory of process `pid`, or `None` when it cannot be read
/// (access denied, process gone, different bitness).
fn process_cwd(pid: u32) -> Option<PathBuf> {
    // SAFETY: every pointer handed to the Win32 calls below points at a local
    // of the size passed alongside it; remote addresses are only ever read
    // through `ReadProcessMemory`, which validates them.
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            FALSE,
            pid,
        );
        if handle.is_null() {
            return None;
        }
        let process = ProcessHandle(handle);

        let mut target_wow64 = FALSE;
        let mut own_wow64 = FALSE;
        if IsWow64Process(process.0, &mut target_wow64) == 0
            || IsWow64Process(GetCurrentProcess(), &mut own_wow64) == 0
            || target_wow64 != own_wow64
        {
            return None;
        }

        let mut info = std::mem::zeroed::<ProcessBasicInfo>();
        let status = NtQueryInformationProcess(
            process.0,
            ProcessBasicInformation,
            (&mut info as *mut ProcessBasicInfo).cast(),
            std::mem::size_of::<ProcessBasicInfo>() as u32,
            std::ptr::null_mut(),
        );
        if status < 0 || info.peb_base_address.is_null() {
            return None;
        }

        let peb: PebPrefix = read_remote(process.0, info.peb_base_address)?;
        let params: ProcessParametersPrefix = read_remote(process.0, peb.process_parameters)?;
        let dir = params.current_directory;
        let units = usize::from(dir.length) / 2;
        if dir.buffer.is_null() || units == 0 {
            return None;
        }

        let mut wide = vec![0u16; units];
        let mut read = 0usize;
        let ok = ReadProcessMemory(
            process.0,
            dir.buffer.cast(),
            wide.as_mut_ptr().cast(),
            units * 2,
            &mut read,
        );
        if ok == 0 || read != units * 2 {
            return None;
        }
        Some(PathBuf::from(clean_cwd(String::from_utf16_lossy(&wide))))
    }
}

/// The stored directory ends in a separator (`C:\work\`); keep it only on a
/// drive root, where `C:` alone would mean "the current directory of C:".
fn clean_cwd(mut dir: String) -> String {
    while dir.len() > 3 && dir.ends_with('\\') {
        dir.pop();
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_cwd_is_read_back() {
        let expected = std::env::current_dir().unwrap();
        assert_eq!(process_cwd(std::process::id()), Some(expected));
    }

    #[test]
    fn trailing_separator_is_dropped_except_on_a_root() {
        assert_eq!(clean_cwd(r"C:\work\".into()), r"C:\work");
        assert_eq!(clean_cwd(r"C:\".into()), r"C:\");
    }

    #[test]
    fn launcher_chain_resolves_to_the_innermost_shell() {
        let processes = vec![
            (10, 1, "bash.exe".to_string()),
            (11, 10, "bash.exe".to_string()),
            (12, 11, "python.exe".to_string()),
            (20, 1, "cmd.exe".to_string()),
        ];
        assert_eq!(innermost_same_named(10, &processes), 11);
        assert_eq!(innermost_same_named(20, &processes), 20);
        assert_eq!(innermost_same_named(99, &processes), 99);
    }
}
