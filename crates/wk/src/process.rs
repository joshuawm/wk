//! Daemon lifecycle helpers (§13): home directory, socket, lock, daemonize,
//! peer-uid check, context ids, and the minimal log (§12: ids and state
//! changes only, never prompt text).

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

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

fn dirs_homedir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn ensure_home() -> anyhow::Result<PathBuf> {
    let home = wk_home();
    std::fs::create_dir_all(&home)?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    Ok(home)
}

pub fn socket_path(home: &Path) -> PathBuf {
    home.join("wkd.sock")
}

pub fn lock_path(home: &Path) -> PathBuf {
    home.join("wkd.lock")
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("wkd.log")
}

/// Hold an exclusive flock for the daemon's lifetime (§13).
pub fn acquire_lock(home: &Path) -> anyhow::Result<File> {
    let file = File::create(lock_path(home))?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!("another wkd holds the lock at {}", lock_path(home).display());
    }
    Ok(file)
}

pub fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The socket only serves same-uid callers (§8): verify with getpeereid.
pub fn peer_uid_is_self(stream: &std::os::unix::net::UnixStream) -> bool {
    peer_uid(stream.as_raw_fd())
        .map(|uid| uid == unsafe { libc::getuid() })
        .unwrap_or(false)
}

pub fn peer_uid(fd: i32) -> std::io::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let mut uid: u32 = 0;
        let mut gid: u32 = 0;
        let ret = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux has no getpeereid; SO_PEERCRED carries the same (uid, gid, pid).
        let mut cred: libc::ucred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
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

fn read_random(buffer: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut file = File::open("/dev/urandom")?;
    file.read_exact(buffer)
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
