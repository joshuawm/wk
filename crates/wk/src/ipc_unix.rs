//! Unix backend: a filesystem unix socket, the original transport.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

pub struct IpcListener {
    inner: UnixListener,
}

pub struct IpcStream {
    inner: UnixStream,
}

impl IpcListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: UnixListener::bind(path)?,
        })
    }

    /// `&mut self` mirrors the Windows backend, where each accept re-arms the
    /// listener; the unix listener itself needs no mutability.
    pub fn accept(&mut self) -> io::Result<IpcStream> {
        self.inner.accept().map(|(stream, _)| IpcStream { inner: stream })
    }
}

impl IpcStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        UnixStream::connect(path).map(|inner| IpcStream { inner })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        self.inner.try_clone().map(|inner| IpcStream { inner })
    }

    /// The socket only serves same-uid callers (§8): verify with getpeereid.
    pub fn peer_is_self(&self) -> bool {
        peer_uid(self.inner.as_raw_fd())
            .map(|uid| uid == unsafe { libc::getuid() })
            .unwrap_or(false)
    }
}

impl io::Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.inner, buf)
    }
}

impl io::Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.inner, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // UnixStream::flush is a no-op; keep that semantic on every backend.
        Ok(())
    }
}

fn peer_uid(fd: i32) -> io::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let mut uid: u32 = 0;
        let mut gid: u32 = 0;
        let ret = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
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
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
}
