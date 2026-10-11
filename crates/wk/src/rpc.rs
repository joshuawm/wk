//! JSON-RPC 2.0 over NDJSON, shared by the `wk` client and `wkd`.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::ipc::IpcStream;

/// Bound one NDJSON request before it reaches the daemon state machine.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Error codes carried in `error.data.code` (§8 of the spec).
pub mod codes {
    pub const UNKNOWN_CONTEXT: &str = "unknown-context";
    pub const CONTEXT_CLOSED: &str = "context-closed";
    pub const NO_PENDING_REQUEST: &str = "no-pending-request";
    pub const ALREADY_ANSWERED: &str = "already-answered";
    pub const INVALID_ANSWER: &str = "invalid-answer";
    pub const DEPTH_EXCEEDED: &str = "depth-exceeded";
    pub const PROVIDER_UNAVAILABLE: &str = "provider-unavailable";
    pub const REJECTED: &str = "rejected";
    pub const INVALID_MODE: &str = "invalid-mode";
}

pub fn request(id: u64, method: &str, params: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

pub fn response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub fn error(id: Value, message: &str, code: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message, "data": { "code": code } },
    })
    .to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

/// One framed message read from the socket.
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: Value, value: Value },
    /// The read timed out; the connection is still alive.
    TimedOut,
    Closed,
}

pub fn read_message<R: BufRead>(reader: &mut R) -> Incoming {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Incoming::Closed,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            ) =>
        {
            return Incoming::TimedOut
        }
        Err(_) => return Incoming::Closed,
        Ok(_) => {}
    }
    if line.len() > MAX_MESSAGE_BYTES {
        return Incoming::Closed;
    }
    let value: Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(_) => return Incoming::Closed,
    };
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        if value.get("id").is_some() {
            Incoming::Request {
                id: value["id"].clone(),
                method: method.to_owned(),
                params,
            }
        } else {
            Incoming::Notification {
                method: method.to_owned(),
                params,
            }
        }
    } else if value.get("id").is_some() {
        Incoming::Response {
            id: value["id"].clone(),
            value,
        }
    } else {
        Incoming::Closed
    }
}

pub fn write_line<W: Write>(stream: &mut W, line: &str) -> bool {
    if stream.write_all(line.as_bytes()).is_err() || stream.write_all(b"\n").is_err() {
        return false;
    }
    let _ = stream.flush();
    true
}

pub struct RpcError {
    pub message: String,
    pub code: String,
}

/// Parse the JSON-RPC result or error out of a raw response value.
pub fn split_response(value: Value) -> Result<Value, RpcError> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned();
        let code = error
            .get("data")
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("internal")
            .to_owned();
        return Err(RpcError { message, code });
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// Blocking single-shot client call on a fresh connection: send, then read
/// until the response with the matching id arrives. Server notifications seen
/// on the way are returned alongside.
pub fn call(path: &std::path::Path, method: &str, params: Value) -> anyhow::Result<(Value, Vec<Value>)> {
    let mut stream = IpcStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    write_line(&mut stream, &request(1, method, params));
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut notifications = Vec::new();
    loop {
        match read_message(&mut reader) {
            Incoming::Closed => anyhow::bail!("daemon closed the connection"),
            Incoming::TimedOut => continue,
            Incoming::Notification { method, params } => {
                notifications.push(json!({ "method": method, "params": params }));
            }
            Incoming::Request { .. } => continue,
            Incoming::Response { value, .. } => return Ok((value, notifications)),
        }
    }
}

/// A long-lived client connection for waiting RPCs: the response to the
/// initial request only arrives at the next stop point (§6.2). The socket read
/// timeout doubles as the poll tick for the signal flag, so it stays short.
pub struct Waiter {
    stream: IpcStream,
    reader: BufReader<IpcStream>,
}

impl Waiter {
    pub fn connect(path: &std::path::Path) -> anyhow::Result<Self> {
        let stream = IpcStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        let clone = stream.try_clone()?;
        Ok(Self {
            stream,
            reader: BufReader::new(clone),
        })
    }

    pub fn send(&mut self, id: u64, method: &str, params: Value) -> anyhow::Result<()> {
        write_line(&mut self.stream, &request(id, method, params));
        Ok(())
    }

    /// Read until the response for `id` arrives, a `waiter/superseded`
    /// notification arrives, `deadline` passes, or `signaled` flips true. The
    /// 250ms socket timeout makes the wait interruptible so the caller can
    /// poll the signal flag and the deadline.
    pub fn wait(
        &mut self,
        id: u64,
        deadline: Option<Instant>,
        signaled: &AtomicBool,
        superseded: &mut bool,
    ) -> Result<Result<Value, RpcError>, WaitEnd> {
        loop {
            if signaled.load(Ordering::SeqCst) {
                return Err(WaitEnd::Signaled);
            }
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Err(WaitEnd::Timeout);
                }
            }
            match read_message(&mut self.reader) {
                Incoming::TimedOut => continue,
                Incoming::Closed => return Err(WaitEnd::Closed),
                Incoming::Notification { method, .. } if method == "waiter/superseded" => {
                    *superseded = true;
                    return Err(WaitEnd::Superseded);
                }
                Incoming::Notification { .. } => continue,
                Incoming::Request { .. } => continue,
                Incoming::Response { id: got, value } if got.as_u64() == Some(id) => {
                    return Ok(split_response(value));
                }
                Incoming::Response { .. } => continue,
            }
        }
    }
}

pub enum WaitEnd {
    Timeout,
    Superseded,
    Signaled,
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn rejects_oversized_ndjson_messages() {
        let mut input = Cursor::new(format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"x\",\"params\":\"{}\"}}\n",
            "x".repeat(MAX_MESSAGE_BYTES)
        ));
        assert!(matches!(read_message(&mut input), Incoming::Closed));
    }

    #[test]
    fn accepts_messages_at_the_control_plane_limit() {
        let prefix = "{\"jsonrpc\":\"2.0\",\"method\":\"x\",\"params\":\"";
        let suffix = "\"}\n";
        let payload_len = MAX_MESSAGE_BYTES - prefix.len() - suffix.len();
        let mut input = Cursor::new(format!("{prefix}{}{suffix}", "x".repeat(payload_len)));
        assert!(matches!(
            read_message(&mut input),
            Incoming::Notification { .. }
        ));
    }
}
