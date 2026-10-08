//! `wk` — the stateless CLI client (§5 of the spec) plus the `daemon`
//! subcommands. Everything the CLI knows comes from the daemon over the
//! unix socket; the only local state is `$WK_HOME`.

mod daemon;
mod owner;
mod process;
mod rpc;
mod store;

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::rpc::{WaitEnd, codes};

const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_DAEMON: i32 = 3;
const EXIT_WORKING: i32 = 12;
const EXIT_REJECTED: i32 = 13;
const EXIT_CANCELED: i32 = 130;

static SIGNALED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SIGNALED.store(true, Ordering::SeqCst);
}

fn install_signals() {
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as usize;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

// ---------------------------------------------------------------------------
// argument parsing

struct Args {
    positional: Vec<String>,
    flags: HashMap<String, String>,
    repeated: HashMap<String, Vec<String>>,
    switches: Vec<String>,
}

const VALUE_FLAGS: &[&str] = &[
    "cwd",
    "model",
    "effort",
    "mode",
    "on-question",
    "prompt-file",
    "timeout",
    "owner",
    "reply",
    "answer",
];

const SWITCHES: &[&str] = &["json", "all"];

fn parse_args(input: &[String]) -> Result<Args, String> {
    let mut args = Args {
        positional: Vec::new(),
        flags: HashMap::new(),
        repeated: HashMap::new(),
        switches: Vec::new(),
    };
    let mut iter = input.iter();
    while let Some(arg) = iter.next() {
        let (name, inline) = match arg.strip_prefix("--") {
            Some(rest) => match rest.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (rest.to_owned(), None),
            },
            None => {
                args.positional.push(arg.clone());
                continue;
            }
        };
        if SWITCHES.contains(&name.as_str()) {
            args.switches.push(name);
            continue;
        }
        if VALUE_FLAGS.contains(&name.as_str()) {
            let value = match inline {
                Some(value) => value,
                None => iter.next().ok_or(format!("--{name} needs a value"))?.clone(),
            };
            if name == "answer" {
                args.repeated.entry(name).or_default().push(value);
            } else {
                args.flags.insert(name, value);
            }
            continue;
        }
        return Err(format!("unknown option --{name}"));
    }
    Ok(args)
}

impl Args {
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    fn has_switch(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }
}

const USAGE: &str = "\
usage:
  wk run <provider> [--cwd DIR] [--model M] [--effort E] --mode MODE
        [--on-question yield|fail|auto] [--prompt-file F|-] [PROMPT]
        [--owner PID] [--timeout SECS] [--json]
  wk send <ctx> [--prompt-file F|-] [PROMPT]
  wk send <ctx> --reply <req> [VALUE] [--answer QID=VALUE ...]
  wk wait <ctx> [--timeout SECS] [--json]
  wk get <ctx> [--json]
  wk result <ctx> [--task T]
  wk pending <ctx> [--json]
  wk cancel <ctx> [--json]
  wk close <ctx>
  wk list [--all] [--json]
  wk daemon run|start|stop|status

global: --json prints the TaskStatus JSON object instead of text output;
--timeout SECS stops waiting at the deadline and exits 12 while the task
keeps running.
";

fn usage_error(message: &str) -> i32 {
    eprintln!("[wk] error: {message}");
    eprint!("{USAGE}");
    EXIT_USAGE
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let code = match raw.first() {
        None => usage_error("a command is required"),
        Some(command) => dispatch(command, &raw[1..]),
    };
    ExitCode::from(code as u8)
}

fn dispatch(command: &str, rest: &[String]) -> i32 {
    let args = match parse_args(rest) {
        Ok(args) => args,
        Err(message) => return usage_error(&message),
    };
    match command {
        "run" => cmd_run(&args),
        "send" => cmd_send(&args),
        "wait" => cmd_wait(&args),
        "get" => cmd_get(&args),
        "result" => cmd_result(&args),
        "pending" => cmd_pending(&args),
        "cancel" => cmd_cancel(&args),
        "close" => cmd_close(&args),
        "list" => cmd_list(&args),
        "daemon" => cmd_daemon(&args),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            EXIT_OK
        }
        other => usage_error(&format!("unknown command `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// daemon connection (§13: auto-start on the first failed connect)

fn socket_connect() -> anyhow::Result<PathBuf> {
    let path = process::socket_path(&process::wk_home());
    UnixStream::connect(&path)?;
    Ok(path)
}

/// Connect, or bring the daemon up (detached) and wait for its socket.
fn ensure_daemon() -> anyhow::Result<PathBuf> {
    match socket_connect() {
        Ok(path) => Ok(path),
        Err(_) => {
            process::ensure_home()?;
            process::spawn_detached_daemon()?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match socket_connect() {
                    Ok(path) => return Ok(path),
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(error) => {
                        anyhow::bail!("daemon did not come up within 3s: {error}")
                    }
                }
            }
        }
    }
}

fn call(path: &PathBuf, method: &str, params: Value) -> Result<Value, (String, String)> {
    let (raw, _) = rpc::call(path, method, params).map_err(|error| {
        (
            error.to_string(),
            "internal".to_owned(),
        )
    })?;
    rpc::split_response(raw).map_err(|error| (error.message, error.code))
}

// ---------------------------------------------------------------------------
// shared pieces

fn owner_param(args: &Args) -> Value {
    let owner = match args.flag("owner").and_then(|pid| pid.parse::<i32>().ok()) {
        Some(pid) => owner::from_pid(pid),
        None => owner::find_owner(std::process::id() as i32),
    };
    match owner {
        Some(owner) => json!({ "pid": owner.pid, "start": owner.start }),
        None => Value::Null,
    }
}

fn timeout_of(args: &Args) -> Result<Option<Duration>, String> {
    match args.flag("timeout") {
        None => Ok(None),
        Some(secs) => secs
            .parse::<f64>()
            .ok()
            .filter(|secs| *secs > 0.0)
            .map(Duration::from_secs_f64)
            .map(Some)
            .ok_or_else(|| "--timeout needs a positive number of seconds".to_owned()),
    }
}

/// PROMPT positional, `--prompt-file F`, or `--prompt-file -` for stdin.
fn prompt_of(args: &Args, position: usize) -> Result<String, String> {
    if let Some(file) = args.flag("prompt-file") {
        if args.positional.get(position).is_some() {
            return Err("PROMPT and --prompt-file are mutually exclusive".to_owned());
        }
        if file == "-" {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("reading stdin failed: {error}"))?;
            return Ok(text);
        }
        return std::fs::read_to_string(file)
            .map_err(|error| format!("reading {file} failed: {error}"));
    }
    match args.positional.get(position) {
        Some(prompt) => Ok(prompt.clone()),
        None => Err("a prompt is required (PROMPT or --prompt-file)".to_owned()),
    }
}

fn json_mode(args: &Args) -> bool {
    args.has_switch("json")
}

fn exit_code_for(status: &Value) -> i32 {
    let state = status.get("state").and_then(Value::as_str).unwrap_or("idle");
    match state {
        "completed" | "idle" => EXIT_OK,
        "failed" => EXIT_FAILED,
        "canceled" => EXIT_CANCELED,
        "input-required" => {
            let permission = status
                .get("pending")
                .and_then(Value::as_array)
                .is_some_and(|pending| {
                    pending
                        .iter()
                        .any(|request| request.get("kind").and_then(Value::as_str) == Some("permission"))
                });
            if permission {
                11
            } else {
                10
            }
        }
        "working" | "submitted" => EXIT_WORKING,
        _ => EXIT_OK,
    }
}

fn rpc_exit_code(code: &str) -> i32 {
    match code {
        codes::REJECTED => EXIT_REJECTED,
        "internal" => EXIT_DAEMON,
        _ => EXIT_USAGE,
    }
}

/// Print the stderr status block (§5.4). Returns nothing; always ends with
/// the `next:` line.
fn print_status_block(status: &Value, note: Option<&str>) {
    let context_id = status.get("contextId").and_then(Value::as_str).unwrap_or("?");
    let state = status.get("state").and_then(Value::as_str).unwrap_or("idle");
    let mut first = format!("[wk] ctx={context_id}");
    if let Some(task) = status.get("taskId").and_then(Value::as_str) {
        first.push_str(&format!(" task={task}"));
    }
    first.push_str(&format!(" state={state}"));
    if let Some(note) = note {
        first.push_str(&format!(" {note}"));
    }
    if status.get("dormant").and_then(Value::as_bool) == Some(true) {
        first.push_str(" dormant");
    }
    if state == "input-required" {
        if let Some(kind) = status.get("inputKind").and_then(Value::as_str) {
            first.push_str(&format!(" kind={kind}"));
        }
    }
    eprintln!("{first}");
    if let Some(error) = status.get("error").and_then(Value::as_str) {
        if !error.is_empty() {
            eprintln!("[wk] error: {error}");
        }
    }
    let mut truncated = false;
    if let Some(pending) = status.get("pending").and_then(Value::as_array) {
        for request in pending {
            let id = request.get("id").and_then(Value::as_str).unwrap_or("?");
            let (summary, over) = request_summary(request);
            truncated |= over;
            eprintln!("[wk] {id}: {summary}");
        }
    }
    if truncated {
        eprintln!("[wk] full: wk pending {context_id}");
    }
    let next = status
        .get("next")
        .and_then(Value::as_str)
        .unwrap_or("wk wait <?>");
    eprintln!("[wk] next: {next}");
}

const SUMMARY_LIMIT: usize = 300;

/// One-line summary of a pending request, truncated at 300 characters
/// (§5.4). The bool reports whether truncation happened.
fn request_summary(request: &Value) -> (String, bool) {
    let payload = request.get("payload").cloned().unwrap_or(Value::Null);
    let summary = match request.get("kind").and_then(Value::as_str) {
        Some("question") => {
            let parts: Vec<String> = payload
                .as_array()
                .map(|questions| questions.iter().map(question_summary).collect())
                .unwrap_or_default();
            parts.join(" | ")
        }
        Some("permission") => {
            let title = payload.get("title").and_then(Value::as_str).unwrap_or("");
            let options = payload
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option.get("id").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if options.is_empty() {
                title.to_owned()
            } else {
                format!("{title} options: {options}")
            }
        }
        _ => payload.to_string(),
    };
    let over = summary.chars().count() > SUMMARY_LIMIT;
    if !over {
        return (summary, false);
    }
    let cut: String = summary.chars().take(SUMMARY_LIMIT - 3).collect();
    (format!("{cut}..."), true)
}

fn question_summary(question: &Value) -> String {
    let text = question.get("question").and_then(Value::as_str).unwrap_or("");
    let options = question.get("options").and_then(Value::as_array);
    match options {
        Some(options) if !options.is_empty() => {
            let labels: Vec<String> = options
                .iter()
                .enumerate()
                .filter_map(|(index, option)| {
                    option
                        .get("label")
                        .and_then(Value::as_str)
                        .map(|label| format!("{}) {}", index + 1, label))
                })
                .collect();
            format!("{} {} (或自由回答)", text, labels.join(" "))
        }
        _ => text.to_owned(),
    }
}

/// The final turn of a waiting command: emit output and the exit code (§5.4).
fn finish(status: Value, json: bool, path: &PathBuf) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".to_owned())
        );
        return exit_code_for(&status);
    }
    let state = status.get("state").and_then(Value::as_str).unwrap_or("");
    if state == "completed" {
        let text = call(path, "tasks/result", json!({ "contextId": status.get("contextId") }))
            .map(|result| {
                result
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            })
            .unwrap_or_default();
        print!("{text}");
        if !text.ends_with('\n') && !text.is_empty() {
            println!();
        }
    }
    print_status_block(&status, None);
    exit_code_for(&status)
}

// ---------------------------------------------------------------------------
// the waiting commands (§6): run / send / wait

/// Send one request on a waiter connection and block until the stop point.
fn wait_for_stop(
    path: &PathBuf,
    context_id: &str,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
    json: bool,
) -> i32 {
    install_signals();
    let mut waiter = match rpc::Waiter::connect(path) {
        Ok(waiter) => waiter,
        Err(error) => {
            eprintln!("[wk] error: cannot reach the daemon: {error}");
            return EXIT_DAEMON;
        }
    };
    if let Err(error) = waiter.send(1, method, params) {
        eprintln!("[wk] error: cannot reach the daemon: {error}");
        return EXIT_DAEMON;
    }
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut superseded = false;
    match waiter.wait(1, deadline, &SIGNALED, &mut superseded) {
        Ok(Ok(status)) => finish(status, json, path),
        Ok(Err(error)) => {
            eprintln!("[wk] error: {} ({})", error.message, error.code);
            rpc_exit_code(&error.code)
        }
        Err(WaitEnd::Superseded) => {
            // §6.1: a newer waiter took over; leave the task alone.
            eprintln!("[wk] ctx={context_id} state=working superseded");
            eprintln!("[wk] next: wk wait {context_id}");
            EXIT_WORKING
        }
        Err(WaitEnd::Timeout) => {
            // §6.3: the deadline passes without touching the task.
            eprintln!("[wk] ctx={context_id} state=working timeout");
            eprintln!("[wk] next: wk wait {context_id}");
            EXIT_WORKING
        }
        Err(WaitEnd::Signaled) => exit_on_signal(path, context_id, waiter),
        Err(WaitEnd::Closed) => {
            eprintln!("[wk] error: the daemon closed the connection");
            EXIT_DAEMON
        }
    }
}

/// §6.4: on SIGINT/SIGTERM/SIGHUP, cancel a running task (waiting for the
/// confirmation for at most 2s) and exit 130. A task already at a stop point
/// is left untouched.
fn exit_on_signal(path: &PathBuf, context_id: &str, mut waiter: rpc::Waiter) -> ! {
    let state = call(path, "tasks/get", json!({ "contextId": context_id }))
        .ok()
        .and_then(|status| {
            status
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if matches!(state.as_deref(), Some("working") | Some("submitted")) {
        SIGNALED.store(false, Ordering::SeqCst);
        if waiter
            .send(2, "tasks/cancel", json!({ "contextId": context_id }))
            .is_ok()
        {
            let _ = waiter.wait(
                2,
                Some(Instant::now() + Duration::from_secs(2)),
                &SIGNALED,
                &mut false,
            );
        }
        eprintln!("[wk] ctx={context_id} state=canceled interrupted");
    }
    std::process::exit(EXIT_CANCELED);
}

fn cmd_run(args: &Args) -> i32 {
    let timeout = match timeout_of(args) {
        Ok(timeout) => timeout,
        Err(message) => return usage_error(&message),
    };
    let provider = match args.positional.first() {
        Some(provider) => provider,
        None => return usage_error("wk run needs a provider"),
    };
    let mode = match args.flag("mode") {
        Some(mode) => mode,
        None => return usage_error("--mode is required on wk run (ask | auto-accept-edits | auto | full-access)"),
    };
    let prompt = match prompt_of(args, 1) {
        Ok(prompt) => prompt,
        Err(message) => return usage_error(&message),
    };
    let cwd = match args.flag("cwd") {
        Some(dir) => PathBuf::from(dir),
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => return usage_error(&format!("the current directory is unusable: {error}")),
        },
    };
    let cwd = match owner::normalize_cwd(&cwd) {
        Ok(cwd) => cwd,
        Err(error) => return usage_error(&error.to_string()),
    };
    let depth: i64 = std::env::var("WK_DEPTH")
        .ok()
        .and_then(|depth| depth.parse().ok())
        .unwrap_or(0)
        + 1;
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    let params = json!({
        "provider": provider,
        "cwd": cwd.display().to_string(),
        "model": args.flag("model"),
        "effort": args.flag("effort"),
        "mode": mode,
        "onQuestion": args.flag("on-question").unwrap_or("yield"),
        "owner": owner_param(args),
        "depth": depth,
    });
    let context_id = match call(&path, "contexts/create", params) {
        Ok(result) => result
            .get("contextId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Err((message, code)) => {
            eprintln!("[wk] error: {message} ({code})");
            return rpc_exit_code(&code);
        }
    };
    if context_id.is_empty() {
        eprintln!("[wk] error: the daemon returned no context id");
        return EXIT_DAEMON;
    }
    let params = json!({
        "contextId": context_id,
        "prompt": prompt,
        "wait": true,
        "owner": owner_param(args),
    });
    wait_for_stop(&path, &context_id, "message/send", params, timeout, json_mode(args))
}

fn cmd_send(args: &Args) -> i32 {
    let timeout = match timeout_of(args) {
        Ok(timeout) => timeout,
        Err(message) => return usage_error(&message),
    };
    let context_id = match args.positional.first() {
        Some(context_id) => context_id.clone(),
        None => return usage_error("wk send needs a context id"),
    };
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    let params = if let Some(request_id) = args.flag("reply") {
        let value = args.positional.get(1);
        let answers = args.repeated.get("answer");
        if value.is_none() && answers.is_none() {
            return usage_error("--reply needs a VALUE or at least one --answer QID=VALUE");
        }
        let mut reply = json!({ "requestId": request_id });
        if let Some(value) = value {
            reply["value"] = json!(value);
        }
        if let Some(answers) = answers {
            let pairs: Vec<Value> = answers
                .iter()
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| json!([key, value]))
                .collect();
            if pairs.len() != answers.len() {
                return usage_error("--answer expects QID=VALUE");
            }
            reply["answers"] = Value::Array(pairs);
        }
        json!({
            "contextId": context_id,
            "reply": reply,
            "wait": true,
            "owner": owner_param(args),
        })
    } else {
        let prompt = match prompt_of(args, 1) {
            Ok(prompt) => prompt,
            Err(message) => return usage_error(&message),
        };
        json!({
            "contextId": context_id,
            "prompt": prompt,
            "wait": true,
            "owner": owner_param(args),
        })
    };
    wait_for_stop(&path, &context_id, "message/send", params, timeout, json_mode(args))
}

fn cmd_wait(args: &Args) -> i32 {
    let timeout = match timeout_of(args) {
        Ok(timeout) => timeout,
        Err(message) => return usage_error(&message),
    };
    let context_id = match args.positional.first() {
        Some(context_id) => context_id.clone(),
        None => return usage_error("wk wait needs a context id"),
    };
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    let params = json!({
        "contextId": context_id,
        "owner": owner_param(args),
    });
    wait_for_stop(&path, &context_id, "tasks/resubscribe", params, timeout, json_mode(args))
}

// ---------------------------------------------------------------------------
// the non-blocking commands

fn cmd_get(args: &Args) -> i32 {
    one_shot_status(args, "tasks/get", json!({ "contextId": args.positional.first() }))
}

fn cmd_cancel(args: &Args) -> i32 {
    one_shot_status(args, "tasks/cancel", json!({ "contextId": args.positional.first() }))
}

fn one_shot_status(args: &Args, method: &str, params: Value) -> i32 {
    if args.positional.first().is_none() {
        return usage_error("a context id is required");
    }
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    match call(&path, method, params) {
        Ok(status) => {
            if json_mode(args) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".to_owned())
                );
                exit_code_for(&status)
            } else {
                print_status_block(&status, None);
                exit_code_for(&status)
            }
        }
        Err((message, code)) => {
            eprintln!("[wk] error: {message} ({code})");
            rpc_exit_code(&code)
        }
    }
}

fn cmd_result(args: &Args) -> i32 {
    let context_id = match args.positional.first() {
        Some(context_id) => context_id.clone(),
        None => return usage_error("wk result needs a context id"),
    };
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    let params = json!({
        "contextId": context_id,
        "taskId": args.flag("task"),
    });
    match call(&path, "tasks/result", params) {
        Ok(result) => {
            let text = result.get("text").and_then(Value::as_str).unwrap_or("");
            print!("{text}");
            if !text.is_empty() && !text.ends_with('\n') {
                println!();
            }
            EXIT_OK
        }
        Err((message, code)) => {
            eprintln!("[wk] error: {message} ({code})");
            rpc_exit_code(&code)
        }
    }
}

fn cmd_pending(args: &Args) -> i32 {
    let context_id = match args.positional.first() {
        Some(context_id) => context_id.clone(),
        None => return usage_error("wk pending needs a context id"),
    };
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    match call(&path, "tasks/pending", json!({ "contextId": context_id })) {
        Ok(result) => {
            let requests = result
                .get("requests")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if requests.is_empty() {
                eprintln!("[wk] no pending requests");
                return EXIT_OK;
            }
            for request in requests {
                let id = request.get("id").and_then(Value::as_str).unwrap_or("?");
                let kind = request.get("kind").and_then(Value::as_str).unwrap_or("?");
                let task = request.get("taskId").and_then(Value::as_str).unwrap_or("?");
                eprintln!("[wk] {id} ({kind}, task {task})");
                let payload = request.get("payload").cloned().unwrap_or(Value::Null);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
                );
            }
            EXIT_OK
        }
        Err((message, code)) => {
            eprintln!("[wk] error: {message} ({code})");
            rpc_exit_code(&code)
        }
    }
}

fn cmd_close(args: &Args) -> i32 {
    let context_id = match args.positional.first() {
        Some(context_id) => context_id.clone(),
        None => return usage_error("wk close needs a context id"),
    };
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    match call(&path, "contexts/close", json!({ "contextId": context_id })) {
        Ok(_) => {
            eprintln!("[wk] {context_id} closed");
            EXIT_OK
        }
        Err((message, code)) => {
            eprintln!("[wk] error: {message} ({code})");
            rpc_exit_code(&code)
        }
    }
}

fn cmd_list(args: &Args) -> i32 {
    let path = match ensure_daemon() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[wk] error: {error}");
            return EXIT_DAEMON;
        }
    };
    let params = json!({ "all": args.has_switch("all") });
    match call(&path, "contexts/list", params) {
        Ok(result) => {
            let contexts = result
                .get("contexts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if contexts.is_empty() {
                eprintln!("[wk] no contexts");
                return EXIT_OK;
            }
            if json_mode(args) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_owned())
                );
                return EXIT_OK;
            }
            for context in contexts {
                let id = context.get("id").and_then(Value::as_str).unwrap_or("?");
                let provider = context.get("provider").and_then(Value::as_str).unwrap_or("?");
                let state = context
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("idle");
                let task = context
                    .get("taskId")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let cwd = context.get("cwd").and_then(Value::as_str).unwrap_or("?");
                let mode = context.get("mode").and_then(Value::as_str).unwrap_or("?");
                let depth = context.get("depth").and_then(Value::as_i64).unwrap_or(0);
                let mut line = format!("{id} {provider} {state} {task} cwd={cwd} mode={mode} depth={depth}");
                if context.get("dormant").and_then(Value::as_bool) == Some(true) {
                    line.push_str(" dormant");
                }
                if context.get("closed").and_then(Value::as_bool) == Some(true) {
                    line.push_str(" closed");
                }
                println!("{line}");
            }
            EXIT_OK
        }
        Err((message, code)) => {
            eprintln!("[wk] error: {message} ({code})");
            rpc_exit_code(&code)
        }
    }
}

// ---------------------------------------------------------------------------
// daemon lifecycle (§13)

fn cmd_daemon(args: &Args) -> i32 {
    let sub = match args.positional.first() {
        Some(sub) => sub.as_str(),
        None => return usage_error("wk daemon needs run | start | stop | status"),
    };
    match sub {
        "run" => match daemon::run() {
            Ok(()) => EXIT_OK,
            Err(error) => {
                eprintln!("[wk] error: {error}");
                EXIT_DAEMON
            }
        },
        "start" => match socket_connect() {
            Ok(_) => {
                eprintln!("[wk] daemon already running");
                EXIT_OK
            }
            Err(_) => {
                if let Err(error) = process::ensure_home().and_then(|_| process::spawn_detached_daemon()) {
                    eprintln!("[wk] error: {error}");
                    return EXIT_DAEMON;
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if socket_connect().is_ok() {
                        eprintln!("[wk] daemon started");
                        return EXIT_OK;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                eprintln!("[wk] error: the daemon did not come up within 3s");
                EXIT_DAEMON
            }
        },
        "stop" => match socket_connect() {
            Ok(path) => match call(&path, "daemon/shutdown", json!({})) {
                Ok(_) => {
                    eprintln!("[wk] daemon stopped");
                    EXIT_OK
                }
                Err((message, _)) => {
                    eprintln!("[wk] error: {message}");
                    EXIT_DAEMON
                }
            },
            Err(_) => {
                eprintln!("[wk] daemon not running");
                EXIT_OK
            }
        },
        "status" => match socket_connect() {
            Ok(path) => match call(&path, "daemon/ping", json!({})) {
                Ok(result) => {
                    let version = result.get("version").cloned().unwrap_or(Value::Null);
                    eprintln!("[wk] daemon running (version {version})");
                    EXIT_OK
                }
                Err((message, _)) => {
                    eprintln!("[wk] error: {message}");
                    EXIT_DAEMON
                }
            },
            Err(_) => {
                eprintln!("[wk] daemon not running");
                EXIT_DAEMON
            }
        },
        other => usage_error(&format!("unknown daemon command `{other}`")),
    }
}
