//! `wkd` — the resident daemon (§2, §8). Owns all driver state behind a unix
//! socket; the `wk` CLI is a stateless client.
//!
//! Borrow discipline: every handler stages what it needs from a context in a
//! scoped block, then touches the store, then mutates the context, then calls
//! into the driver — the store and `Ctx` never borrow each other.

use std::collections::HashMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use waku_core::driver::{DriverHandle, DriverStartOptions, event_channel, start_local};
use waku_core::model::{
    DriverEvent, PermissionOption, ProviderKind, ProviderResumeCursor, RuntimeMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::process::{ensure_home, log_line, socket_path};
use crate::rpc;
use crate::store::{self, Store};

pub const AUTO_ANSWER: &str =
    "The user is unavailable. Choose the most reasonable option, state your assumption, and continue.";

const DRIVER_IDLE_KEEP: i64 = 600;
const DORMANT_AFTER: i64 = 1800;
const OWNER_GRACE: Duration = Duration::from_secs(60);
const OWNER_POLL: Duration = Duration::from_secs(5);

type Shared = Arc<Mutex<DaemonState>>;
type SharedStream = Arc<Mutex<UnixStream>>;

#[derive(Clone)]
enum Payload {
    Question(Vec<UserInputQuestion>),
    Permission {
        title: String,
        detail: String,
        options: Vec<PermissionOption>,
    },
}

impl Payload {
    fn to_json(&self) -> Value {
        match self {
            Payload::Question(questions) => serde_json::to_value(questions).unwrap_or(Value::Null),
            Payload::Permission {
                title,
                detail,
                options,
            } => json!({
                "title": title,
                "detail": detail,
                "options": serde_json::to_value(options).unwrap_or(Value::Null),
            }),
        }
    }
}

struct PendingReq {
    id: String,
    task_id: i64,
    kind: &'static str,
    driver_request_id: String,
    payload: Payload,
    created_at: i64,
}

struct Task {
    id: i64,
    state: String,
    result: String,
    error: Option<String>,
    dormant: bool,
    finished_at: Option<i64>,
}

impl Task {
    fn terminal(&self) -> bool {
        matches!(self.state.as_str(), "completed" | "failed" | "canceled")
    }
}

struct Waiter {
    stream: SharedStream,
    id: Value,
}

struct Ctx {
    id: String,
    provider: String,
    cwd: PathBuf,
    model: Option<String>,
    reasoning_effort: Option<String>,
    mode: RuntimeMode,
    on_question: String,
    cursor: Option<ProviderResumeCursor>,
    owner: Option<crate::owner::Owner>,
    depth: i64,
    closed: bool,
    created_at: i64,

    connected: bool,
    handle: Option<DriverHandle>,
    _wake_rx: Option<smol::channel::Receiver<()>>,
    task: Option<Task>,
    pending: Vec<PendingReq>,
    next_question: u32,
    next_permission: u32,
    waiter: Option<Waiter>,
    steering: bool,
    pending_prompt: Option<String>,
    owner_dead_since: Option<Instant>,
}

struct DaemonState {
    store: Store,
    ctxs: HashMap<String, Ctx>,
    max_depth: i64,
    shutting_down: bool,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn parse_provider(name: &str) -> Option<ProviderKind> {
    match name.to_ascii_lowercase().as_str() {
        "amp" => Some(ProviderKind::Amp),
        "claude" => Some(ProviderKind::Claude),
        "codex" => Some(ProviderKind::Codex),
        "cursor" => Some(ProviderKind::Cursor),
        "deepseek" => Some(ProviderKind::DeepSeek),
        "fx" => Some(ProviderKind::Fx),
        "opencode" | "opencode2" => Some(ProviderKind::OpenCode),
        "grok" => Some(ProviderKind::Grok),
        "kimi" => Some(ProviderKind::Kimi),
        "ohmypi" => Some(ProviderKind::OhMyPi),
        "pi" => Some(ProviderKind::Pi),
        _ => None,
    }
}

pub fn parse_mode(name: &str) -> Option<RuntimeMode> {
    match name {
        "ask" | "plan" => Some(RuntimeMode::Ask),
        "auto-accept-edits" | "autoAcceptEdits" => Some(RuntimeMode::AutoAcceptEdits),
        "auto" => Some(RuntimeMode::Auto),
        "full-access" | "fullAccess" => Some(RuntimeMode::FullAccess),
        _ => None,
    }
}

pub fn mode_name(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Ask => "ask",
        RuntimeMode::AutoAcceptEdits => "auto-accept-edits",
        RuntimeMode::Auto => "auto",
        RuntimeMode::FullAccess => "full-access",
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static STATE: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();

// ---------------------------------------------------------------------------
// entry point

pub fn run() -> anyhow::Result<()> {
    let home = ensure_home()?;
    let lock = crate::process::acquire_lock(&home)?;
    let socket = socket_path(&home);

    // A connectable socket means a live daemon; a stale file is ours to remove.
    if socket.exists() && UnixStream::connect(&socket).is_ok() {
        anyhow::bail!("another wkd is already serving {}", socket.display());
    }
    let _ = std::fs::remove_file(&socket);

    let store = Store::open(&home.join("state.db"))?;
    let mut state = DaemonState {
        store,
        ctxs: HashMap::new(),
        max_depth: std::env::var("WK_MAX_DEPTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3),
        shutting_down: false,
    };
    recover_from_restart(&mut state);

    let listener = crate::process::bind_socket(&socket)?;
    log_line(&home, &format!("wkd listening on {}", socket.display()));

    // Capture the login-shell environment off the accept path; drivers spawn
    // through `command_env::command`, which falls back to the inherited env
    // until this lands.
    std::thread::Builder::new()
        .name("wk-shell-env".into())
        .spawn(waku_core::command_env::refresh_from_default_shell)
        .ok();

    let shared: Shared = Arc::new(Mutex::new(state));
    let _ = STATE.set(shared.clone());
    let janitor_shared = shared.clone();
    std::thread::Builder::new()
        .name("wk-janitor".into())
        .spawn(move || janitor_loop(janitor_shared))
        .ok();

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if !crate::process::peer_uid_is_self(&stream) {
                    log_line(&home, "rejected connection from a different uid");
                    continue;
                }
                let shared = shared.clone();
                let home = home.clone();
                std::thread::Builder::new()
                    .name("wk-conn".into())
                    .spawn(move || {
                        let _ = serve_connection(shared, stream, home);
                    })
                    .ok();
            }
            Err(error) => log_line(&home, &format!("accept failed: {error}")),
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
    }
    drop(lock);
    Ok(())
}

pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
    if let Some(home) = ensure_home().ok() {
        let _ = UnixStream::connect(socket_path(&home));
    }
}

/// §7.5: on daemon boot, unfinished work resolves against the record.
fn recover_from_restart(state: &mut DaemonState) {
    for context in state.store.load_contexts() {
        let cursor = context
            .cursor
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        state.ctxs.insert(
            context.id.clone(),
            Ctx {
                id: context.id.clone(),
                provider: context.provider.clone(),
                cwd: PathBuf::from(&context.cwd),
                model: context.model.clone(),
                reasoning_effort: context.reasoning_effort.clone(),
                mode: parse_mode(&context.mode).unwrap_or(RuntimeMode::FullAccess),
                on_question: context.on_question.clone(),
                cursor,
                owner: context
                    .owner_pid
                    .zip(context.owner_start.clone())
                    .map(|(pid, start)| crate::owner::Owner {
                        pid: pid as i32,
                        start,
                    }),
                depth: context.depth,
                closed: context.closed,
                created_at: context.created_at,
                connected: false,
                handle: None,
                _wake_rx: None,
                task: None,
                pending: Vec::new(),
                next_question: 0,
                next_permission: 0,
                waiter: None,
                steering: false,
                pending_prompt: None,
                owner_dead_since: None,
            },
        );
    }
    for task in state.store.load_tasks() {
        let (state_name, error, dormant) = match task.state.as_str() {
            "working" | "submitted" => (
                "failed".to_owned(),
                Some("daemon restarted".to_owned()),
                task.dormant,
            ),
            "input-required" => (task.state.clone(), task.error.clone(), true),
            other => (other.to_owned(), task.error.clone(), task.dormant),
        };
        if state_name != task.state {
            state
                .store
                .update_task_state(&task.context_id, task.id, &state_name);
        }
        if dormant != task.dormant {
            state
                .store
                .update_task_dormant(&task.context_id, task.id, dormant);
        }
        if let Some(error) = &error {
            state
                .store
                .update_task_error(&task.context_id, task.id, error);
        }
        let Some(ctx) = state.ctxs.get_mut(&task.context_id) else {
            continue;
        };
        ctx.task = Some(Task {
            id: task.id,
            state: state_name,
            result: task.result_text,
            error,
            dormant,
            finished_at: task.finished_at,
        });
    }
    for request in state.store.load_requests() {
        let Some(ctx) = state.ctxs.get_mut(&request.context_id) else {
            continue;
        };
        if request.answered {
            continue;
        }
        let Ok(payload_value) = serde_json::from_str::<Value>(&request.payload) else {
            continue;
        };
        let payload = if request.kind == "question" {
            serde_json::from_value::<Vec<UserInputQuestion>>(payload_value)
                .ok()
                .map(Payload::Question)
        } else {
            serde_json::from_value::<Value>(payload_value).ok().map(|value| {
                Payload::Permission {
                    title: value["title"].as_str().unwrap_or_default().to_owned(),
                    detail: value["detail"].as_str().unwrap_or_default().to_owned(),
                    options: serde_json::from_value(value["options"].clone()).unwrap_or_default(),
                }
            })
        };
        let Some(payload) = payload else {
            continue;
        };
        let kind = if request.kind == "question" {
            "question"
        } else {
            "permission"
        };
        ctx.next_question = ctx.next_question.max(
            ctx.pending
                .iter()
                .filter(|r| r.kind == "question")
                .count() as u32,
        );
        ctx.next_permission = ctx.next_permission.max(
            ctx.pending
                .iter()
                .filter(|r| r.kind == "permission")
                .count() as u32,
        );
        ctx.pending.push(PendingReq {
            id: request.id,
            task_id: request.task_id,
            kind,
            driver_request_id: request.driver_request_id,
            payload,
            created_at: request.created_at,
        });
    }
}

// ---------------------------------------------------------------------------
// connection handling

fn serve_connection(shared: Shared, stream: UnixStream, home: PathBuf) -> anyhow::Result<()> {
    let stream: SharedStream = Arc::new(Mutex::new(stream));
    let clone = stream
        .lock()
        .ok()
        .and_then(|guarded| guarded.try_clone().ok())
        .ok_or_else(|| anyhow::anyhow!("socket vanished"))?;
    let mut reader = BufReader::new(clone);
    loop {
        match rpc::read_message(&mut reader) {
            rpc::Incoming::Closed => break,
            rpc::Incoming::TimedOut => continue,
            rpc::Incoming::Notification { .. } | rpc::Incoming::Response { .. } => continue,
            rpc::Incoming::Request { id, method, params } => {
                let outcome = handle_request(&shared, &stream, &id, &method, params);
                if outcome == Flow::Disconnect {
                    break;
                }
                if method == "daemon/shutdown" {
                    request_shutdown();
                    break;
                }
            }
        }
    }
    // Connection gone: drop any waiter registered on it.
    if let Ok(mut state) = shared.lock() {
        for ctx in state.ctxs.values_mut() {
            let same = ctx
                .waiter
                .as_ref()
                .is_some_and(|waiter| Arc::ptr_eq(&waiter.stream, &stream));
            if same {
                ctx.waiter = None;
            }
        }
    }
    log_line(&home, "connection ended");
    Ok(())
}

#[derive(PartialEq)]
enum Flow {
    Continue,
    Disconnect,
}

fn handle_request(
    shared: &Shared,
    stream: &SharedStream,
    id: &Value,
    method: &str,
    params: Value,
) -> Flow {
    let mut state = match shared.lock() {
        Ok(state) => state,
        Err(_) => {
            send_raw(
                stream,
                &rpc::error(id.clone(), "daemon state poisoned", "internal"),
            );
            return Flow::Continue;
        }
    };
    match dispatch(&mut state, stream, id, method, params) {
        Ok(flow) => flow,
        Err(error) => {
            send_raw(stream, &rpc::error(id.clone(), &error.message, &error.code));
            Flow::Continue
        }
    }
}

fn dispatch(
    state: &mut DaemonState,
    stream: &SharedStream,
    id: &Value,
    method: &str,
    params: Value,
) -> Result<Flow, rpc::RpcError> {
    match method {
        "daemon/ping" => {
            send_raw(stream, &rpc::response(id.clone(), json!({ "version": 1 })));
            Ok(Flow::Continue)
        }
        "contexts/create" => {
            let context_id = context_create(state, &params)?;
            send_raw(
                stream,
                &rpc::response(id.clone(), json!({ "contextId": context_id })),
            );
            Ok(Flow::Continue)
        }
        "message/send" => {
            message_send(state, stream, id, &params)?;
            Ok(Flow::Continue)
        }
        "tasks/resubscribe" => {
            let context_id = param_context(&params)?;
            owner_refresh(state, &params)?;
            let waiter = Waiter {
                stream: stream.clone(),
                id: id.clone(),
            };
            {
                let ctx = ctx_mut(state, &context_id)?;
                supersede(ctx);
                if at_stop_point(ctx) {
                    let status = current_status(ctx);
                    respond_waiter(&waiter, &status);
                } else {
                    ctx.waiter = Some(waiter);
                }
            }
            Ok(Flow::Continue)
        }
        "tasks/get" => {
            let ctx = ctx_mut(state, param_context(&params)?.as_str())?;
            let status = current_status(ctx);
            send_raw(stream, &rpc::response(id.clone(), status));
            Ok(Flow::Continue)
        }
        "tasks/cancel" => {
            let context_id = param_context(&params)?;
            let status = task_cancel(state, &context_id);
            send_raw(stream, &rpc::response(id.clone(), status));
            Ok(Flow::Continue)
        }
        "tasks/result" => {
            let ctx = ctx_mut(state, param_context(&params)?.as_str())?;
            let text = ctx
                .task
                .as_ref()
                .map(|task| task.result.clone())
                .unwrap_or_default();
            send_raw(stream, &rpc::response(id.clone(), json!({ "text": text })));
            Ok(Flow::Continue)
        }
        "tasks/pending" => {
            let ctx = ctx_mut(state, param_context(&params)?.as_str())?;
            let requests: Vec<Value> = ctx
                .pending
                .iter()
                .map(|request| {
                    json!({
                        "id": request.id,
                        "taskId": format!("t{}", request.task_id),
                        "kind": request.kind,
                        "payload": request.payload.to_json(),
                    })
                })
                .collect();
            send_raw(
                stream,
                &rpc::response(id.clone(), json!({ "requests": requests })),
            );
            Ok(Flow::Continue)
        }
        "contexts/close" => {
            let context_id = param_context(&params)?;
            context_close(state, &context_id);
            send_raw(stream, &rpc::response(id.clone(), Value::Null));
            Ok(Flow::Continue)
        }
        "contexts/list" => {
            let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
            let contexts: Vec<Value> = state
                .ctxs
                .values()
                .filter(|ctx| all || !ctx.closed)
                .map(|ctx| {
                    let task = ctx.task.as_ref();
                    json!({
                        "id": ctx.id,
                        "provider": ctx.provider,
                        "cwd": ctx.cwd.display().to_string(),
                        "mode": mode_name(ctx.mode),
                        "depth": ctx.depth,
                        "closed": ctx.closed,
                        "dormant": task.is_some_and(|task| task.dormant),
                        "state": task.map(|task| task.state.clone()),
                        "taskId": task.map(|task| format!("t{}", task.id)),
                        "createdAt": ctx.created_at,
                    })
                })
                .collect();
            send_raw(
                stream,
                &rpc::response(id.clone(), json!({ "contexts": contexts })),
            );
            Ok(Flow::Continue)
        }
        "daemon/shutdown" => {
            shutdown(state);
            send_raw(stream, &rpc::response(id.clone(), Value::Null));
            Ok(Flow::Disconnect)
        }
        _ => Err(rpc::RpcError {
            message: format!("unknown method {method}"),
            code: "unknown-method".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// contexts

fn context_create(state: &mut DaemonState, params: &Value) -> Result<String, rpc::RpcError> {
    let provider_name = params
        .get("provider")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("provider is required"))?
        .to_owned();
    let provider = parse_provider(&provider_name).ok_or_else(|| bad("unknown provider"))?;
    if provider == ProviderKind::OpenCode {
        return Err(rpc::RpcError {
            message: "OpenCode is not supported by wk v1 (shared-service conflict)".into(),
            code: rpc::codes::PROVIDER_UNAVAILABLE.into(),
        });
    }
    let mode_name_in = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("mode is required"))?
        .to_owned();
    let mode = parse_mode(&mode_name_in).ok_or_else(|| bad("unknown mode"))?;
    // Providers that never emit questions hard-error on Ask at driver start;
    // reject up front with a clearer message (§11).
    if mode == RuntimeMode::Ask
        && matches!(
            provider,
            ProviderKind::Amp | ProviderKind::Pi | ProviderKind::OhMyPi
        )
    {
        return Err(rpc::RpcError {
            message: format!("{provider_name} never asks questions; --mode ask is unavailable"),
            code: rpc::codes::INVALID_MODE.into(),
        });
    }
    let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(1);
    if depth > state.max_depth {
        return Err(rpc::RpcError {
            message: format!(
                "nesting depth {depth} exceeds the maximum of {}",
                state.max_depth
            ),
            code: rpc::codes::DEPTH_EXCEEDED.into(),
        });
    }
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("cwd is required"))?
        .to_owned();
    let model = params.get("model").and_then(Value::as_str).map(str::to_owned);
    let effort = params
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let on_question = params
        .get("onQuestion")
        .and_then(Value::as_str)
        .unwrap_or("yield")
        .to_owned();
    let owner = params
        .get("owner")
        .and_then(|owner| {
            let pid = owner.get("pid")?.as_i64()? as i32;
            let start = owner.get("start")?.as_str()?.to_owned();
            (pid != 0).then_some(crate::owner::Owner { pid, start })
        });
    let id = crate::process::new_context_id();
    let created_at = now_secs();
    state.store.insert_context(&store::ContextRow {
        id: id.clone(),
        provider: provider_name.clone(),
        cwd: cwd.clone(),
        model: model.clone(),
        reasoning_effort: effort.clone(),
        mode: mode_name(mode).to_owned(),
        on_question: on_question.clone(),
        cursor: None,
        owner_pid: owner.as_ref().map(|owner| owner.pid as i64),
        owner_start: owner.as_ref().map(|owner| owner.start.clone()),
        depth,
        closed: false,
        created_at,
    });
    state.ctxs.insert(
        id.clone(),
        Ctx {
            id: id.clone(),
            provider: provider_name,
            cwd: PathBuf::from(cwd),
            model,
            reasoning_effort: effort,
            mode,
            on_question,
            cursor: None,
            owner,
            depth,
            closed: false,
            created_at,
            connected: false,
            handle: None,
            _wake_rx: None,
            task: None,
            pending: Vec::new(),
            next_question: 0,
            next_permission: 0,
            waiter: None,
            steering: false,
            pending_prompt: None,
            owner_dead_since: None,
        },
    );
    Ok(id)
}

// ---------------------------------------------------------------------------
// message/send (§5.3)

fn message_send(
    state: &mut DaemonState,
    stream: &SharedStream,
    id: &Value,
    params: &Value,
) -> Result<(), rpc::RpcError> {
    let context_id = param_context(params)?;
    let wait = params.get("wait").and_then(Value::as_bool).unwrap_or(true);
    owner_refresh(state, params)?;

    enum Action {
        Prompt(String),
        Steer(String),
        Reply {
            index: usize,
        },
    }
    // Phase 1 — validate and decide (read-only over the context).
    let action = {
        let ctx = ctx_mut(state, &context_id)?;
        if ctx.closed {
            return Err(rpc::RpcError {
                message: "context is closed".into(),
                code: rpc::codes::CONTEXT_CLOSED.into(),
            });
        }
        if let Some(reply) = params.get("reply") {
            let request_id = reply
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("reply needs a requestId"))?
                .to_owned();
            match ctx
                .pending
                .iter()
                .position(|request| request.id == request_id)
            {
                Some(index) => Action::Reply { index },
                None => {
                    let code = if ctx
                        .task
                        .as_ref()
                        .is_some_and(|task| task.state == "input-required")
                    {
                        rpc::codes::ALREADY_ANSWERED
                    } else {
                        rpc::codes::NO_PENDING_REQUEST
                    };
                    return Err(rpc::RpcError {
                        message: format!("no pending request {request_id}"),
                        code: code.into(),
                    });
                }
            }
        } else {
            let prompt = params
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("message/send needs a prompt or a reply"))?
                .to_owned();
            match ctx.task.as_ref().map(|task| task.state.as_str()) {
                None | Some("completed") | Some("failed") | Some("canceled") => {
                    Action::Prompt(prompt)
                }
                Some("working") | Some("submitted") => {
                    let steerable = ctx
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.supports_steer());
                    if !steerable {
                        return Err(rpc::RpcError {
                            message: format!(
                                "provider {} cannot steer a running turn",
                                ctx.provider
                            ),
                            code: rpc::codes::REJECTED.into(),
                        });
                    }
                    Action::Steer(prompt)
                }
                Some(_) => {
                    return Err(rpc::RpcError {
                        message: "answer the pending requests or cancel first".into(),
                        code: rpc::codes::REJECTED.into(),
                    });
                }
            }
        }
    };

    // Phase 2 — apply.
    match action {
        Action::Reply { index } => {
            let staged = {
                let ctx = ctx_mut(state, &context_id)?;
                let request = &ctx.pending[index];
                (
                    request.id.clone(),
                    request.kind,
                    request.driver_request_id.clone(),
                    request.payload.clone(),
                )
            };
            let (request_id, kind, driver_request_id, payload) = staged;
            let resolved = resolve_answer(&payload, params.get("reply").unwrap())?;
            state
                .store
                .update_request_answered(&context_id, &request_id, &resolved.summary);

            let dormant;
            let new_state;
            {
                let ctx = ctx_mut(state, &context_id)?;
                ctx.pending.remove(index);
                let task = ctx.task.as_mut().ok_or_else(|| rpc::RpcError {
                    message: "no active task".into(),
                    code: rpc::codes::NO_PENDING_REQUEST.into(),
                })?;
                dormant = task.dormant;
                if task.state == "input-required" && ctx.pending.is_empty() {
                    task.state = "working".into();
                }
                if dormant {
                    task.dormant = false;
                }
                new_state = task.state.clone();
            }
            if new_state == "working" {
                if let Some(task) = state
                    .ctxs
                    .get(&context_id)
                    .and_then(|ctx| ctx.task.as_ref())
                {
                    state.store.update_task_state(&context_id, task.id, "working");
                }
            }
            if dormant {
                state
                    .store
                    .update_task_dormant(&context_id, task_id_of(state, &context_id), false);
                // §7.3: the provider is gone; restart it from the cursor and
                // deliver the answer as a synthesized prompt on `Connected`.
                let prompt = synthesized_prompt(kind, &payload, &resolved);
                let mut start_error = None;
                {
                    let ctx = ctx_mut(state, &context_id)?;
                    ctx.pending_prompt = Some(prompt);
                    if let Err(error) = ensure_driver(ctx) {
                        start_error = Some(error);
                    }
                }
                if let Some(error) = start_error {
                    fail_task(state, &context_id, &error);
                }
            } else if let Some(handle) = state
                .ctxs
                .get(&context_id)
                .and_then(|ctx| ctx.handle.clone())
            {
                match kind {
                    "question" => handle.respond_user_input(driver_request_id, resolved.answers),
                    _ => handle.respond(driver_request_id, resolved.summary),
                }
            }
        }
        Action::Prompt(prompt) => {
            let task_id = state
                .ctxs
                .get(&context_id)
                .and_then(|ctx| ctx.task.as_ref())
                .map(|task| task.id)
                .unwrap_or(0)
                + 1;
            state.store.insert_task(&store::TaskRow {
                context_id: context_id.clone(),
                id: task_id,
                state: "submitted".into(),
                dormant: false,
                prompt: prompt.clone(),
                result_text: String::new(),
                error: None,
                created_at: now_secs(),
                finished_at: None,
            });
            {
                let ctx = ctx_mut(state, &context_id)?;
                ctx.task = Some(Task {
                    id: task_id,
                    state: "submitted".into(),
                    result: String::new(),
                    error: None,
                    dormant: false,
                    finished_at: None,
                });
                if ctx.connected && ctx.handle.is_some() {
                    if let Some(handle) = ctx.handle.as_ref() {
                        handle.prompt(prompt);
                    }
                    ctx.task.as_mut().unwrap().state = "working".into();
                } else {
                    ctx.pending_prompt = Some(prompt);
                    if let Err(error) = ensure_driver(ctx) {
                        let task = ctx.task.as_mut().unwrap();
                        task.state = "failed".into();
                        task.error = Some(error);
                        task.finished_at = Some(now_secs());
                    }
                }
            }
            if let Some(task) = state
                .ctxs
                .get(&context_id)
                .and_then(|ctx| ctx.task.as_ref())
            {
                state
                    .store
                    .update_task_state(&context_id, task.id, &task.state);
                if let Some(error) = &task.error {
                    state.store.update_task_error(&context_id, task.id, error);
                }
            }
        }
        Action::Steer(prompt) => {
            let handle = state
                .ctxs
                .get(&context_id)
                .and_then(|ctx| ctx.handle.clone())
                .ok_or_else(|| rpc::RpcError {
                    message: "driver is not running".into(),
                    code: rpc::codes::PROVIDER_UNAVAILABLE.into(),
                })?;
            handle.steer(prompt);
            if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                ctx.steering = true;
            }
        }
    }

    // Phase 3 — the response is the status at the next stop point (§6.2).
    let waiter = Waiter {
        stream: stream.clone(),
        id: id.clone(),
    };
    if wait {
        let ctx = ctx_mut(state, &context_id)?;
        supersede(ctx);
        if at_stop_point(ctx) {
            let status = current_status(ctx);
            respond_waiter(&waiter, &status);
        } else {
            ctx.waiter = Some(waiter);
        }
    } else {
        let status = current_status(ctx_mut(state, &context_id)?);
        send_raw(stream, &rpc::response(id.clone(), status));
    }
    Ok(())
}

fn task_id_of(state: &DaemonState, context_id: &str) -> i64 {
    state
        .ctxs
        .get(context_id)
        .and_then(|ctx| ctx.task.as_ref())
        .map(|task| task.id)
        .unwrap_or(0)
}

fn fail_task(state: &mut DaemonState, context_id: &str, error: &str) {
    let task_id = {
        let Some(ctx) = state.ctxs.get_mut(context_id) else {
            return;
        };
        let Some(task) = ctx.task.as_mut() else {
            return;
        };
        if task.terminal() {
            return;
        }
        task.state = "failed".into();
        task.error = Some(error.to_owned());
        task.finished_at = Some(now_secs());
        task.id
    };
    state.store.update_task_state(context_id, task_id, "failed");
    state.store.update_task_error(context_id, task_id, error);
    settle(state, context_id);
}

struct ResolvedAnswer {
    summary: String,
    answers: Vec<UserInputAnswer>,
}

/// The CLI sends raw values; resolve them against the stored payload (§5.2).
fn resolve_answer(payload: &Payload, reply: &Value) -> Result<ResolvedAnswer, rpc::RpcError> {
    let invalid = || rpc::RpcError {
        message: "the answer does not fit this request".into(),
        code: rpc::codes::INVALID_ANSWER.into(),
    };
    match payload {
        Payload::Permission { options, .. } => {
            let value = reply
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let option_id = if let Some(index) = parse_index(value) {
                options
                    .get(index - 1)
                    .map(|option| option.id.clone())
                    .ok_or_else(invalid)?
            } else if let Some(option) = options.iter().find(|option| option.id == value) {
                option.id.clone()
            } else if let Some(option) = options.iter().find(|option| option.label == value) {
                option.id.clone()
            } else {
                return Err(invalid());
            };
            Ok(ResolvedAnswer {
                summary: option_id,
                answers: Vec::new(),
            })
        }
        Payload::Question(questions) => {
            // `answers` maps questionId -> raw values (multi_select repeats);
            // a bare `value` applies when the request has one sub-question.
            let mut per_question: HashMap<String, Vec<String>> = HashMap::new();
            if let Some(value) = reply.get("value").and_then(Value::as_str) {
                if questions.len() != 1 {
                    return Err(invalid());
                }
                per_question
                    .entry(questions[0].id.clone())
                    .or_default()
                    .push(value.to_owned());
            }
            if let Some(pairs) = reply.get("answers").and_then(Value::as_array) {
                for pair in pairs {
                    let key = pair.get(0).and_then(Value::as_str).unwrap_or_default();
                    let value = pair.get(1).and_then(Value::as_str).unwrap_or_default();
                    if !questions.iter().any(|question| question.id == key) {
                        return Err(invalid());
                    }
                    per_question
                        .entry(key.to_owned())
                        .or_default()
                        .push(value.to_owned());
                }
            }
            let mut answers: Vec<UserInputAnswer> = Vec::new();
            let mut summary: Vec<String> = Vec::new();
            for question in questions {
                let values = per_question.remove(&question.id).ok_or_else(invalid)?;
                if values.is_empty() {
                    return Err(invalid());
                }
                let mut resolved = Vec::new();
                for value in values {
                    resolved.push(resolve_question_value(question, &value).ok_or_else(invalid)?);
                }
                summary.push(format!("{}: {}", question.id, resolved.join(", ")));
                answers.push(UserInputAnswer {
                    question_id: question.id.clone(),
                    answers: resolved,
                });
            }
            if !per_question.is_empty() {
                return Err(invalid());
            }
            Ok(ResolvedAnswer {
                summary: summary.join("; "),
                answers,
            })
        }
    }
}

fn resolve_question_value(question: &UserInputQuestion, value: &str) -> Option<String> {
    if question.options.is_empty() {
        return Some(value.to_owned());
    }
    if let Some(index) = parse_index(value) {
        return question
            .options
            .get(index - 1)
            .map(|option| option.label.clone());
    }
    if let Some(option) = question.options.iter().find(|option| option.label == value) {
        return Some(option.label.clone());
    }
    // Free text is always allowed, options or not (§5.2).
    Some(value.to_owned())
}

fn synthesized_prompt(kind: &str, payload: &Payload, resolved: &ResolvedAnswer) -> String {
    if kind == "question" {
        format!(
            "Earlier you asked:\n{}\nAnswer: {}\nContinue the task.",
            payload_summary(payload),
            resolved.summary
        )
    } else {
        let title = payload_title(payload);
        let allowed = match payload {
            Payload::Permission { options, .. } => options
                .iter()
                .any(|option| option.id == resolved.summary && option.allow),
            _ => false,
        };
        if allowed {
            format!("Permission granted for: {title}. Retry it and continue.")
        } else {
            format!("Permission denied for: {title}. Continue without it.")
        }
    }
}

fn payload_summary(payload: &Payload) -> String {
    match payload {
        Payload::Question(questions) => questions
            .iter()
            .map(|question| {
                let options = question
                    .options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| format!("{}) {}", index + 1, option.label))
                    .collect::<Vec<_>>()
                    .join(" ");
                if options.is_empty() {
                    question.question.clone()
                } else {
                    format!("{} {}", question.question, options)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Permission { title, .. } => title.clone(),
    }
}

fn payload_title(payload: &Payload) -> String {
    match payload {
        Payload::Question(questions) => questions
            .first()
            .map(|question| question.question.clone())
            .unwrap_or_default(),
        Payload::Permission { title, .. } => title.clone(),
    }
}

// ---------------------------------------------------------------------------
// cancel / close / shutdown

fn task_cancel(state: &mut DaemonState, context_id: &str) -> Value {
    let canceled = {
        let Ok(ctx) = ctx_mut(state, context_id) else {
            return Value::Null;
        };
        let mut canceled = false;
        if let Some(task) = ctx.task.as_mut() {
            if !task.terminal() {
                task.state = "canceled".into();
                task.finished_at = Some(now_secs());
                canceled = true;
            }
        }
        if let Some(handle) = ctx.handle.as_ref() {
            handle.cancel();
        }
        canceled.then(|| ctx.task.as_ref().map(|task| task.id)).flatten()
    };
    if let Some(task_id) = canceled {
        state.store.update_task_state(context_id, task_id, "canceled");
    }
    let status = match ctx_mut(state, context_id) {
        Ok(ctx) => current_status(ctx),
        Err(_) => Value::Null,
    };
    if let Ok(ctx) = ctx_mut(state, context_id) {
        if let Some(waiter) = ctx.waiter.take() {
            respond_waiter(&waiter, &status);
        }
    }
    status
}

fn context_close(state: &mut DaemonState, context_id: &str) {
    let Ok(ctx) = ctx_mut(state, context_id) else {
        return;
    };
    ctx.closed = true;
    let canceled_task = ctx.task.as_mut().and_then(|task| {
        (!task.terminal()).then(|| {
            task.state = "canceled".into();
            task.finished_at = Some(now_secs());
            task.id
        })
    });
    if let Some(handle) = ctx.handle.take() {
        handle.cancel();
    }
    ctx.connected = false;
    ctx._wake_rx = None;
    ctx.pending.clear();
    state.store.set_closed(context_id);
    if let Some(task_id) = canceled_task {
        state.store.update_task_state(context_id, task_id, "canceled");
    }
}

fn shutdown(state: &mut DaemonState) {
    state.shutting_down = true;
    let context_ids: Vec<String> = state.ctxs.keys().cloned().collect();
    for context_id in context_ids {
        let updates = {
            let Ok(ctx) = ctx_mut(state, &context_id) else {
                continue;
            };
            let mut updates: Vec<(i64, &str, Option<String>)> = Vec::new();
            if let Some(task) = ctx.task.as_mut() {
                match task.state.as_str() {
                    "working" | "submitted" => {
                        task.state = "failed".into();
                        task.error = Some("daemon restarted".into());
                        task.finished_at = Some(now_secs());
                        updates.push((task.id, "failed", Some("daemon restarted".into())));
                    }
                    "input-required" if !task.dormant => {
                        task.dormant = true;
                    }
                    _ => {}
                }
            }
            if let Some(handle) = ctx.handle.take() {
                handle.cancel();
            }
            ctx.connected = false;
            ctx._wake_rx = None;
            let dormant_task = ctx
                .task
                .as_ref()
                .filter(|task| task.dormant && task.state == "input-required")
                .map(|task| task.id);
            (updates, dormant_task)
        };
        for (task_id, state_name, error) in updates.0 {
            state.store.update_task_state(&context_id, task_id, state_name);
            if let Some(error) = error {
                state.store.update_task_error(&context_id, task_id, &error);
            }
        }
        if let Some(task_id) = updates.1 {
            state.store.update_task_dormant(&context_id, task_id, true);
        }
    }
}

// ---------------------------------------------------------------------------
// driver lifecycle

fn ensure_driver(ctx: &mut Ctx) -> Result<(), String> {
    if ctx.handle.is_some() {
        return Ok(());
    }
    let provider =
        parse_provider(&ctx.provider).ok_or_else(|| format!("unknown provider {}", ctx.provider))?;
    let binary = waku_core::command_env::find_executable(provider.command()).ok_or_else(|| {
        format!(
            "provider binary `{}` not found on PATH",
            provider.command()
        )
    })?;
    let (wake, wake_rx) = smol::channel::bounded(1);
    let (sender, receiver) = event_channel(wake);
    let options = DriverStartOptions {
        binary,
        cwd: ctx.cwd.clone(),
        mode: ctx.mode,
        model: ctx.model.clone(),
        reasoning_effort: ctx.reasoning_effort.clone(),
        service_tier: None,
        context_window: None,
        agent_preset: None,
        computer_use_enabled: false,
        provider_cursor: ctx.cursor.clone(),
        extra_env: vec![
            ("WK_DEPTH".into(), ctx.depth.to_string()),
            ("WK_CONTEXT".into(), ctx.id.clone()),
        ],
    };
    let handle = start_local(provider, options, sender).map_err(|error| error.to_string())?;
    ctx.handle = Some(handle);
    ctx.connected = false;
    ctx._wake_rx = Some(wake_rx);
    let shared = STATE
        .get()
        .ok_or_else(|| "daemon state unavailable".to_owned())?
        .clone();
    let context_id = ctx.id.clone();
    std::thread::Builder::new()
        .name(format!("wk-pump-{}", ctx.id))
        .spawn(move || pump_loop(shared, context_id, receiver))
        .map_err(|error| format!("failed to start the event pump: {error}"))?;
    Ok(())
}

/// The pump thread: one per running driver, translating `DriverEvent`s into
/// task state transitions (§4).
fn pump_loop(
    shared: Shared,
    context_id: String,
    receiver: crossbeam_channel::Receiver<DriverEvent>,
) {
    while let Ok(event) = receiver.recv() {
        let Ok(mut state) = shared.lock() else {
            return;
        };
        if state.shutting_down {
            return;
        }
        match event {
            DriverEvent::Connected { provider_cursor } => {
                let mut cursor_update = None;
                let task_update;
                {
                    let Some(ctx) = state.ctxs.get_mut(&context_id) else {
                        continue;
                    };
                    ctx.connected = true;
                    if let Some(cursor) = provider_cursor {
                        if ctx.cursor.as_ref() != Some(&cursor) {
                            cursor_update =
                                Some(serde_json::to_string(&cursor).unwrap_or_default());
                            ctx.cursor = Some(cursor);
                        }
                    }
                    if let Some(prompt) = ctx.pending_prompt.take() {
                        if let Some(handle) = ctx.handle.as_ref() {
                            handle.prompt(prompt);
                        }
                        if let Some(task) = ctx.task.as_mut() {
                            if task.state == "submitted" {
                                task.state = "working".into();
                            }
                            task.dormant = false;
                        }
                    }
                    task_update = ctx
                        .task
                        .as_ref()
                        .map(|task| (task.id, task.state.clone()));
                }
                if let Some(cursor_json) = cursor_update {
                    state.store.update_cursor(&context_id, Some(&cursor_json));
                }
                if let Some((task_id, task_state)) = task_update {
                    state
                        .store
                        .update_task_state(&context_id, task_id, &task_state);
                    state
                        .store
                        .update_task_dormant(&context_id, task_id, false);
                }
            }
            DriverEvent::TurnStarted => {
                let task = state
                    .ctxs
                    .get_mut(&context_id)
                    .and_then(|ctx| ctx.task.as_mut())
                    .filter(|task| task.state == "submitted")
                    .map(|task| {
                        task.state = "working".into();
                        task.id
                    });
                if let Some(task_id) = task {
                    state.store.update_task_state(&context_id, task_id, "working");
                }
            }
            DriverEvent::TextDelta(delta) => {
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    if let Some(task) = ctx.task.as_mut() {
                        if !task.terminal() {
                            task.result.push_str(&delta);
                        }
                    }
                }
            }
            DriverEvent::Permission {
                request_id,
                title,
                detail,
                options,
            } => {
                on_request(
                    &mut state,
                    &context_id,
                    "permission",
                    request_id,
                    Payload::Permission {
                        title,
                        detail,
                        options,
                    },
                );
            }
            DriverEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                let policy = state
                    .ctxs
                    .get(&context_id)
                    .map(|ctx| ctx.on_question.clone())
                    .unwrap_or_default();
                match policy.as_str() {
                    "auto" => {
                        let handle = state
                            .ctxs
                            .get(&context_id)
                            .and_then(|ctx| ctx.handle.clone());
                        if let Some(handle) = handle {
                            let answers = questions
                                .iter()
                                .map(|question| UserInputAnswer {
                                    question_id: question.id.clone(),
                                    answers: vec![AUTO_ANSWER.to_owned()],
                                })
                                .collect::<Vec<_>>();
                            handle.respond_user_input(request_id, answers);
                        }
                    }
                    "fail" => {
                        on_question_fail(&mut state, &context_id, &questions);
                    }
                    _ => on_request(
                        &mut state,
                        &context_id,
                        "question",
                        request_id,
                        Payload::Question(questions),
                    ),
                }
            }
            DriverEvent::SteerAccepted { .. } => {
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    ctx.steering = false;
                }
            }
            DriverEvent::SteerRejected { reason, .. } => {
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    ctx.steering = false;
                    if let Some(waiter) = ctx.waiter.take() {
                        respond_waiter_error(
                            &waiter,
                            &format!("steer rejected: {reason}"),
                            rpc::codes::REJECTED,
                        );
                    }
                }
            }
            DriverEvent::TurnFinished { success, .. } => {
                let terminal = state
                    .ctxs
                    .get_mut(&context_id)
                    .and_then(|ctx| ctx.task.as_mut())
                    .filter(|task| !task.terminal())
                    .map(|task| {
                        task.state = if success { "completed" } else { "failed" }.into();
                        if task.state == "failed" && task.error.is_none() {
                            task.error = Some("the provider ended the turn with an error".into());
                        }
                        task.finished_at = Some(now_secs());
                    });
                if terminal.is_some() {
                    finish_task(&mut state, &context_id);
                }
            }
            DriverEvent::ProcessExited => {
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    if let Some(task) = ctx.task.as_mut() {
                        if !task.terminal() {
                            task.state = "failed".into();
                            task.error = Some("provider exited".into());
                            task.finished_at = Some(now_secs());
                        }
                    }
                    ctx.handle = None;
                    ctx.connected = false;
                    ctx._wake_rx = None;
                }
                finish_task(&mut state, &context_id);
            }
            // TurnParked (verified in waku-core): only the Claude driver emits
            // it — the turn stays open for the provider's own wake while
            // detached work runs. The task therefore remains `working`.
            DriverEvent::TurnParked => {}
            DriverEvent::Error(_) => {}
            _ => {}
        }
    }
}

fn on_question_fail(state: &mut DaemonState, context_id: &str, questions: &[UserInputQuestion]) {
    let summary = questions
        .first()
        .map(|question| question.question.clone())
        .unwrap_or_default();
    let error = format!("question-not-allowed: {summary}");
    if let Some(ctx) = state.ctxs.get_mut(context_id) {
        if let Some(task) = ctx.task.as_mut() {
            if !task.terminal() {
                task.state = "failed".into();
                task.error = Some(error.clone());
                task.finished_at = Some(now_secs());
            }
        }
        if let Some(handle) = ctx.handle.as_ref() {
            handle.cancel();
        }
    }
    fail_task(state, context_id, &error);
}

fn finish_task(state: &mut DaemonState, context_id: &str) {
    let staged = state.ctxs.get(context_id).and_then(|ctx| {
        ctx.task
            .as_ref()
            .map(|task| (task.id, task.state.clone(), task.result.clone(), task.error.clone()))
    });
    if let Some((task_id, task_state, result, error)) = staged {
        state.store.update_task_state(context_id, task_id, &task_state);
        state.store.update_task_result(context_id, task_id, &result);
        if let Some(error) = error {
            state.store.update_task_error(context_id, task_id, &error);
        }
    }
    if let Some(ctx) = state.ctxs.get_mut(context_id) {
        ctx.pending.clear();
    }
    settle(state, context_id);
}

fn on_request(
    state: &mut DaemonState,
    context_id: &str,
    kind: &'static str,
    driver_request_id: String,
    payload: Payload,
) {
    let task_id = match state
        .ctxs
        .get(context_id)
        .and_then(|ctx| ctx.task.as_ref())
    {
        Some(task) if !task.terminal() => task.id,
        _ => return,
    };
    let id = {
        let ctx = state.ctxs.get_mut(context_id).unwrap();
        if kind == "question" {
            ctx.next_question += 1;
            format!("q{}", ctx.next_question)
        } else {
            ctx.next_permission += 1;
            format!("p{}", ctx.next_permission)
        }
    };
    let created_at = now_secs();
    state.store.insert_request(&store::RequestRow {
        context_id: context_id.to_owned(),
        id: id.clone(),
        task_id,
        kind: kind.to_owned(),
        driver_request_id: driver_request_id.clone(),
        payload: serde_json::to_string(&payload.to_json()).unwrap_or_default(),
        answered: false,
        answer: None,
        answered_at: None,
        created_at,
    });
    let Some(ctx) = state.ctxs.get_mut(context_id) else {
        return;
    };
    ctx.pending.push(PendingReq {
        id,
        task_id,
        kind,
        driver_request_id,
        payload,
        created_at,
    });
    if let Some(task) = ctx.task.as_mut() {
        task.state = "input-required".into();
    }
    state
        .store
        .update_task_state(context_id, task_id, "input-required");
    settle(state, context_id);
}

/// A stop point was reached: flush text, answer the waiter (§6.2).
fn settle(state: &mut DaemonState, context_id: &str) {
    let Some(ctx) = state.ctxs.get_mut(context_id) else {
        return;
    };
    if let Some(task) = ctx.task.as_ref() {
        let result = task.result.clone();
        state
            .store
            .update_task_result(context_id, task.id, &result);
    }
    let status = current_status(ctx);
    if let Some(waiter) = ctx.waiter.take() {
        respond_waiter(&waiter, &status);
    }
}

fn at_stop_point(ctx: &Ctx) -> bool {
    match ctx.task.as_ref() {
        None => true,
        Some(task) => task.terminal() || task.state == "input-required",
    }
}

fn current_status(ctx: &Ctx) -> Value {
    let task = ctx.task.as_ref();
    let state_name = task.map(|task| task.state.as_str()).unwrap_or("idle");
    let pending: Vec<Value> = ctx
        .pending
        .iter()
        .map(|request| {
            json!({
                "id": request.id,
                "taskId": format!("t{}", request.task_id),
                "kind": request.kind,
                "payload": request.payload.to_json(),
            })
        })
        .collect();
    let mut status = json!({
        "contextId": ctx.id,
        "taskId": task.map(|task| format!("t{}", task.id)),
        "state": state_name,
        "pending": pending,
        "dormant": task.is_some_and(|task| task.dormant),
        "error": task.and_then(|task| task.error.clone()),
        "next": next_command(ctx),
    });
    if let Some(request) = ctx.pending.first() {
        status["inputKind"] = Value::String(request.kind.into());
    }
    status
}

fn next_command(ctx: &Ctx) -> String {
    match ctx.task.as_ref().map(|task| task.state.as_str()) {
        Some("input-required") => match ctx.pending.first() {
            Some(request) if request.kind == "permission" => {
                format!("wk send {} --reply {} <option>", ctx.id, request.id)
            }
            Some(request) => format!("wk send {} --reply {} <answer>", ctx.id, request.id),
            None => format!("wk wait {}", ctx.id),
        },
        Some("working") | Some("submitted") => format!("wk wait {}", ctx.id),
        Some(_) => format!("wk send {} \"<follow-up>\"", ctx.id),
        None => format!("wk send {} \"<prompt>\"", ctx.id),
    }
}

// ---------------------------------------------------------------------------
// waiters (§6)

fn supersede(ctx: &mut Ctx) {
    if let Some(old) = ctx.waiter.take() {
        send_raw(&old.stream, &rpc::notification("waiter/superseded", json!({})));
    }
}

fn respond_waiter(waiter: &Waiter, status: &Value) {
    send_raw(
        &waiter.stream,
        &rpc::response(waiter.id.clone(), status.clone()),
    );
}

fn respond_waiter_error(waiter: &Waiter, message: &str, code: &str) {
    send_raw(&waiter.stream, &rpc::error(waiter.id.clone(), message, code));
}

fn send_raw(stream: &SharedStream, line: &str) {
    if let Ok(mut stream) = stream.lock() {
        rpc::write_line(&mut stream, line);
    }
}

// ---------------------------------------------------------------------------
// janitor (§7)

fn janitor_loop(shared: Shared) {
    loop {
        std::thread::sleep(OWNER_POLL);
        let Ok(mut state) = shared.lock() else {
            return;
        };
        if state.shutting_down {
            return;
        }
        let now = Instant::now();
        let context_ids: Vec<String> = state.ctxs.keys().cloned().collect();
        for context_id in context_ids {
            let owner_dead = state
                .ctxs
                .get(&context_id)
                .and_then(|ctx| ctx.owner.as_ref())
                .map(|owner| !crate::owner::is_alive(owner))
                .unwrap_or(false);
            let owner_dead_since = {
                let Some(ctx) = state.ctxs.get_mut(&context_id) else {
                    continue;
                };
                if owner_dead && ctx.owner_dead_since.is_none() {
                    ctx.owner_dead_since = Some(now);
                }
                ctx.owner_dead_since
            };
            let task_snapshot = state
                .ctxs
                .get(&context_id)
                .and_then(|ctx| ctx.task.as_ref())
                .map(|task| {
                    (
                        task.id,
                        task.state.clone(),
                        task.dormant,
                        task.finished_at,
                    )
                });
            let pending_oldest = state
                .ctxs
                .get(&context_id)
                .map(|ctx| ctx.pending.iter().map(|request| request.created_at).min())
                .unwrap_or(None);

            // Owner death (§7.2): after the grace period, cancel the task and
            // stop the driver; the context and its cursor survive.
            if owner_dead_since.is_some_and(|dead_since| dead_since.elapsed() >= OWNER_GRACE) {
                let mut canceled = None;
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    if let Some(task) = ctx.task.as_mut() {
                        if !task.terminal() {
                            task.state = "canceled".into();
                            task.finished_at = Some(now_secs());
                            canceled = Some(task.id);
                        }
                    }
                    if let Some(handle) = ctx.handle.take() {
                        handle.cancel();
                    }
                    ctx.connected = false;
                    ctx._wake_rx = None;
                    ctx.owner_dead_since = None;
                }
                if let Some(task_id) = canceled {
                    state
                        .store
                        .update_task_state(&context_id, task_id, "canceled");
                }
                settle(&mut state, &context_id);
                continue;
            }
            // Dormancy (§7.3): an unanswered input-required task stops its
            // driver after the configured window and stays hidden.
            let dormant_due = matches!(&task_snapshot, Some((_, state_name, false, _)) if state_name == "input-required")
                && pending_oldest
                    .is_some_and(|created| now_secs() - created > DORMANT_AFTER);
            if dormant_due {
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    if let Some(task) = ctx.task.as_mut() {
                        task.dormant = true;
                    }
                    if let Some(handle) = ctx.handle.take() {
                        handle.cancel();
                    }
                    ctx.connected = false;
                    ctx._wake_rx = None;
                }
                if let Some((task_id, ..)) = task_snapshot {
                    state.store.update_task_dormant(&context_id, task_id, true);
                }
                log_line(
                    &ensure_home().unwrap_or_default(),
                    &format!("context {context_id} went dormant"),
                );
                continue;
            }
            // Idle driver (§7.4): a terminal task keeps its driver briefly.
            let idle_due = matches!(&task_snapshot, Some((_, state_name, _, Some(finished_at))) if
                matches!(state_name.as_str(), "completed" | "failed" | "canceled")
                    && now_secs() - finished_at > DRIVER_IDLE_KEEP);
            if idle_due {
                if let Some(ctx) = state.ctxs.get_mut(&context_id) {
                    if ctx.handle.is_some() {
                        ctx.handle = None;
                        ctx.connected = false;
                        ctx._wake_rx = None;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// params

fn bad(message: &str) -> rpc::RpcError {
    rpc::RpcError {
        message: message.to_owned(),
        code: "invalid-params".into(),
    }
}

fn param_context(params: &Value) -> Result<String, rpc::RpcError> {
    params
        .get("contextId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| bad("contextId is required"))
}

fn ctx_mut<'a>(state: &'a mut DaemonState, context_id: &str) -> Result<&'a mut Ctx, rpc::RpcError> {
    state.ctxs.get_mut(context_id).ok_or_else(|| rpc::RpcError {
        message: format!("unknown context {context_id}"),
        code: rpc::codes::UNKNOWN_CONTEXT.into(),
    })
}

/// Re-record the caller's owner on every send/wait (§7.1) so a restarted
/// harness takes the context over.
fn owner_refresh(state: &mut DaemonState, params: &Value) -> Result<(), rpc::RpcError> {
    let Some(owner_value) = params.get("owner") else {
        return Ok(());
    };
    let Some(pid) = owner_value.get("pid").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Some(start) = owner_value.get("start").and_then(Value::as_str) else {
        return Ok(());
    };
    if pid == 0 {
        return Ok(());
    }
    let context_id = param_context(params)?;
    let ctx = ctx_mut(state, &context_id)?;
    ctx.owner = Some(crate::owner::Owner {
        pid: pid as i32,
        start: start.to_owned(),
    });
    ctx.owner_dead_since = None;
    state.store.update_owner(&context_id, pid, start);
    Ok(())
}

fn parse_index(value: &str) -> Option<usize> {
    value.parse::<usize>().ok().filter(|index| *index >= 1)
}
