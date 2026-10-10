//! The daemon IPC seam: line-delimited JSON-RPC rides on a local byte stream.
//! Unix uses a filesystem unix socket; Windows uses a named pipe with the
//! same user-scoped isolation (the pipe's default DACL only admits the
//! creating user, replacing the socket's uid check). The API mirrors the
//! `UnixStream`/`UnixListener` surface the RPC layer already needs.

#[cfg(unix)]
#[path = "ipc_unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "ipc_windows.rs"]
mod windows;

#[cfg(unix)]
pub use unix::{IpcListener, IpcStream};
#[cfg(windows)]
pub use windows::{IpcListener, IpcStream};

use std::path::Path;

/// Whether something is serving on `path` right now: a successful connect
/// means a live daemon. Never probe a pipe name with `Path::exists` — it does
/// not see the pipe namespace. Transient pipe-busy races are retried inside
/// `connect`, so a lone `Ok`/`Err` here is definitive.
pub fn endpoint_is_live(path: &Path) -> bool {
    IpcStream::connect(path).is_ok()
}

/// Undo a stale endpoint left behind by a crashed daemon. Unix needs the
/// filesystem entry removed; the pipe namespace is owned by the kernel and
/// disappears with the process, so this is a no-op on Windows.
pub fn remove_stale_endpoint(path: &Path) {
    let _ = std::fs::remove_file(path);
}
