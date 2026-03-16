// Protocol types are deserialized from JSON; some fields exist for completeness
// even when not directly read by Hermes.

use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

// ── Inbound messages (Claude CLI stdout → Hermes) ─────────────────────

/// Top-level message from Claude CLI stdout (NDJSON stream-json protocol).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    System(SystemMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    Result(ResultMessage),
    #[serde(rename = "user")]
    #[allow(dead_code)]
    User(UserMessage),
    #[serde(rename = "stream_event")]
    #[allow(dead_code)]
    StreamEvent(StreamEventMessage),
    #[serde(rename = "tool_progress")]
    ToolProgress(ToolProgressMessage),
    #[serde(rename = "control_request")]
    ControlRequest(ControlRequestMessage),
    #[serde(other)]
    Unknown,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct SystemMessage {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub message: Option<AssistantMessageBody>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct AssistantMessageBody {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ResultMessage {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub num_turns: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub duration_api_ms: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct UserMessage {
    #[serde(default)]
    pub message: Option<Value>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct StreamEventMessage {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ToolProgressMessage {
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ControlRequestMessage {
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub request: Option<ControlRequestBody>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ControlRequestBody {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(flatten)]
    pub data: Value,
}

// ── Content blocks within assistant messages ──────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<Value>,
        #[serde(default)]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
    },
    #[serde(other)]
    Unknown,
}

// ── Outbound messages (Hermes → Claude CLI stdin) ─────────────────────

/// Build a user message JSON line to write to stdin.
/// Returns an error if serialization fails (should be extremely rare).
pub fn user_message(prompt: &str, session_id: Option<&str>) -> Result<String, serde_json::Error> {
    let mut msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": prompt
        }
    });
    if let Some(sid) = session_id {
        msg["session_id"] = serde_json::json!(sid);
    }
    serde_json::to_string(&msg)
}

/// Build a control response JSON line to write to stdin (e.g. tool approval).
/// Returns an error if serialization fails (should be extremely rare).
pub fn control_response(request_id: &str, response: Value) -> Result<String, serde_json::Error> {
    let msg = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response
        }
    });
    serde_json::to_string(&msg)
}

/// Build a tool-denial control response (reject unapproved tools).
/// Returns a hardcoded fallback if serialization fails (should never happen for this simple JSON).
pub fn deny_tool(request_id: &str) -> String {
    control_response(request_id, serde_json::json!({ "behavior": "deny" }))
        .unwrap_or_else(|e| {
            warn!("Failed to serialize deny_tool response: {}", e);
            // Hardcoded fallback (should never be needed).
            format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{}","response":{{"behavior":"deny"}}}}}}"#,
                request_id
            )
        })
}

/// Build a tool-approval control response (allow unapproved tools after user confirmation).
pub fn approve_tool(request_id: &str) -> String {
    control_response(request_id, serde_json::json!({ "behavior": "allow" })).unwrap_or_else(|e| {
        warn!("Failed to serialize approve_tool response: {}", e);
        // Fallback: deny if serialization fails (should never happen).
        deny_tool(request_id)
    })
}

/// Build a control response that approves an AskUserQuestion request
/// and injects the user's answer via `updated_input`.
pub fn answer_question(request_id: &str, questions: &Value, answer_text: &str) -> String {
    // Build answers map: map each question index to the user's reply text.
    let mut answers = serde_json::Map::new();
    if let Some(arr) = questions.get("questions").and_then(|q| q.as_array()) {
        for (i, _) in arr.iter().enumerate() {
            answers.insert(i.to_string(), Value::String(answer_text.to_string()));
        }
    } else {
        // Single question or unknown shape — use "0" as the key.
        answers.insert("0".to_string(), Value::String(answer_text.to_string()));
    }

    let mut updated_input = questions.clone();
    if let Some(obj) = updated_input.as_object_mut() {
        obj.insert("answers".to_string(), Value::Object(answers));
    }

    control_response(
        request_id,
        serde_json::json!({
            "behavior": "allow",
            "updated_input": updated_input
        }),
    )
    .unwrap_or_else(|e| {
        warn!("Failed to serialize answer_question response: {}", e);
        // Fallback: deny the question if we can't serialize the answer.
        deny_tool(request_id)
    })
}

// ── Parser ────────────────────────────────────────────────────────────

/// Parse a single NDJSON line from Claude CLI stdout.
/// Returns `None` for empty/malformed lines (logged as warnings).
pub fn parse_line(line: &str) -> Option<StreamMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<StreamMessage>(trimmed) {
        Ok(msg) => Some(msg),
        Err(e) => {
            warn!(
                "Failed to parse stream-json line: {}. Line: {}",
                e,
                &trimmed[..crate::util::floor_char_boundary(trimmed, 200)]
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_system_init() {
        let line =
            r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-sonnet"}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::System(sys) => {
                assert_eq!(sys.subtype.as_deref(), Some("init"));
                assert_eq!(sys.session_id.as_deref(), Some("abc-123"));
                assert_eq!(sys.model.as_deref(), Some("claude-sonnet"));
            }
            other => panic!("Expected System, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_assistant_text() {
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::Assistant(a) => {
                let body = a.message.unwrap();
                assert_eq!(body.content.len(), 1);
                match &body.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
                    other => panic!("Expected Text, got {:?}", other),
                }
            }
            other => panic!("Expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_assistant_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/tmp/test"}}]}}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::Assistant(a) => {
                let body = a.message.unwrap();
                match &body.content[0] {
                    ContentBlock::ToolUse { id, name, input } => {
                        assert_eq!(id, "t1");
                        assert_eq!(name, "Read");
                        assert_eq!(input["file_path"], "/tmp/test");
                    }
                    other => panic!("Expected ToolUse, got {:?}", other),
                }
            }
            other => panic!("Expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_result() {
        let line = r#"{"type":"result","subtype":"success","session_id":"s1","is_error":false,"result":"Done","num_turns":3,"duration_ms":1500}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::Result(r) => {
                assert_eq!(r.subtype.as_deref(), Some("success"));
                assert_eq!(r.session_id.as_deref(), Some("s1"));
                assert_eq!(r.is_error, Some(false));
                assert_eq!(r.result.as_deref(), Some("Done"));
                assert_eq!(r.num_turns, Some(3));
                assert_eq!(r.duration_ms, Some(1500));
            }
            other => panic!("Expected Result, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_control_request() {
        let line = r#"{"type":"control_request","request_id":"req1","request":{"subtype":"tool_use","tool_name":"Bash"}}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::ControlRequest(c) => {
                assert_eq!(c.request_id.as_deref(), Some("req1"));
                let body = c.request.unwrap();
                assert_eq!(body.tool_name.as_deref(), Some("Bash"));
            }
            other => panic!("Expected ControlRequest, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_tool_progress() {
        let line = r#"{"type":"tool_progress","tool_name":"Bash","tool_use_id":"t1"}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::ToolProgress(tp) => {
                assert_eq!(tp.tool_name.as_deref(), Some("Bash"));
                assert_eq!(tp.tool_use_id.as_deref(), Some("t1"));
            }
            other => panic!("Expected ToolProgress, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_type() {
        let line = r#"{"type":"some_future_type","data":42}"#;
        let msg = parse_line(line).unwrap();
        assert!(matches!(msg, StreamMessage::Unknown));
    }

    #[test]
    fn test_parse_empty_line() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn test_parse_malformed_json() {
        assert!(parse_line("{invalid json}").is_none());
    }

    #[test]
    fn test_user_message_without_session() {
        let msg = user_message("hello", None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["content"], "hello");
        assert!(parsed.get("session_id").is_none());
    }

    #[test]
    fn test_user_message_with_session() {
        let msg = user_message("hello", Some("s1")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["content"], "hello");
        assert_eq!(parsed["session_id"], "s1");
    }

    #[test]
    fn test_deny_tool() {
        let msg = deny_tool("req2");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["request_id"], "req2");
        assert_eq!(parsed["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn test_approve_tool() {
        let msg = approve_tool("req3");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["request_id"], "req3");
        assert_eq!(parsed["response"]["response"]["behavior"], "allow");
    }

    #[test]
    fn test_parse_thinking_block() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me think..."},{"type":"text","text":"Here is my answer"}]}}"#;
        let msg = parse_line(line).unwrap();
        match msg {
            StreamMessage::Assistant(a) => {
                let body = a.message.unwrap();
                assert_eq!(body.content.len(), 2);
                assert!(matches!(&body.content[0], ContentBlock::Thinking { .. }));
                assert!(matches!(&body.content[1], ContentBlock::Text { .. }));
            }
            other => panic!("Expected Assistant, got {:?}", other),
        }
    }
}
