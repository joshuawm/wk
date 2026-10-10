//! Owner detection (§7.1): the first non-shell ancestor of the calling
//! process, identified by pid + process start time to survive pid reuse.

use std::path::Path;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Owner {
    pub pid: i32,
    /// Platform token disambiguating pid reuse: seconds+micros of the process
    /// start on macOS, the raw jiffies-since-boot field on Linux, the raw
    /// FILETIME creation stamp on Windows.
    pub start: String,
}

#[cfg(not(windows))]
const SHELL_AND_WRAPPER_NAMES: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "env", "timeout", "nohup", "wk",
];

/// Windows adds the console shells. The terminal emulator itself (mintty,
/// WindowsTerminal) deliberately stays off the list — it becomes the owner,
/// the same "terminal session owns the context" outcome as unix.
#[cfg(windows)]
const SHELL_AND_WRAPPER_NAMES: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "env", "timeout", "nohup", "wk",
    "cmd", "pwsh", "powershell",
];

/// Wrapper matching tolerates Windows executable suffixes and casing
/// (`bash.exe`, `WK.EXE`); unix process names match as-is.
fn is_wrapper(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    SHELL_AND_WRAPPER_NAMES.contains(&stem)
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: i32) -> Option<(i32, String, String)> {
    // (ppid, process name, start token) in one proc_pidinfo call.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if ret != size {
        return None;
    }
    let comm = c_str(&info.pbi_comm);
    let start = format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec);
    Some((info.pbi_ppid as i32, comm, start))
}

#[cfg(target_os = "macos")]
fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(not(target_os = "macos"))]
#[cfg(unix)]
fn stat_fields(pid: i32) -> Option<(i32, String, String)> {
    // (ppid, process name, start token) from /proc/<pid>/stat. The comm field
    // is parenthesized and may contain spaces, so split after the last ')'.
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = text.rfind(')')?;
    let comm = text[text.find('(')? + 1..close].to_owned();
    // Fields after ')': field 3 (state) first, then 4, 5, ... Field 22 is
    // starttime in jiffies since boot — unique enough to defeat pid reuse.
    let fields: Vec<&str> = text[close + 2..].split(' ').collect();
    let ppid: i32 = fields.first()?.parse().ok()?;
    let start = fields.get(22 - 3)?.to_string();
    Some((ppid, comm, start))
}

#[cfg(windows)]
fn info_of(pid: i32) -> Option<(i32, String, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found: Option<(i32, String)> = None;
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid as u32 {
                    let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                    found = Some((entry.th32ParentProcessID as i32, name));
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    let (ppid, name) = found?;
    let start = process_start_token(pid)?;
    Some((ppid, name, start))
}

#[cfg(windows)]
fn process_start_token(pid: i32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    // 100ns ticks since 1601 — a stable per-process identity, unique enough
    // to catch pid reuse.
    let stamp = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
    Some(stamp.to_string())
}

#[cfg(target_os = "macos")]
fn info_of(pid: i32) -> Option<(i32, String, String)> {
    bsd_info(pid)
}

#[cfg(not(target_os = "macos"))]
#[cfg(unix)]
fn info_of(pid: i32) -> Option<(i32, String, String)> {
    stat_fields(pid)
}

fn parent_of(pid: i32) -> Option<i32> {
    info_of(pid).map(|(ppid, _, _)| ppid)
}

fn start_token(pid: i32) -> Option<String> {
    info_of(pid).map(|(_, _, start)| start)
}

/// Walk up from `pid` and pick the owner (§7.1): the first ancestor whose name
/// is not a shell or wrapper. When every ancestor is a shell (running directly
/// in a terminal), the topmost shell is the owner.
pub fn find_owner(pid: i32) -> Option<Owner> {
    let mut current = pid;
    let mut top: Option<Owner> = None;
    for _ in 0..32 {
        let (_, name, start) = info_of(current)?;
        let owner = Owner { pid: current, start };
        if !is_wrapper(&name) {
            return Some(owner);
        }
        top = Some(owner);
        let parent = parent_of(current)?;
        if parent == 0 || parent == current {
            break;
        }
        current = parent;
    }
    top
}

/// Explicit `--owner <pid>` (§7.1): trust the caller's pid, take our own
/// start token so pid reuse still gets caught.
pub fn from_pid(pid: i32) -> Option<Owner> {
    start_token(pid).map(|start| Owner { pid, start })
}

/// Whether the process is still the one this `Owner` recorded (§7.2).
#[cfg(unix)]
pub fn is_alive(owner: &Owner) -> bool {
    if unsafe { libc::kill(owner.pid, 0) } != 0 {
        return false;
    }
    start_token(owner.pid).is_some_and(|start| start == owner.start)
}

/// Windows twin of the waku-daemon liveness check (OpenProcess +
/// GetExitCodeProcess), plus the start-token equality so a reused pid does
/// not keep a dead owner alive.
#[cfg(windows)]
pub fn is_alive(owner: &Owner) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, owner.pid as u32) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe { CloseHandle(handle) };
    ok != 0 && code == STILL_ACTIVE as u32
        && start_token(owner.pid).is_some_and(|start| start == owner.start)
}

pub fn normalize_cwd(cwd: &Path) -> anyhow::Result<std::path::PathBuf> {
    // dunce keeps Windows paths in their normal form instead of the `\\?\`
    // verbatim prefix std would leak into stored rows and provider cwds.
    #[cfg(windows)]
    let canonical = dunce::canonicalize(cwd);
    #[cfg(not(windows))]
    let canonical = std::fs::canonicalize(cwd);
    canonical.map_err(|error| anyhow::anyhow!("cwd {} is not usable: {error}", cwd.display()))
}
