//! Antigravity (agy) driver: one long-lived `agy` process per conversation,
//! speaking NDJSON on both stdin and stdout.
//!
//! The CLI's `--input-format stream-json --output-format stream-json` transport
//! is the streaming shape the Agent SDK drives on Claude Code, with agy's own
//! event vocabulary on top:
//!
//! - stdin accepts `{"event":"user","message":{...claude-style content...}}`
//!   lines, one turn per message.
//! - stdout emits `init` (once, with the conversation id), `step_update`
//!   (progress: user_input / agent_response / tool steps), and `result` (the
//!   settled turn, with status and usage).
//!
//! agy owns conversation ids: a fresh process mints a UUID the driver adopts
//! from `init`, and `--conversation <id>` resumes one. There is no
//! caller-chosen session id and no mid-stream permission channel, so the
//! rewind/fork machinery routes through a driver restart and Supervised mode
//! relies on `denied_actions` reporting after the fact.
//!
//! Stream shapes observed against agy 1.3.1:
//!
//! ```text
//! {"event":"init","conversation_id":"<uuid>","init":{...}}
//! {"event":"step_update","step_update":{"conversation_id":"…","step_index":0,
//!  "state":"ACTIVE","step_type":"user_input"}}
//! {"event":"step_update","step_update":{…,"step_type":"agent_response","text_delta":"…"}}
//! {"event":"step_update","step_update":{…,"step_type":"tool","tool_info":{
//!  "action_type":"execute_command","parameters":{…},"title":"…"}}}
//! {"event":"result","result":{"conversation_id":"…","status":"SUCCESS",
//!  "response":"…","usage":{"input_tokens":…,…}}}
//! ```

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use crossbeam_channel::unbounded;
use serde_json::{Value, json};

use crate::command_env;
use crate::driver::DriverEventSender;
use crate::model::{ActivityKind, DriverEvent, ProviderResumeCursor, RuntimeMode};

/// agy's only stream transport is print mode, whose permission requests have
/// no interactive channel. Read-only `--sandbox` covers Ask; every other mode
/// wants the work done, which here means skipping prompts.
fn permission_args(mode: RuntimeMode) -> &'static [&'static str] {
    if mode == RuntimeMode::Ask {
        &["--sandbox"]
    } else {
        &["--dangerously-skip-permissions"]
    }
}

/// Prompt payloads ride the same channel the writer drains; there is no other
/// control intent, so the channel type is the payload string itself.
type PromptChannel = crossbeam_channel::Sender<String>;

pub struct AntigravityDriver {
    prompts: PromptChannel,
    child: Arc<Mutex<Option<Child>>>,
}

impl AntigravityDriver {
    pub fn start(
        options: crate::driver::DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let crate::driver::DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            computer_use_enabled: _,
            provider_cursor,
            extra_env,
        } = options;
        let resume_conversation_id = match provider_cursor {
            Some(ProviderResumeCursor::Antigravity { conversation_id }) => {
                (!conversation_id.is_empty()).then_some(conversation_id)
            }
            Some(cursor) => {
                return Err(anyhow::anyhow!(
                    "cannot resume Antigravity from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };

        let mut command = command_env::command(&binary);
        command.current_dir(&cwd);
        command.args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Print mode without a prompt reads turns from stdin; 0 waits for
            // each turn to complete rather than timing out mid-turn.
            "--print-timeout",
            "0",
        ]);
        command.args(permission_args(mode));
        if let Some(model) = model.as_deref() {
            command.args(["--model", model]);
        }
        // agy spells effort low|medium|high|xhigh|max — the same vocabulary.
        if let Some(effort) = reasoning_effort.as_deref() {
            command.args(["--effort", effort]);
        }
        if let Some(conversation_id) = resume_conversation_id.as_deref() {
            command.args(["--conversation", conversation_id]);
        }
        for (name, value) in &extra_env {
            command.env(name, value);
        }
        let mut child = command_env::spawn(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .context("failed to start `agy` in streaming-input mode")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("agy stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("agy stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("agy stderr unavailable"))?;

        let child = Arc::new(Mutex::new(Some(child)));
        let (prompts, prompt_rx) = unbounded::<String>();

        // Until `init` names the conversation, a resume restart has nothing to
        // point at; the init handler below sends the authoritative cursor.
        let cursor = resume_conversation_id
            .clone()
            .map(|conversation_id| ProviderResumeCursor::Antigravity { conversation_id });
        let _ = events.send(DriverEvent::Connected {
            provider_cursor: cursor,
        });

        // Stdin writer: serialized prompt lines.
        thread::spawn(move || {
            let mut stdin = stdin;
            for payload in prompt_rx {
                if writeln!(stdin, "{payload}").is_err() || stdin.flush().is_err() {
                    break;
                }
            }
            // Dropping stdin signals EOF, which ends print mode cleanly.
        });

        // Stdout reader: the event vocabulary lives here.
        let reader_events = events.clone();
        let reader_child = Arc::clone(&child);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                match frame.get("event").and_then(Value::as_str) {
                    Some("init") => handle_init(&frame, &reader_events),
                    Some("step_update") => handle_step_update(&frame, &reader_events),
                    Some("result") => handle_result(&frame, &reader_events),
                    _ => {}
                }
            }
            // Reap the process so no zombie outlives the session.
            if let Some(mut child) = reader_child.lock().unwrap().take() {
                let _ = child.wait();
            }
            let _ = reader_events.send(DriverEvent::ProcessExited);
        });

        // Stderr is diagnostic only; drain it so the pipe never fills.
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                line.clear();
            }
        });

        Ok(Self { prompts, child })
    }
}

fn handle_init(frame: &Value, events: &DriverEventSender) {
    let Some(conversation_id) = frame
        .get("conversation_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let _ = events.send(DriverEvent::Connected {
        provider_cursor: Some(ProviderResumeCursor::Antigravity {
            conversation_id: conversation_id.to_owned(),
        }),
    });
}

fn handle_step_update(frame: &Value, events: &DriverEventSender) {
    let Some(step) = frame.get("step_update") else {
        return;
    };
    let step_type = step
        .get("step_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state = step
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let complete = state == "DONE";
    match step_type {
        "agent_response" => {
            if let Some(delta) = step.get("text_delta").and_then(Value::as_str) {
                let _ = events.send(DriverEvent::TextDelta(delta.to_owned()));
            }
        }
        "tool" => {
            let info = step.get("tool_info").cloned().unwrap_or_else(|| json!({}));
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| info.get("action_type").and_then(Value::as_str))
                .unwrap_or("tool")
                .to_owned();
            let _ = events.send(DriverEvent::Activity {
                id: step.get("step_index").map(|index| index.to_string()),
                kind: ActivityKind::Tool,
                title,
                detail: Some(info.to_string()),
                complete,
            });
        }
        // user_input echoes the prompt the daemon already surfaced through
        // PromptSubmitted, so the echo needs no transcript entry.
        "user_input" => {}
        // A step type this driver predates degrades to a generic tool
        // activity; the turn still settles on `result`.
        other => {
            let _ = events.send(DriverEvent::Activity {
                id: step.get("step_index").map(|index| index.to_string()),
                kind: ActivityKind::Tool,
                title: format!("unsupported step type: {other}"),
                detail: Some(step.to_string()),
                complete,
            });
        }
    }
}

fn handle_result(frame: &Value, events: &DriverEventSender) {
    let Some(result) = frame.get("result") else {
        return;
    };
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    let success = status == "SUCCESS";
    if let Some(usage) = result.get("usage") {
        let _ = events.send(DriverEvent::UsageUpdated {
            context_tokens: usage.get("input_tokens").and_then(Value::as_u64),
            context_window: None,
        });
    }
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        if !error.is_empty() {
            let _ = events.send(DriverEvent::Error(error.to_owned()));
        }
    }
    // The stream transport serves many turns per process: the result settles
    // one turn, and the next stdin message opens the next. Only closed stdout
    // ends the loop.
    let summary = result
        .get("response")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|response| !response.is_empty())
        .map(str::to_owned);
    let _ = events.send(DriverEvent::TurnFinished { success, summary });
}

impl crate::driver::DriverControl for AntigravityDriver {
    fn prompt(&self, prompt: String) {
        let payload = json!({
            "event": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": prompt}],
            },
        });
        let _ = self.prompts.send(payload.to_string());
    }

    fn supports_steer(&self) -> bool {
        // A queued stdin message lands after the running turn settles, which
        // is exactly Waku's queue-then-prompt fallback.
        false
    }

    fn cancel(&self) {
        // No stdin control channel: kill the process. The reader thread's
        // ProcessExited marks the session dead, and a resume restarts it.
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
        }
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        anyhow::bail!("conversation rollback is not supported by Antigravity")
    }
}

impl Drop for AntigravityDriver {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_args_match_runtime_mode() {
        assert_eq!(permission_args(RuntimeMode::Ask), &["--sandbox"]);
        assert_eq!(
            permission_args(RuntimeMode::FullAccess),
            &["--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn result_success_finishes_the_turn_without_closing_the_stream() {
        let (_wake, _wakes) = smol::channel::bounded::<()>(1);
        let (events, received) = crate::driver::test_event_channel();
        let frame = json!({
            "event": "result",
            "result": {
                "conversation_id": "abc",
                "status": "SUCCESS",
                "response": "done",
                "usage": {"input_tokens": 10, "output_tokens": 2},
            },
        });
        handle_result(&frame, &events);
        assert!(matches!(
            received.try_recv(),
            Ok(DriverEvent::UsageUpdated {
                context_tokens: Some(10),
                ..
            })
        ));
        assert!(matches!(
            received.try_recv(),
            Ok(DriverEvent::TurnFinished { success: true, .. })
        ));
    }

    #[test]
    fn init_publishes_the_conversation_cursor() {
        let (_wake, _wakes) = smol::channel::bounded::<()>(1);
        let (events, received) = crate::driver::test_event_channel();
        let frame = json!({
            "event": "init",
            "conversation_id": "abc-123",
        });
        handle_init(&frame, &events);
        match received.try_recv() {
            Ok(DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Antigravity { conversation_id }),
            }) => assert_eq!(conversation_id, "abc-123"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn tool_steps_become_activities_and_responses_become_deltas() {
        let (_wake, _wakes) = smol::channel::bounded::<()>(1);
        let (events, received) = crate::driver::test_event_channel();
        handle_step_update(
            &json!({
                "event": "step_update",
                "step_update": {
                    "step_index": 3,
                    "state": "ACTIVE",
                    "step_type": "tool",
                    "tool_info": {"action_type": "run_command", "title": "ls"},
                },
            }),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(DriverEvent::Activity {
                kind: ActivityKind::Tool,
                complete: false,
                ..
            })
        ));
        handle_step_update(
            &json!({
                "event": "step_update",
                "step_update": {
                    "step_index": 4,
                    "state": "DONE",
                    "step_type": "agent_response",
                    "text_delta": "hi",
                },
            }),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(DriverEvent::TextDelta(text)) if text == "hi"
        ));
    }
}
