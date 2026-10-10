//! Daemon lifecycle helpers (§13): home directory, socket, lock, daemonize,
//! peer-uid check, context ids, and the minimal log (§12: ids and state
//! changes only, never prompt text).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub fn wk_home() -> PathBuf {
    if let Some(home) = std::env::var_os("WK_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".wk")
}

#[cfg(unix)]
pub fn wk_home() -> PathBuf {
    if let Some(home) = std::env::var_os("WK_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    let mut home = dirs_homedir();
    home.push(".wk");
    home
}

#[cfg(unix)]
fn dirs_homedir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn ensure_home() -> anyhow::Result<PathBuf> {
    let home = wk_home();
    std::fs::create_dir_all(&home)?;
    // Unix tightens the directory; on Windows the per-user profile directory
    // is already ACL-protected against other users by default.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(home)
}

#[cfg(unix)]
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("wkd.sock")
}

/// Windows serves `wk` over a named pipe instead of a socket file. Pipe names
/// live in a machine-wide namespace, so the home path is hashed into the name:
/// separate `WK_HOME`s get separate daemons (concurrent E2E runs), and two
/// users on one machine never collide. The spelling is normalized (lowercase,
/// backslash separators, no trailing slash) so every way of writing the same
/// path hashes the same on both the client and the daemon side.
#[cfg(windows)]
pub fn socket_path(home: &Path) -> PathBuf {
    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
    let normalized = home
        .to_string_lossy()
        .to_lowercase()
        .replace('/', "\\");
    let normalized = normalized.trim_end_matches('\\');
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let user = if user.is_empty() { "u".to_owned() } else { user };
    PathBuf::from(format!(r"\\.\pipe\wkd-{user}-{:016x}", fnv1a64(normalized.as_bytes())))
}

pub fn lock_path(home: &Path) -> PathBuf {
    home.join("wkd.lock")
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("wkd.log")
}

/// Hold an exclusive lock for the daemon's lifetime (§13).
#[cfg(unix)]
pub fn acquire_lock(home: &Path) -> anyhow::Result<File> {
    use std::os::fd::AsRawFd;
    let file = File::create(lock_path(home))?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!("another wkd holds the lock at {}", lock_path(home).display());
    }
    Ok(file)
}

/// No flock on Windows: the lock file is opened with sharing disabled and the
/// handle is held for the daemon's lifetime, so a second opener fails.
#[cfg(windows)]
pub fn acquire_lock(home: &Path) -> anyhow::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(0)
        .open(lock_path(home))
        .map_err(|error| {
            anyhow::anyhow!(
                "another wkd holds the lock at {} ({error})",
                lock_path(home).display()
            )
        })
}

/// §2: 6-character base32 context ids.
pub fn new_context_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = [0u8; 4];
    if read_random(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = (nanos as u32).to_le_bytes();
    }
    let mut value = u32::from_le_bytes(bytes) & 0x3fff_ffff; // 30 bits
    let mut id = String::with_capacity(6);
    for _ in 0..6 {
        id.push(ALPHABET[(value & 31) as usize] as char);
        value >>= 5;
    }
    id
}

#[cfg(unix)]
fn read_random(buffer: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut file = File::open("/dev/urandom")?;
    file.read_exact(buffer)
}

#[cfg(windows)]
fn read_random(buffer: &mut [u8]) -> std::io::Result<()> {
    getrandom::fill(buffer)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

/// §12: the daemon log records ids and state changes only.
pub fn log_line(home: &Path, message: &str) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(home))
    else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(file, "{timestamp} {message}");
}

/// §13: start the daemon detached — setsid plus a double fork, then exec
/// `wk daemon run` with stdio redirected to the log (stdin from /dev/null).
#[cfg(unix)]
pub fn spawn_detached_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let home = ensure_home()?;
    let log = File::create(log_path(&home))?;
    let null = File::open("/dev/null")?;

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
        }
        if pid > 0 {
            // Wait for the intermediate child so it cannot become a zombie.
            let mut status: libc::c_int = 0;
            libc::waitpid(pid, &mut status, 0);
            return Ok(());
        }
        // Intermediate child: new session, then fork again so the daemon can
        // never reacquire a controlling terminal.
        if libc::setsid() < 0 {
            libc::_exit(1);
        }
        let pid = libc::fork();
        if pid < 0 {
            libc::_exit(1);
        }
        if pid > 0 {
            libc::_exit(0);
        }
        // Grandchild: exec the daemon. Command::exec applies the configured
        // stdio redirections across the exec boundary and never returns on
        // success.
        use std::os::unix::process::CommandExt;
        let _ = std::process::Command::new(exe)
            .arg("daemon")
            .arg("run")
            .stdin(std::process::Stdio::from(null))
            .stdout(std::process::Stdio::from(log.try_clone()?))
            .stderr(std::process::Stdio::from(log))
            .exec();
        libc::_exit(127);
    }
}

/// §13 on Windows: spawn `wk daemon run` detached. There is no fork/exec, so
/// the spawn returns immediately and the detached child outlives this process.
/// Spawn via raw `CreateProcessW` with `bInheritHandles=FALSE`. The std
/// `Command` path must pass `bInheritHandles=TRUE` to deliver the log stdio,
/// which copies every inheritable handle of the caller into the daemon —
/// including a shell's command-substitution pipe, whose write end the daemon
/// would then hold open forever (the caller's `$(...)` never sees EOF). With
/// no inherited handles the daemon has no std streams at all; it opens
/// wkd.log itself for logging (§12) and inherits the environment block
/// (lpEnvironment=NULL), which is copied by the kernel, not via handles.
/// DETACHED_PROCESS and CREATE_NO_WINDOW are mutually exclusive console
/// flags; the new process group keeps this console's Ctrl+C away from the
/// daemon.
#[cfg(windows)]
pub fn spawn_detached_daemon() -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    let exe = std::env::current_exe()?;
    ensure_home()?;
    let mut cmd: Vec<u16> = std::ffi::OsStr::new(&format!("\"{}\" daemon run", exe.display()))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles = FALSE — see the doc comment
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        anyhow::bail!(
            "failed to spawn daemon: {}",
            std::io::Error::last_os_error()
        );
    }
    unsafe {
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    Ok(())
}
