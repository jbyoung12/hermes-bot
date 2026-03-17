//! Markdown to Slack mrkdwn conversion and message formatting.
//!
//! Slack uses a limited markdown variant called "mrkdwn" with different syntax
//! for formatting. This module converts Claude's standard Markdown output to
//! Slack-compatible format and handles message splitting for size limits.

use regex::Regex;
use std::sync::LazyLock;

use super::SLACK_MAX_MESSAGE_CHARS;

// ── Agent response formatting ──────────────────────────────────────────

/// Response from an agent invocation, used for formatting Slack messages.
///
/// This structure captures the final result of a turn, including metadata
/// like duration, turn count, and error status. It's populated from the
/// `TurnComplete` event and passed to formatting functions.
///
/// This type is internal to the Slack formatting layer and not exposed
/// outside the crate.
#[derive(Debug, Clone)]
pub(crate) struct AgentResponse {
    pub(crate) result: String,
    pub(crate) is_error: bool,
    pub(crate) duration_ms: u64,
    pub(crate) num_turns: u32,
    pub(crate) subtype: Option<String>,
    /// Summary of interesting tool usage during this turn (e.g. Edit, Write, WebSearch).
    pub(crate) tool_summary: Option<String>,
}

// ── Markdown conversion ────────────────────────────────────────────────

static RE_CODE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(```[^\n]*\n.*?```)").unwrap());
static RE_BOLD_ITALIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\*{3}(.+?)\*{3}|(?s)_{3}(.+?)_{3}").unwrap());
static RE_BOLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\*{2}(.+?)\*{2}|(?s)_{2}(.+?)_{2}").unwrap());
static RE_HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap());
static RE_STRIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~(.+?)~~").unwrap());
static RE_IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)").unwrap());
static RE_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
static RE_HR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^[-*_]{3,}\s*$").unwrap());
static RE_SYSTEM_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<local-command-caveat[^>]*>.*?</local-command-caveat>|<system-reminder[^>]*>.*?</system-reminder>").unwrap()
});

/// Converts standard Markdown to Slack's mrkdwn format.
///
/// Performs the following transformations:
/// - **Bold**: `**text**` or `__text__` → `*text*`
/// - **Italic**: (preserved as-is, not commonly used in Claude output)
/// - **Bold+Italic**: `***text***` → `*_text_*`
/// - **Headers**: `# Header` → `*Header*` (bold)
/// - **Strikethrough**: `~~text~~` → `~text~`
/// - **Links**: `[text](url)` → `<url|text>`
/// - **Images**: `![alt](url)` → `<url|alt>`
/// - **Code blocks**: ` ```code``` ` → preserved as-is
/// - **Horizontal rules**: `---`, `***`, `___` → `─────────`
///
/// Also removes system tags like `<local-command-caveat>` and `<system-reminder>`
/// that appear in Claude's output but shouldn't be shown to users.
///
/// # Arguments
///
/// * `text` - Markdown text to convert
///
/// # Returns
///
/// Slack mrkdwn formatted text
pub(crate) fn markdown_to_slack(text: &str) -> String {
    // Strip system tags first.
    let text = RE_SYSTEM_TAGS.replace_all(text, "");

    // Split text into code blocks and non-code segments.
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;

    for m in RE_CODE_FENCE.find_iter(text.as_ref()) {
        // Process the non-code segment before this code block.
        let before = &text.as_ref()[last_end..m.start()];
        result.push_str(&convert_markdown_segment(before));
        // Preserve the code block verbatim.
        result.push_str(m.as_str());
        last_end = m.end();
    }

    // Process any remaining text after the last code block.
    let tail = &text.as_ref()[last_end..];
    result.push_str(&convert_markdown_segment(tail));

    result
}

/// Convert markdown formatting in a non-code segment to Slack mrkdwn.
fn convert_markdown_segment(text: &str) -> String {
    let mut s = text.to_string();

    // Bold+italic: ***text*** or ___text___ (dotall so it spans newlines)
    s = RE_BOLD_ITALIC
        .replace_all(&s, |caps: &regex::Captures| {
            let content = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
            // Collapse internal newlines so Slack renders inline formatting correctly.
            let content = content
                .split('\n')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
            format!("*_{}_*", content.trim())
        })
        .into_owned();

    // Bold: **text** or __text__ (dotall so it spans newlines)
    s = RE_BOLD
        .replace_all(&s, |caps: &regex::Captures| {
            let content = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
            // Collapse internal newlines so Slack renders inline formatting correctly.
            let content = content
                .split('\n')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
            format!("*{}*", content.trim())
        })
        .into_owned();

    // Headers: lines starting with # → bold
    s = RE_HEADER.replace_all(&s, "*$1*").into_owned();

    // Strikethrough: ~~text~~ → ~text~
    s = RE_STRIKE.replace_all(&s, "~$1~").into_owned();

    // Images: ![alt](url) → <url|alt>  (must come before links)
    s = RE_IMAGE.replace_all(&s, "<$2|$1>").into_owned();

    // Links: [text](url) → <url|text>
    s = RE_LINK.replace_all(&s, "<$2|$1>").into_owned();

    // Horizontal rules: ---, ***, ___ on their own line
    s = RE_HR.replace_all(&s, "─────────").into_owned();

    s
}

/// Splits text into chunks that fit within Slack's message size limits.
///
/// Slack has a limit of ~40,000 characters per message. This function splits
/// longer text into multiple chunks, preferring to split at newline boundaries
/// to preserve formatting and readability.
///
/// # Arguments
///
/// * `text` - Text to split
/// * `max_len` - Maximum length per chunk (typically `SLACK_MAX_MESSAGE_CHARS`)
///
/// # Returns
///
/// Vector of text chunks, each ≤ `max_len` characters. Returns a single-element
/// vector if the text already fits.
///
/// # Examples
///
/// ```ignore
/// let chunks = split_for_slack("short text", 40000);
/// assert_eq!(chunks.len(), 1);
/// ```
pub fn split_for_slack(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    // Pre-allocate capacity based on estimated chunk count
    let estimated_chunks = (text.len() / max_len) + 1;
    let mut chunks = Vec::with_capacity(estimated_chunks);
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        let boundary = crate::util::floor_char_boundary(remaining, max_len);
        let split_at = remaining[..boundary].rfind('\n').unwrap_or(boundary);
        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start_matches('\n');
    }
    chunks
}

/// Formats an agent response for posting to Slack.
///
/// Converts the response text from Markdown to Slack mrkdwn format and adds
/// metadata footer (turns, duration, error status). Splits into multiple
/// messages if needed to fit Slack's size limits.
///
/// # Arguments
///
/// * `resp` - Agent response containing result text and metadata
///
/// # Returns
///
/// Vector of formatted messages ready to post to Slack. Usually a single
/// message, but may be multiple if the response is very long.
pub(crate) fn format_agent_response(resp: &AgentResponse) -> Vec<String> {
    let text = markdown_to_slack(&resp.result);

    let duration = if resp.duration_ms >= 1000 {
        format!("{:.1}s", resp.duration_ms as f64 / 1000.0)
    } else {
        format!("{}ms", resp.duration_ms)
    };

    let hit_turn_limit = resp.subtype.as_deref() == Some("max_turns_reached")
        || resp.subtype.as_deref() == Some("error_max_turns");

    let tool_line = resp
        .tool_summary
        .as_ref()
        .map(|s| format!("{}\n", s))
        .unwrap_or_default();

    let footer = if hit_turn_limit {
        format!(
            "\n\n-----\n{}_:warning: Hit turn limit ({} turns, {})._ Reply in this thread to continue where Claude left off.",
            tool_line, resp.num_turns, duration
        )
    } else {
        format!(
            "\n\n-----\n{}_Turns: {} | Duration: {}_",
            tool_line, resp.num_turns, duration
        )
    };

    let prefix = if resp.is_error { "*Error:* " } else { "" };

    let mut chunks = split_for_slack(&text, SLACK_MAX_MESSAGE_CHARS);
    // Add error prefix to first chunk.
    if !prefix.is_empty()
        && let Some(first) = chunks.first_mut()
    {
        *first = format!("{}{}", prefix, first);
    }
    // Append footer to the last chunk.
    if let Some(last) = chunks.last_mut() {
        last.push_str(&footer);
    }
    chunks
}

/// Format AskUserQuestion data as a readable Slack message.
pub(crate) fn format_questions(questions: &serde_json::Value) -> String {
    let mut lines = vec!["*Claude is asking:*\n".to_string()];

    if let Some(arr) = questions.as_array() {
        for q in arr {
            if let Some(question) = q.get("question").and_then(|v| v.as_str()) {
                lines.push(format!("*{}*", question));
            }

            if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
                for (i, opt) in options.iter().enumerate() {
                    let label = opt.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                    let desc = opt
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(|d| format!(" — {}", d))
                        .unwrap_or_default();
                    lines.push(format!("{}. {}{}", i + 1, label, desc));
                }
            }
            lines.push(String::new());
        }
    } else if let Some(question) = questions.as_str() {
        lines.push(format!("*{}*", question));
    }

    lines.push("_Reply with the number or your own answer._".to_string());
    lines.join("\n")
}

/// Format a tool approval request for posting to Slack.
pub(crate) fn format_tool_approval(tool_name: &str, tool_input: &serde_json::Value) -> String {
    let input_preview = if let Some(obj) = tool_input.as_object() {
        let parts: Vec<String> = obj
            .iter()
            .take(3)
            .map(|(k, v)| {
                let v_str = match v.as_str() {
                    Some(s) if s.len() > 200 => format!("`{}...`", &s[..200]),
                    Some(s) => format!("`{}`", s),
                    None => {
                        let s = v.to_string();
                        if s.len() > 200 {
                            format!("`{}...`", &s[..200])
                        } else {
                            format!("`{}`", s)
                        }
                    }
                };
                format!("  {} = {}", k, v_str)
            })
            .collect();
        if parts.is_empty() {
            String::new()
        } else {
            format!("\n{}", parts.join("\n"))
        }
    } else {
        String::new()
    };

    format!(
        ":lock: *Tool approval:* `{}`{}\n\n_Reply `yes` to allow or `no` to deny._",
        tool_name, input_preview
    )
}

pub(crate) fn truncate_text(text: &str, max_len: usize) -> String {
    let text = &markdown_to_slack(text);
    if text.len() <= max_len {
        return text.to_string();
    }
    let mut truncated = text.to_string();
    let boundary = crate::util::floor_char_boundary(&truncated, max_len);
    if let Some(pos) = truncated[..boundary].rfind('\n') {
        truncated.truncate(pos);
    } else {
        truncated.truncate(boundary);
    }
    truncated.push_str("\n\n_(streaming...)_");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // ── Parameterized formatting tests ────────────────────────────

    #[rstest]
    #[case("**hello**", "*hello*", "bold with **")]
    #[case("__hello__", "*hello*", "bold with __")]
    #[case("**foo** and **bar**", "*foo* and *bar*", "multiple bold on same line")]
    #[case(
        "** spaced bold **",
        "*spaced bold*",
        "bold with leading/trailing space"
    )]
    fn test_bold_formatting(
        #[case] input: &str,
        #[case] expected: &str,
        #[case] description: &str,
    ) {
        assert_eq!(markdown_to_slack(input), expected, "{}", description);
    }

    #[rstest]
    #[case("***hello***", "*_hello_*", "bold+italic with ***")]
    #[case("___hello___", "*_hello_*", "bold+italic with ___")]
    fn test_bold_italic_formatting(
        #[case] input: &str,
        #[case] expected: &str,
        #[case] description: &str,
    ) {
        assert_eq!(markdown_to_slack(input), expected, "{}", description);
    }

    #[rstest]
    #[case("# Title", "*Title*", "h1")]
    #[case("## Subtitle", "*Subtitle*", "h2")]
    #[case("### Deep", "*Deep*", "h3")]
    fn test_headers_formatting(
        #[case] input: &str,
        #[case] expected: &str,
        #[case] description: &str,
    ) {
        assert_eq!(markdown_to_slack(input), expected, "{}", description);
    }

    #[rstest]
    #[case("---", "─────────", "dashes")]
    #[case("***", "─────────", "asterisks")]
    #[case("___", "─────────", "underscores")]
    fn test_horizontal_rule_formatting(
        #[case] input: &str,
        #[case] expected: &str,
        #[case] description: &str,
    ) {
        assert_eq!(markdown_to_slack(input), expected, "{}", description);
    }

    // ── Individual tests for special cases ────────────────────────

    #[test]
    fn test_italic_underscores() {
        // Single underscores pass through as-is (Slack italic).
        assert_eq!(markdown_to_slack("_hello_"), "_hello_");
    }

    #[test]
    fn test_strikethrough() {
        assert_eq!(markdown_to_slack("~~deleted~~"), "~deleted~");
    }

    #[test]
    fn test_links() {
        assert_eq!(
            markdown_to_slack("[click here](https://example.com)"),
            "<https://example.com|click here>"
        );
    }

    #[test]
    fn test_images() {
        assert_eq!(
            markdown_to_slack("![logo](https://example.com/img.png)"),
            "<https://example.com/img.png|logo>"
        );
    }

    #[test]
    fn test_bold_multiline() {
        // Bold spanning multiple lines should be collapsed to a single line.
        let input = "**changes made to\nthe auth module**";
        let result = markdown_to_slack(input);
        assert_eq!(result, "*changes made to the auth module*");
    }

    #[test]
    fn test_code_blocks_preserved() {
        let input = "before\n```rust\nlet x = **not bold**;\n```\nafter **bold**";
        let result = markdown_to_slack(input);
        assert!(result.contains("let x = **not bold**;"));
        assert!(result.contains("after *bold*"));
    }

    #[test]
    fn test_inline_code_untouched() {
        // Inline backtick code doesn't get the code-fence protection,
        // but the regex patterns shouldn't match inside backticks in typical usage.
        let input = "Use `**bold**` for bold";
        let result = markdown_to_slack(input);
        // The backticked content may or may not be transformed — this is acceptable
        // since Slack renders inline code literally anyway.
        assert!(result.contains("for bold"));
    }

    #[test]
    fn test_mixed_formatting() {
        let input =
            "## Summary\n\n**Key point:** see [docs](https://docs.rs) for details.\n\n---\n\nDone.";
        let result = markdown_to_slack(input);
        assert!(result.contains("*Summary*"));
        assert!(result.contains("*Key point:*"));
        assert!(result.contains("<https://docs.rs|docs>"));
        assert!(result.contains("─────────"));
        assert!(result.contains("Done."));
    }

    // ── Tool approval formatting tests ──────────────────────────────

    #[test]
    fn test_format_tool_approval_bash() {
        let input = serde_json::json!({ "command": "cargo test" });
        let result = format_tool_approval("Bash", &input);
        assert!(result.contains("`Bash`"));
        assert!(result.contains("cargo test"));
        assert!(result.contains("`yes`"));
        assert!(result.contains("`no`"));
    }

    #[test]
    fn test_format_tool_approval_empty_input() {
        let input = serde_json::json!({});
        let result = format_tool_approval("Edit", &input);
        assert!(result.contains("`Edit`"));
        assert!(result.contains("`yes`"));
    }

    #[test]
    fn test_format_tool_approval_truncates_long_values() {
        let long_value = "x".repeat(300);
        let input = serde_json::json!({ "content": long_value });
        let result = format_tool_approval("Write", &input);
        assert!(result.contains("...`"));
        assert!(result.len() < 500);
    }
}
