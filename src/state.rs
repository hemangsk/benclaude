//! Session state folded from transcript events, plus the 30-day history
//! aggregates that annotate the live view.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration, Local, NaiveDate, Utc};

use crate::transcript::{self, TranscriptEvent};

/// One tool invocation as shown in the live feed.
#[derive(Debug, Clone)]
pub(crate) struct ToolRun {
    pub(crate) at: DateTime<Utc>,
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) done_at: Option<DateTime<Utc>>,
}

/// Everything derived from the live session transcript.
#[derive(Debug, Default)]
pub(crate) struct SessionState {
    pub(crate) session_id: String,
    pub(crate) started: Option<DateTime<Utc>>,
    pub(crate) prompts: u64,
    pub(crate) last_prompt_at: Option<DateTime<Utc>>,
    pub(crate) interruptions: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) turn_output_tokens: u64,
    pub(crate) tools: Vec<ToolRun>,
    open_tools: HashMap<String, usize>,
    pub(crate) files_this_turn: Vec<String>,
    pub(crate) last_assistant_text: Option<String>,
    pub(crate) waiting_since: Option<DateTime<Utc>>,
    pub(crate) babysit: Duration,
}

impl SessionState {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            session_id,
            ..Self::default()
        }
    }

    pub(crate) fn apply(&mut self, event: &TranscriptEvent) {
        match event {
            TranscriptEvent::UserPrompt { at } => {
                self.started.get_or_insert(*at);
                // A new turn means every still-open tool of the previous
                // turn is dead — results can't arrive across turns.
                self.open_tools.clear();
                if let Some(since) = self.waiting_since.take() {
                    self.babysit += *at - since;
                }
                self.prompts += 1;
                self.last_prompt_at = Some(*at);
                self.turn_output_tokens = 0;
                self.files_this_turn.clear();
            }
            TranscriptEvent::Interrupted { at } => {
                self.interruptions += 1;
                self.waiting_since = Some(*at);
            }
            TranscriptEvent::ToolCall {
                at,
                id,
                name,
                label,
            } => {
                self.waiting_since = None;
                if transcript::is_file_edit(name)
                    && !label.is_empty()
                    && !self.files_this_turn.contains(label)
                {
                    self.files_this_turn.push(label.clone());
                }
                self.tools.push(ToolRun {
                    at: *at,
                    name: name.clone(),
                    label: label.clone(),
                    done_at: None,
                });
                if !id.is_empty() {
                    self.open_tools.insert(id.clone(), self.tools.len() - 1);
                }
            }
            TranscriptEvent::ToolResult { at, id } => {
                if let Some(index) = self.open_tools.remove(id)
                    && let Some(run) = self.tools.get_mut(index)
                {
                    run.done_at = Some(*at);
                }
            }
            TranscriptEvent::AssistantText { text } => {
                self.last_assistant_text = Some(text.clone());
            }
            TranscriptEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                self.turn_output_tokens += output_tokens;
            }
            TranscriptEvent::TurnDone { at } => {
                // Unmatched tool results must not block waiting-detection:
                // the turn is over, so nothing is genuinely running.
                self.open_tools.clear();
                self.waiting_since.get_or_insert(*at);
            }
        }
    }

    /// The tool that is genuinely running now: only the newest call counts —
    /// an older call whose result never matched is stale, not running.
    pub(crate) fn running_tool(&self) -> Option<&ToolRun> {
        self.tools.last().filter(|t| t.done_at.is_none())
    }

    /// Most recent finished tools, newest first.
    pub(crate) fn recent_finished(&self, count: usize) -> Vec<&ToolRun> {
        self.tools
            .iter()
            .rev()
            .filter(|t| t.done_at.is_some())
            .take(count)
            .collect()
    }
}

/// A digest of one past session, used to join sessions to git commits.
#[derive(Debug, Clone)]
pub(crate) struct SessionSummary {
    pub(crate) id: String,
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
    pub(crate) prompts: u64,
    pub(crate) output_tokens: u64,
    pub(crate) babysit: Duration,
}

/// Aggregates over the last 30 days of transcripts in the project directory.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub(crate) sessions: u64,
    pub(crate) tokens_by_day: Vec<(NaiveDate, u64)>,
    pub(crate) edits_per_file: HashMap<String, u64>,
    pub(crate) sessions_per_file: HashMap<String, u64>,
    /// Edit counts per file per local day — the heatmap grid.
    pub(crate) edits_by_file_day: HashMap<String, HashMap<NaiveDate, u64>>,
    pub(crate) followups_avg: f64,
    pub(crate) babysit_today: Duration,
    pub(crate) summaries: Vec<SessionSummary>,
}

impl History {
    /// Scans every recent transcript once, at startup. Cheap enough for tens
    /// of megabytes; live data afterwards comes from the tailer only.
    pub(crate) fn scan(project_dir: &Path, now: DateTime<Utc>) -> Self {
        let mut history = Self::default();
        let mut followups_total: u64 = 0;
        let mut tokens_by_day: HashMap<NaiveDate, u64> = HashMap::new();
        let today = now.with_timezone(&Local).date_naive();

        for path in transcript::recent_sessions(project_dir, 30) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            history.sessions += 1;
            let mut summary = SessionSummary {
                id: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                start: DateTime::<Utc>::MAX_UTC,
                end: DateTime::<Utc>::MIN_UTC,
                prompts: 0,
                output_tokens: 0,
                babysit: Duration::zero(),
            };
            let mut session_files: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut waiting: Option<DateTime<Utc>> = None;
            for event in text.lines().flat_map(transcript::parse_line) {
                match event {
                    TranscriptEvent::UserPrompt { at } => {
                        summary.prompts += 1;
                        summary.start = summary.start.min(at);
                        summary.end = summary.end.max(at);
                        if let Some(since) = waiting.take() {
                            summary.babysit += at - since;
                            if at.with_timezone(&Local).date_naive() == today {
                                history.babysit_today += at - since;
                            }
                        }
                    }
                    TranscriptEvent::TurnDone { at } => {
                        summary.end = summary.end.max(at);
                        waiting = Some(at);
                    }
                    TranscriptEvent::ToolCall {
                        at, name, label, ..
                    } => {
                        waiting = None;
                        summary.end = summary.end.max(at);
                        if transcript::is_file_edit(&name) && !label.is_empty() {
                            *history.edits_per_file.entry(label.clone()).or_insert(0) += 1;
                            *history
                                .edits_by_file_day
                                .entry(label.clone())
                                .or_default()
                                .entry(at.with_timezone(&Local).date_naive())
                                .or_insert(0) += 1;
                            session_files.insert(label);
                        }
                    }
                    TranscriptEvent::Usage {
                        at, output_tokens, ..
                    } => {
                        summary.output_tokens += output_tokens;
                        summary.end = summary.end.max(at);
                        let day = at.with_timezone(&Local).date_naive();
                        *tokens_by_day.entry(day).or_insert(0) += output_tokens;
                    }
                    _ => {}
                }
            }
            followups_total += summary.prompts.saturating_sub(1);
            for file in session_files {
                *history.sessions_per_file.entry(file).or_insert(0) += 1;
            }
            if summary.prompts > 0 {
                history.summaries.push(summary);
            }
        }
        history.summaries.sort_by_key(|s| s.start);

        // Last 8 calendar days, oldest first, for the sparkline.
        history.tokens_by_day = (0..8)
            .rev()
            .map(|days_ago| {
                let day = today - Duration::days(days_ago);
                (day, tokens_by_day.get(&day).copied().unwrap_or(0))
            })
            .collect();
        if history.sessions > 0 {
            #[allow(clippy::cast_precision_loss)]
            {
                history.followups_avg = followups_total as f64 / history.sessions as f64;
            }
        }
        history
    }

    /// The most-edited files, descending — the heatmap rows.
    pub(crate) fn hottest_files(&self, count: usize) -> Vec<(&str, u64)> {
        let mut files: Vec<(&str, u64)> = self
            .edits_per_file
            .iter()
            .map(|(name, edits)| (name.as_str(), *edits))
            .collect();
        files.sort_by_key(|(name, edits)| (std::cmp::Reverse(*edits), *name));
        files.truncate(count);
        files
    }

    /// Historical churn for a file shown in FILES THIS TURN.
    pub(crate) fn churn(&self, file: &str) -> u64 {
        self.edits_per_file.get(file).copied().unwrap_or(0)
    }
}

/// `8_243` -> `"8.2k"`, `412_390` -> `"412k"`, `999` -> `"999"`.
pub(crate) fn fmt_tokens(tokens: u64) -> String {
    if tokens >= 100_000 {
        format!("{}k", tokens / 1_000)
    } else if tokens >= 1_000 {
        #[allow(clippy::cast_precision_loss)]
        let thousands = tokens as f64 / 1_000.0;
        format!("{thousands:.1}k")
    } else {
        tokens.to_string()
    }
}

/// `Duration` -> `"3m 12s"` / `"41s"` / `"2h 05m"`.
pub(crate) fn fmt_duration(duration: Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("valid timestamp")
    }

    #[test]
    fn folds_a_turn_and_tracks_waiting() {
        let mut session = SessionState::new("s1".into());
        session.apply(&TranscriptEvent::UserPrompt { at: at(0) });
        session.apply(&TranscriptEvent::ToolCall {
            at: at(1),
            id: "t1".into(),
            name: "Edit".into(),
            label: "profile_tab.dart".into(),
        });
        session.apply(&TranscriptEvent::ToolResult {
            at: at(5),
            id: "t1".into(),
        });
        session.apply(&TranscriptEvent::Usage {
            at: at(6),
            input_tokens: 100,
            output_tokens: 900,
        });
        session.apply(&TranscriptEvent::TurnDone { at: at(6) });

        assert_eq!(session.prompts, 1);
        assert_eq!(session.turn_output_tokens, 900);
        assert_eq!(session.files_this_turn, vec!["profile_tab.dart"]);
        assert!(session.running_tool().is_none());
        assert_eq!(session.waiting_since, Some(at(6)));

        // The next prompt banks the babysit gap and resets turn counters.
        session.apply(&TranscriptEvent::UserPrompt { at: at(66) });
        assert_eq!(session.babysit, Duration::seconds(60));
        assert_eq!(session.turn_output_tokens, 0);
        assert!(session.files_this_turn.is_empty());
    }

    #[test]
    fn stale_unmatched_tool_is_not_running_and_does_not_block_waiting() {
        let mut session = SessionState::new("s1".into());
        session.apply(&TranscriptEvent::UserPrompt { at: at(0) });
        // A tool whose result never arrives (unmatched tool_use_id).
        session.apply(&TranscriptEvent::ToolCall {
            at: at(1),
            id: "lost".into(),
            name: "Read".into(),
            label: "app.dart".into(),
        });
        // A newer tool that completes normally.
        session.apply(&TranscriptEvent::ToolCall {
            at: at(10),
            id: "ok".into(),
            name: "Bash".into(),
            label: "cargo test".into(),
        });
        session.apply(&TranscriptEvent::ToolResult {
            at: at(12),
            id: "ok".into(),
        });

        // The stale Read must not resurface as "running".
        assert!(session.running_tool().is_none());
        // And it must not appear among finished tools (duration unknown).
        assert!(session.recent_finished(5).iter().all(|t| t.name == "Bash"));

        // Turn end still detects waiting despite the unmatched open tool.
        session.apply(&TranscriptEvent::TurnDone { at: at(20) });
        assert_eq!(session.waiting_since, Some(at(20)));
    }

    #[test]
    fn formats_tokens_and_durations() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(8_243), "8.2k");
        assert_eq!(fmt_tokens(412_390), "412k");
        assert_eq!(fmt_duration(Duration::seconds(41)), "41s");
        assert_eq!(fmt_duration(Duration::seconds(192)), "3m 12s");
        assert_eq!(fmt_duration(Duration::seconds(7500)), "2h 05m");
    }
}
