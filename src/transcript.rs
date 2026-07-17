//! Locating, tailing, and parsing Claude Code session transcripts.
//!
//! Transcripts live under `~/.claude/projects/<sanitized-cwd>/<session>.jsonl`
//! and are appended to while a session runs. The tailer is strictly read-only
//! and only consumes bytes up to the last complete newline, so half-written
//! JSON lines never reach the parser.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// One semantic event pulled out of a transcript line. A single JSONL line
/// can yield several events (an assistant message carries usage plus any
/// number of tool calls).
#[derive(Debug, Clone)]
pub(crate) enum TranscriptEvent {
    /// A real prompt typed by the user (tool results are not prompts).
    UserPrompt { at: DateTime<Utc> },
    /// The user interrupted the agent mid-turn.
    Interrupted { at: DateTime<Utc> },
    /// The assistant invoked a tool.
    ToolCall {
        at: DateTime<Utc>,
        id: String,
        name: String,
        label: String,
    },
    /// A tool finished and its result was recorded.
    ToolResult { at: DateTime<Utc>, id: String },
    /// Plain assistant prose.
    AssistantText { text: String },
    /// Token usage reported on an assistant message.
    Usage {
        at: DateTime<Utc>,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// The assistant ended its turn and is now waiting on the user.
    TurnDone { at: DateTime<Utc> },
}

/// Maps a working directory to its Claude Code project transcript directory,
/// e.g. `/Users/x/app` -> `~/.claude/projects/-Users-x-app`.
pub(crate) fn project_dir(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let mut dir = PathBuf::from(home);
    dir.push(".claude");
    dir.push("projects");
    dir.push(sanitize(cwd));
    Ok(dir)
}

/// Claude Code flattens the cwd path into a directory name by replacing every
/// non-alphanumeric character with `-`.
pub(crate) fn sanitize(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// All `*.jsonl` transcripts in `dir`, newest-modified first.
pub(crate) fn sessions_by_recency(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                return None;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, path))
        })
        .collect();
    sessions.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    sessions.into_iter().map(|(_, path)| path).collect()
}

/// The most recently modified `*.jsonl` transcript in `dir`, if any.
pub(crate) fn latest_session(dir: &Path) -> Option<PathBuf> {
    sessions_by_recency(dir).into_iter().next()
}

/// All `*.jsonl` transcripts in `dir` modified within the last `days` days.
pub(crate) fn recent_sessions(dir: &Path, days: u64) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(days * 24 * 60 * 60));
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                return None;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            match cutoff {
                Some(cutoff) if modified < cutoff => None,
                _ => Some(path),
            }
        })
        .collect()
}

/// Incremental, read-only reader for an append-only JSONL file.
#[derive(Debug)]
pub(crate) struct Tailer {
    path: PathBuf,
    offset: u64,
}

impl Tailer {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, offset: 0 }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Reads any newly appended complete lines and parses them into events.
    pub(crate) fn poll(&mut self) -> Result<Vec<TranscriptEvent>> {
        let mut file =
            File::open(&self.path).with_context(|| format!("open {}", self.path.display()))?;
        let len = file.metadata()?.len();
        if len < self.offset {
            // Truncated or replaced; start over.
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(len - self.offset).read_to_end(&mut bytes)?;
        // Hold back everything after the last newline: it is a line still
        // being written and will be consumed once complete.
        let Some(last_newline) = bytes.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        self.offset += u64::try_from(last_newline + 1).context("offset overflow")?;
        let text = String::from_utf8_lossy(&bytes[..=last_newline]);
        Ok(text.lines().flat_map(parse_line).collect())
    }
}

/// Parses one transcript JSONL line into zero or more events. Unknown or
/// malformed lines are skipped: the transcript format is not a public
/// contract, so tolerance beats strictness here.
pub(crate) fn parse_line(line: &str) -> Vec<TranscriptEvent> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(at) = value["timestamp"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
    else {
        return Vec::new();
    };
    match value["type"].as_str() {
        Some("user") => parse_user(&value, at),
        Some("assistant") => parse_assistant(&value, at),
        _ => Vec::new(),
    }
}

fn parse_user(value: &Value, at: DateTime<Utc>) -> Vec<TranscriptEvent> {
    if value["isMeta"].as_bool() == Some(true) {
        return Vec::new();
    }
    let content = &value["message"]["content"];
    let mut events = Vec::new();
    let mut text = String::new();
    if let Some(s) = content.as_str() {
        text.push_str(s);
    }
    if let Some(items) = content.as_array() {
        for item in items {
            match item["type"].as_str() {
                Some("tool_result") => {
                    if let Some(id) = item["tool_use_id"].as_str() {
                        events.push(TranscriptEvent::ToolResult {
                            at,
                            id: id.to_owned(),
                        });
                    }
                }
                Some("text") => {
                    if let Some(s) = item["text"].as_str() {
                        text.push_str(s);
                    }
                }
                _ => {}
            }
        }
    }
    let text = text.trim();
    if text.starts_with("[Request interrupted") {
        events.push(TranscriptEvent::Interrupted { at });
    } else if !text.is_empty() && !text.starts_with('<') {
        // Lines starting with '<' are harness-injected (command output,
        // caveats, reminders), not something the user typed.
        events.push(TranscriptEvent::UserPrompt { at });
    }
    events
}

fn parse_assistant(value: &Value, at: DateTime<Utc>) -> Vec<TranscriptEvent> {
    let message = &value["message"];
    let mut events = Vec::new();
    let usage = &message["usage"];
    if usage.is_object() {
        events.push(TranscriptEvent::Usage {
            at,
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        });
    }
    if let Some(blocks) = message["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("?").to_owned();
                    let label = tool_label(&name, &block["input"]);
                    events.push(TranscriptEvent::ToolCall {
                        at,
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name,
                        label,
                    });
                }
                Some("text") => {
                    if let Some(s) = block["text"].as_str() {
                        let s = s.trim();
                        if !s.is_empty() {
                            events.push(TranscriptEvent::AssistantText { text: s.to_owned() });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if message["stop_reason"].as_str() == Some("end_turn") {
        events.push(TranscriptEvent::TurnDone { at });
    }
    events
}

/// A short human label for a tool call, mirroring what Claude Code shows:
/// file basename for file tools, truncated command for Bash, etc.
fn tool_label(name: &str, input: &Value) -> String {
    let label = match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" => input["file_path"]
            .as_str()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy().into_owned()),
        "Bash" => input["command"]
            .as_str()
            .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")),
        "Grep" | "Glob" => input["pattern"].as_str().map(str::to_owned),
        "WebFetch" => input["url"].as_str().map(str::to_owned),
        "Skill" => input["skill"].as_str().map(str::to_owned),
        "Agent" | "Task" => input["description"].as_str().map(str::to_owned),
        _ => None,
    };
    let mut label = label.unwrap_or_default();
    if label.chars().count() > 28 {
        label = label.chars().take(27).collect::<String>() + "…";
    }
    label
}

/// True for tools that mutate files; used for the FILES THIS TURN panel and
/// the historical churn index.
pub(crate) fn is_file_edit(name: &str) -> bool {
    matches!(name, "Edit" | "Write" | "NotebookEdit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_claude_code_scheme() {
        assert_eq!(
            sanitize(Path::new("/Users/hemang/code/yana/frontend")),
            "-Users-hemang-code-yana-frontend"
        );
    }

    #[test]
    fn parses_user_prompt_and_tool_result() {
        let prompt = r#"{"type":"user","timestamp":"2026-07-17T04:00:00.000Z","message":{"role":"user","content":"fix the banner"}}"#;
        assert!(matches!(
            parse_line(prompt).as_slice(),
            [TranscriptEvent::UserPrompt { .. }]
        ));

        let result = r#"{"type":"user","timestamp":"2026-07-17T04:00:01.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ok"}]}}"#;
        assert!(matches!(
            parse_line(result).as_slice(),
            [TranscriptEvent::ToolResult { .. }]
        ));
    }

    #[test]
    fn parses_assistant_tools_usage_and_turn_end() {
        let line = r#"{"type":"assistant","timestamp":"2026-07-17T04:00:02.000Z","message":{"role":"assistant","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":42},"content":[{"type":"text","text":"done"},{"type":"tool_use","id":"tu_2","name":"Edit","input":{"file_path":"/a/b/profile_tab.dart"}}]}}"#;
        let events = parse_line(line);
        assert_eq!(events.len(), 4);
        assert!(events.iter().any(|e| matches!(
            e,
            TranscriptEvent::ToolCall { label, .. } if label == "profile_tab.dart"
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TranscriptEvent::TurnDone { .. }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            TranscriptEvent::Usage {
                output_tokens: 42,
                ..
            }
        )));
    }

    #[test]
    fn garbage_and_partial_lines_are_skipped() {
        assert!(parse_line("{not json").is_empty());
        assert!(parse_line(r#"{"type":"user"}"#).is_empty());
    }
}
