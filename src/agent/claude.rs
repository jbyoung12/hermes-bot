use crate::agent::protocol::{self, ContentBlock, StreamMessage};
use crate::agent::{Agent, AgentEvent, AgentHandle};
use crate::error::{HermesError, Result};
use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

pub struct ClaudeAgent;

impl ClaudeAgent {
    fn build_command(
        repo_path: &Path,
        allowed_tools: &[String],
        system_prompt: Option<&str>,
        resume_session_id: Option<&str>,
        model: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new("claude");
        cmd.current_dir(repo_path);

        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--input-format").arg("stream-json");
        cmd.arg("--verbose");

        if let Some(sid) = resume_session_id {
            cmd.arg("--resume").arg(sid);
        }

        // Only pass --model for new sessions; the CLI remembers for --resume.
        if resume_session_id.is_none()
            && let Some(m) = model
        {
            cmd.arg("--model").arg(m);
        }

        for tool in allowed_tools {
            cmd.arg("--allowedTools").arg(tool);
        }

        if let Some(sp) = system_prompt {
            cmd.arg("--append-system-prompt").arg(sp);
        }

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        cmd
    }
}

#[async_trait]
impl Agent for ClaudeAgent {
    async fn spawn(
        &self,
        repo_path: &Path,
        allowed_tools: &[String],
        system_prompt: Option<&str>,
        resume_session_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<AgentHandle> {
        let mut cmd = Self::build_command(
            repo_path,
            allowed_tools,
            system_prompt,
            resume_session_id,
            model,
        );
        debug!("Spawning claude CLI: {:?}", cmd);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HermesError::ClaudeNotFound
            } else {
                HermesError::AgentSpawnFailed {
                    reason: e.to_string(),
                }
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HermesError::AgentSpawnFailed {
                reason: "stdin was not piped".into(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HermesError::AgentSpawnFailed {
                reason: "stdout was not piped".into(),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| HermesError::AgentSpawnFailed {
                reason: "stderr was not piped".into(),
            })?;

        // Channels: agent events (out) and user messages (in).
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        let (user_tx, mut user_rx) = mpsc::channel::<String>(64);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        // ── Stdout reader task ────────────────────────────────────────
        let event_tx_stdout = event_tx.clone();
        let mut stdin_writer = stdin;
        // Shared mpsc for stdin writes (user messages + control responses).
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(64);

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let msg = match protocol::parse_line(&line) {
                    Some(m) => m,
                    None => continue,
                };

                match msg {
                    StreamMessage::System(sys) => {
                        if sys.subtype.as_deref() == Some("init") {
                            let session_id = sys.session_id.unwrap_or_default();
                            let model = sys.model.unwrap_or_default();
                            info!("Claude session init: id={}, model={}", session_id, model);
                            let _ = event_tx_stdout
                                .send(AgentEvent::SessionInit { session_id, model })
                                .await;
                        }
                    }
                    StreamMessage::Assistant(assistant) => {
                        if let Some(body) = assistant.message {
                            for block in body.content {
                                match block {
                                    ContentBlock::Text { text } => {
                                        let _ = event_tx_stdout.send(AgentEvent::Text(text)).await;
                                    }
                                    ContentBlock::ToolUse { name, input, .. } => {
                                        let _ = event_tx_stdout
                                            .send(AgentEvent::ToolUse { name, input })
                                            .await;
                                    }
                                    ContentBlock::ToolResult { .. }
                                    | ContentBlock::Thinking { .. }
                                    | ContentBlock::Unknown => {}
                                }
                            }
                        }
                    }
                    StreamMessage::Result(result) => {
                        let _ = event_tx_stdout
                            .send(AgentEvent::TurnComplete {
                                result: result.result,
                                subtype: result.subtype.unwrap_or_else(|| "success".to_string()),
                                num_turns: result.num_turns.unwrap_or(0),
                                duration_ms: result.duration_ms.unwrap_or(0),
                                is_error: result.is_error.unwrap_or(false),
                                session_id: result.session_id.unwrap_or_default(),
                            })
                            .await;
                    }
                    StreamMessage::ControlRequest(ctrl) => {
                        if let Some(request_id) = ctrl.request_id {
                            let tool_name = ctrl
                                .request
                                .as_ref()
                                .and_then(|r| r.tool_name.as_deref())
                                .unwrap_or("unknown");

                            if tool_name == "AskUserQuestion" {
                                // Forward the question to Slack instead of denying.
                                let questions = ctrl
                                    .request
                                    .as_ref()
                                    .and_then(|r| r.tool_input.clone())
                                    .unwrap_or_default();
                                info!(
                                    "AskUserQuestion control_request (request_id={})",
                                    request_id
                                );
                                let _ = event_tx_stdout
                                    .send(AgentEvent::QuestionPending {
                                        request_id,
                                        questions,
                                    })
                                    .await;
                            } else {
                                // Forward unapproved tool requests to Slack for
                                // interactive approval (mirrors the AskUserQuestion flow).
                                info!(
                                    "Tool approval requested: {} (request_id={})",
                                    tool_name, request_id
                                );
                                let tool_input = ctrl
                                    .request
                                    .as_ref()
                                    .and_then(|r| r.tool_input.clone())
                                    .unwrap_or_default();
                                let _ = event_tx_stdout
                                    .send(AgentEvent::ToolApprovalPending {
                                        request_id,
                                        tool_name: tool_name.to_string(),
                                        tool_input,
                                    })
                                    .await;
                            }
                        }
                    }
                    StreamMessage::ToolProgress(tp) => {
                        let tool_name = tp.tool_name.unwrap_or_default();
                        let _ = event_tx_stdout
                            .send(AgentEvent::ToolProgress { tool_name })
                            .await;
                    }
                    StreamMessage::User(_)
                    | StreamMessage::StreamEvent(_)
                    | StreamMessage::Unknown => {}
                }
            }

            // Stdout closed — process likely exited.
            debug!("Claude stdout reader finished");
        });

        // ── Stdin writer task ─────────────────────────────────────────
        let stdin_tx_user = stdin_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = user_rx.recv().await {
                match protocol::user_message(&msg, None) {
                    Ok(json) => {
                        let _ = stdin_tx_user.send(json).await;
                    }
                    Err(e) => {
                        error!("Failed to serialize user message: {}", e);
                        // Continue processing other messages instead of crashing.
                    }
                }
            }
        });

        // ── Consolidated stdin writer ─────────────────────────────────
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if let Err(e) = stdin_writer.write_all(line.as_bytes()).await {
                    warn!("Failed to write to claude stdin: {}", e);
                    break;
                }
                if let Err(e) = stdin_writer.write_all(b"\n").await {
                    warn!("Failed to write newline to claude stdin: {}", e);
                    break;
                }
                if let Err(e) = stdin_writer.flush().await {
                    warn!("Failed to flush claude stdin: {}", e);
                    break;
                }
            }
        });

        // ── Stderr drainer ────────────────────────────────────────────
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!("claude stderr: {}", line);
            }
        });

        // ── Kill listener + process wait ──────────────────────────────
        let event_tx_exit = event_tx;
        tokio::spawn(async move {
            tokio::select! {
                _ = kill_rx => {
                    info!("Kill signal received, terminating claude process");
                    let _ = child.kill().await;
                }
                status = child.wait() => {
                    match status {
                        Ok(s) => {
                            let code = s.code();
                            debug!("Claude process exited with code: {:?}", code);
                            let _ = event_tx_exit.send(AgentEvent::ProcessExited { code }).await;
                        }
                        Err(e) => {
                            error!("Error waiting for claude process: {}", e);
                            let _ = event_tx_exit.send(AgentEvent::ProcessExited { code: None }).await;
                        }
                    }
                }
            }
        });

        Ok(AgentHandle {
            sender: user_tx,
            receiver: event_rx,
            kill_tx: Some(kill_tx),
            session_id: None,
            stdin_tx,
        })
    }
}
