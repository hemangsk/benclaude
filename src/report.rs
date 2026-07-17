//! The git join: links transcript sessions to AI-co-authored commits and
//! derives the outcome metrics — line survival, friction per file, and
//! per-session results.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

use crate::git::{self, AiCommit};
use crate::state::{History, fmt_duration, fmt_tokens};

/// A commit needs at least this age before its survival says anything.
const MATURITY: Duration = Duration::days(7);
/// A commit is linked to a session when it lands inside the session window
/// stretched by this much (committing usually happens after the last turn).
const COMMIT_SLACK: Duration = Duration::minutes(45);

#[derive(Debug)]
pub(crate) struct CommitRow {
    pub(crate) sha: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) subject: String,
    pub(crate) added: u64,
    pub(crate) surviving: u64,
    pub(crate) mature: bool,
    pub(crate) session_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct HeatRow {
    pub(crate) file: String,
    pub(crate) edits: u64,
    pub(crate) sessions: u64,
    pub(crate) added: u64,
    pub(crate) surviving: u64,
}

#[derive(Debug)]
pub(crate) struct SessionRow {
    pub(crate) id: String,
    pub(crate) start: DateTime<Utc>,
    pub(crate) prompts: u64,
    pub(crate) output_tokens: u64,
    pub(crate) babysit: Duration,
    pub(crate) commits: u64,
}

/// Everything the report/heatmap/sessions views show.
#[derive(Debug)]
pub(crate) struct ReportData {
    pub(crate) commits: Vec<CommitRow>,
    pub(crate) heat: Vec<HeatRow>,
    pub(crate) sessions: Vec<SessionRow>,
    pub(crate) added_mature: u64,
    pub(crate) surviving_mature: u64,
    pub(crate) output_tokens_30d: u64,
}

impl ReportData {
    /// Survival across commits old enough to judge (≥ 7 days).
    pub(crate) fn survival_pct(&self) -> Option<f64> {
        (self.added_mature > 0).then(|| {
            #[allow(clippy::cast_precision_loss)]
            {
                self.surviving_mature as f64 / self.added_mature as f64 * 100.0
            }
        })
    }

    /// Output tokens spent per line that survived — the honest cost metric.
    pub(crate) fn tokens_per_surviving_line(&self) -> Option<f64> {
        (self.surviving_mature > 0).then(|| {
            #[allow(clippy::cast_precision_loss)]
            {
                self.output_tokens_30d as f64 / self.surviving_mature as f64
            }
        })
    }
}

/// Runs the whole join for the repo containing `cwd`.
pub(crate) fn build(cwd: &Path, history: &History) -> Result<ReportData> {
    let root = git::repo_root(cwd).context("not inside a git repository")?;
    let commits = git::ai_commits(&root, 30)?;
    let survival = git::survival(&root, &commits);
    let now = Utc::now();

    let commit_rows: Vec<CommitRow> = commits
        .iter()
        .map(|commit| CommitRow {
            sha: commit.sha.clone(),
            at: commit.at,
            subject: commit.subject.clone(),
            added: commit.added,
            surviving: survival.per_commit.get(&commit.sha).copied().unwrap_or(0),
            mature: now - commit.at >= MATURITY,
            session_id: link_session(history, commit),
        })
        .collect();

    let (added_mature, surviving_mature) = commit_rows
        .iter()
        .filter(|row| row.mature)
        .fold((0, 0), |(added, surviving), row| {
            (added + row.added, surviving + row.surviving)
        });

    Ok(ReportData {
        heat: heat_rows(history, &commits, &survival.per_file),
        sessions: session_rows(history, &commit_rows),
        commits: commit_rows,
        added_mature,
        surviving_mature,
        output_tokens_30d: history.summaries.iter().map(|s| s.output_tokens).sum(),
    })
}

/// The session whose (padded) time window contains the commit.
fn link_session(history: &History, commit: &AiCommit) -> Option<String> {
    history
        .summaries
        .iter()
        .find(|s| commit.at >= s.start && commit.at <= s.end + COMMIT_SLACK)
        .map(|s| s.id.clone())
}

/// Transcript churn joined with git adds/survival, worst offenders first.
/// Transcripts know basenames while git knows repo-relative paths, so the
/// join key is the basename; collisions merge, which is good enough for a
/// friction ranking.
fn heat_rows(
    history: &History,
    commits: &[AiCommit],
    survival_per_file: &HashMap<String, u64>,
) -> Vec<HeatRow> {
    let mut added_by_base: HashMap<String, u64> = HashMap::new();
    for commit in commits {
        for (path, added) in &commit.files {
            *added_by_base.entry(basename(path)).or_insert(0) += added;
        }
    }
    // Survival is a per-path fact — aggregate it once per path, not once per
    // commit touching the path.
    let mut surviving_by_base: HashMap<String, u64> = HashMap::new();
    for (path, surviving) in survival_per_file {
        *surviving_by_base.entry(basename(path)).or_insert(0) += surviving;
    }
    let mut rows: Vec<HeatRow> = history
        .edits_per_file
        .iter()
        .map(|(file, edits)| HeatRow {
            file: file.clone(),
            edits: *edits,
            sessions: history.sessions_per_file.get(file).copied().unwrap_or(0),
            added: added_by_base.get(file).copied().unwrap_or(0),
            surviving: surviving_by_base.get(file).copied().unwrap_or(0),
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse((row.edits, row.added)));
    rows
}

fn session_rows(history: &History, commits: &[CommitRow]) -> Vec<SessionRow> {
    history
        .summaries
        .iter()
        .rev()
        .map(|summary| SessionRow {
            id: summary.id.clone(),
            start: summary.start,
            prompts: summary.prompts,
            output_tokens: summary.output_tokens,
            babysit: summary.babysit,
            commits: commits
                .iter()
                .filter(|c| c.session_id.as_deref() == Some(summary.id.as_str()))
                .count() as u64,
        })
        .collect()
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |f| f.to_string_lossy().into_owned())
}

// ---- plain-text output for the non-TUI subcommands ------------------------

pub(crate) fn print_report(data: &ReportData) {
    println!("benclaude report — last 30 days\n");
    println!("AI commits        {}", data.commits.len());
    println!(
        "lines added       {}",
        data.commits.iter().map(|c| c.added).sum::<u64>()
    );
    match data.survival_pct() {
        Some(pct) => println!(
            "line survival 7d+ {pct:.0}%  ({} of {} mature lines)",
            data.surviving_mature, data.added_mature
        ),
        None => println!("line survival 7d+ — (no commits old enough yet)"),
    }
    if let Some(cost) = data.tokens_per_surviving_line() {
        println!("tok/surviving ln  {cost:.0}");
    }
    println!();
    for row in &data.commits {
        let survival = if row.mature {
            format!("{}/{}", row.surviving, row.added)
        } else {
            format!("{}/{} <7d", row.surviving, row.added)
        };
        println!(
            "{}  {}  {:>12}  {}",
            &row.sha[..8.min(row.sha.len())],
            row.at.format("%m-%d"),
            survival,
            row.subject
        );
    }
}

pub(crate) fn print_heatmap(data: &ReportData) {
    println!("benclaude heatmap — agent friction, last 30 days\n");
    println!(
        "{:<34} {:>5} {:>5} {:>7} {:>7}",
        "file", "edits", "sess", "added", "alive"
    );
    for row in data.heat.iter().take(20) {
        println!(
            "{:<34} {:>5} {:>5} {:>7} {:>7}",
            row.file, row.edits, row.sessions, row.added, row.surviving
        );
    }
}

pub(crate) fn print_sessions(data: &ReportData) {
    println!("benclaude sessions — last 30 days\n");
    println!(
        "{:<10} {:<12} {:>7} {:>8} {:>8} {:>7}",
        "session", "start", "turns", "tokens", "babysit", "commits"
    );
    for row in &data.sessions {
        println!(
            "{:<10} {:<12} {:>7} {:>8} {:>8} {:>7}",
            row.id.chars().take(8).collect::<String>(),
            row.start.format("%m-%d %H:%M"),
            row.prompts,
            fmt_tokens(row.output_tokens),
            fmt_duration(row.babysit),
            row.commits
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionSummary;

    #[test]
    fn links_commits_landing_inside_the_padded_session_window() {
        let start = DateTime::from_timestamp(1_800_000_000, 0).expect("ts");
        let end = start + Duration::hours(1);
        let history = History {
            summaries: vec![SessionSummary {
                id: "abc".into(),
                start,
                end,
                prompts: 3,
                output_tokens: 1000,
                babysit: Duration::zero(),
            }],
            ..History::default()
        };
        let commit = AiCommit {
            sha: "deadbeef".into(),
            at: end + Duration::minutes(20),
            subject: "x".into(),
            added: 10,
            files: vec![],
        };
        assert_eq!(link_session(&history, &commit), Some("abc".into()));

        let too_late = AiCommit {
            at: end + Duration::hours(2),
            ..commit
        };
        assert_eq!(link_session(&history, &too_late), None);
    }
}
