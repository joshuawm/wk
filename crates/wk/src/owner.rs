//! Owner detection (§7.1): the first non-shell ancestor of the calling
//! process, identified by pid + process start time to survive pid reuse.

use std::path::Path;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Owner {
    pub pid: i32,
    /// Platform token disambiguating pid reuse: seconds+micros of the process
    /// start on macOS, the raw jiffies-since-boot field on Linux.
    pub start: String,
}

const SHELL_AND_WRAPPER_NAMES: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "env", "timeout", "nohup", "wk",
];

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

fn info_of(pid: i32) -> Option<(i32, String, String)> {
    #[cfg(target_os = "macos")]
    return bsd_info(pid);
    #[cfg(not(target_os = "macos"))]
    return stat_fields(pid);
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
        if !SHELL_AND_WRAPPER_NAMES.contains(&name.as_str()) {
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
pub fn is_alive(owner: &Owner) -> bool {
    if unsafe { libc::kill(owner.pid, 0) } != 0 {
        return false;
    }
    start_token(owner.pid).is_some_and(|start| start == owner.start)
}

pub fn normalize_cwd(cwd: &Path) -> anyhow::Result<std::path::PathBuf> {
    std::fs::canonicalize(cwd)
        .map_err(|error| anyhow::anyhow!("cwd {} is not usable: {error}", cwd.display()))
}
