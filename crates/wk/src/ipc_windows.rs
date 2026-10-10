//! Windows backend: an overlapped, byte-mode duplex named pipe. Overlapped
//! I/O is required, not an optimization: the daemon blocks on a connection's
//! read (waiting for the next request) while another thread writes a response
//! on the same instance, and synchronous pipe handles serialize all
//! operations per instance — the pending read would deadlock the write. This
//! is the concurrency the unix transport gets from duplicated fds. The
//! default pipe DACL (no SECURITY_ATTRIBUTES) only admits the daemon's user,
//! which is the same-user isolation the socket gets from its uid check.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::INFINITE;

const PIPE_BUFFER_SIZE: u32 = 4096;
const CONNECT_BUSY_RETRY: Duration = Duration::from_millis(500);
const CONNECT_BUSY_DEADLINE: Duration = Duration::from_secs(2);

pub struct IpcListener {
    /// The pipe name kept wide so each accept can create the next instance.
    name: Vec<u16>,
    handle: windows_sys::Win32::Foundation::HANDLE,
}

pub struct IpcStream {
    file: std::fs::File,
    /// Client-side read timeout (server-side accepted streams wait forever).
    /// Duplicated handles share the pipe but not this field, so `try_clone`
    /// copies it — the same visibility a dup'd fd gets from SO_RCVTIMEO.
    timeout: Mutex<Option<Duration>>,
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// One listening pipe instance. `first` asks for FILE_FLAG_FIRST_PIPE_INSTANCE
/// so a second simultaneous daemon bind fails instead of silently serving a
/// second endpoint under the same name.
fn create_instance(name: &[u16], first: bool) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED
    };
    // SECURITY_ATTRIBUTES = null → default DACL from the daemon's token.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

impl IpcListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let name = wide(path);
        let handle = create_instance(&name, true)?;
        Ok(Self { name, handle })
    }

    pub fn accept(&mut self) -> io::Result<IpcStream> {
        let handle = self.handle;
        let mut overlapped = OVERLAPPED::default();
        let ok = unsafe { ConnectNamedPipe(handle, &mut overlapped) };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            match error {
                ERROR_IO_PENDING => {
                    let mut transferred: u32 = 0;
                    if unsafe {
                        GetOverlappedResultEx(handle, &overlapped, &mut transferred, INFINITE, 0)
                    } == 0
                    {
                        let error = unsafe { GetLastError() };
                        unsafe { DisconnectNamedPipe(handle) };
                        return Err(io::Error::from_raw_os_error(error as i32));
                    }
                }
                // The client connected before the call; no wait needed.
                ERROR_PIPE_CONNECTED => {}
                other => {
                    // The client connected and vanished before any I/O, or the
                    // wait failed structurally; reset the instance and surface it.
                    unsafe { DisconnectNamedPipe(handle) };
                    return Err(io::Error::from_raw_os_error(other as i32));
                }
            }
        }
        let stream = IpcStream {
            file: unsafe { std::fs::File::from_raw_handle(handle as RawHandle) },
            timeout: Mutex::new(None),
        };
        // Serve the next connection from a fresh instance.
        self.handle = create_instance(&self.name, false)?;
        Ok(stream)
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl IpcStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let name = wide(path);
        let deadline = std::time::Instant::now() + CONNECT_BUSY_DEADLINE;
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0, // no sharing: the pipe server owns instances
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(IpcStream {
                    file: unsafe { std::fs::File::from_raw_handle(handle as RawHandle) },
                    timeout: Mutex::new(None),
                });
            }
            let error = unsafe { GetLastError() };
            // Every free instance may be between accepts during daemon startup;
            // wait for the next one instead of failing (ensure_daemon polls and
            // depends on this not being a hard error).
            if error == ERROR_PIPE_BUSY {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                unsafe { WaitNamedPipeW(name.as_ptr(), CONNECT_BUSY_RETRY.as_millis() as u32) };
                continue;
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.timeout.lock().expect("ipc timeout lock") = timeout;
        Ok(())
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(IpcStream {
            file: self.file.try_clone()?,
            timeout: Mutex::new(*self.timeout.lock().expect("ipc timeout lock")),
        })
    }

    /// The pipe's default DACL already restricts clients to the daemon's user,
    /// which is the isolation the unix socket gets from its uid check.
    pub fn peer_is_self(&self) -> bool {
        true
    }

    fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
    }
}

/// Peer disconnect reads as EOF, exactly like a closed unix socket.
fn read_error(error: u32) -> io::Result<usize> {
    if error == ERROR_BROKEN_PIPE {
        return Ok(0);
    }
    Err(io::Error::from_raw_os_error(error as i32))
}

fn wait_ms(timeout: Option<Duration>) -> u32 {
    match timeout {
        None => INFINITE,
        Some(duration) => duration.as_millis().min(u32::MAX as u128) as u32,
    }
}

impl io::Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let timeout = *self.timeout.lock().expect("ipc timeout lock");
        let handle = self.handle();
        let mut overlapped = OVERLAPPED::default();
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return read_error(error);
            }
        }
        let mut transferred: u32 = 0;
        if unsafe {
            GetOverlappedResultEx(handle, &overlapped, &mut transferred, wait_ms(timeout), 0)
        } != 0
        {
            // 0 bytes transferred is the peer's EOF.
            return Ok(transferred as usize);
        }
        let error = unsafe { GetLastError() };
        if error == WAIT_TIMEOUT {
            // Cancel the pending read; if data raced in during the cancel it is
            // drained and returned rather than lost.
            unsafe { CancelIoEx(handle, &overlapped) };
            let mut drained: u32 = 0;
            if unsafe { GetOverlappedResult(handle, &overlapped, &mut drained, 1) } != 0
                && drained > 0
            {
                return Ok(drained as usize);
            }
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        read_error(error)
    }
}

impl io::Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let handle = self.handle();
        let mut overlapped = OVERLAPPED::default();
        let ok = unsafe {
            WriteFile(
                handle,
                buf.as_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        let mut transferred: u32 = 0;
        if unsafe {
            GetOverlappedResultEx(handle, &overlapped, &mut transferred, INFINITE, 0)
        } != 0
        {
            return Ok(transferred as usize);
        }
        Err(io::Error::last_os_error())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Deliberately a no-op: FlushFileBuffers blocks until the peer drains
        // the pipe, while the unix flush this replaces is a no-op too.
        Ok(())
    }
}
