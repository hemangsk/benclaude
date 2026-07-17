//! Git-side data: AI-co-authored commits and how many of their lines are
//! still alive today. Shells out to the `git` CLI — it is on every machine
//! benclaude runs on, and it beats carrying a git library dependency.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

/// One commit with a `Co-Authored-By: Claude` trailer.
#[derive(Debug, Clone)]
pub(crate) struct AiCommit {
    pub(crate) sha: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) subject: String,
    /// Lines added, total and per file (from `--numstat`).
    pub(crate) added: u64,
    pub(crate) files: Vec<(String, u64)>,
}

/// Lines from AI commits still present in `HEAD`, keyed two ways.
#[derive(Debug, Default)]
pub(crate) struct Survival {
    pub(crate) per_commit: HashMap<String, u64>,
    pub(crate) per_file: HashMap<String, u64>,
}

/// The repository containing `dir`, if any.
pub(crate) fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let root = out.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// All commits on the current branch within `days` that carry a
/// `Co-Authored-By: Claude` trailer, with per-file added-line counts.
pub(crate) fn ai_commits(root: &Path, days: u64) -> Result<Vec<AiCommit>> {
    let since = format!("--since={days}.days");
    let out = git(
        root,
        &[
            "log",
            &since,
            "--no-merges",
            "--grep=Co-Authored-By: Claude",
            "--format=%x01%H%x02%ct%x02%s",
            "--numstat",
        ],
    )?;
    let mut commits: Vec<AiCommit> = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix('\u{1}') {
            let mut parts = rest.split('\u{2}');
            let sha = parts.next().unwrap_or_default().to_owned();
            let Some(at) = parts
                .next()
                .and_then(|t| t.parse::<i64>().ok())
                .and_then(|t| DateTime::from_timestamp(t, 0))
            else {
                continue;
            };
            commits.push(AiCommit {
                sha,
                at,
                subject: parts.next().unwrap_or_default().to_owned(),
                added: 0,
                files: Vec::new(),
            });
        } else if !line.is_empty() {
            // numstat row: `<added>\t<deleted>\t<path>`; binary files show
            // `-`, renames show `old => new` — both are skipped below or
            // simply fail blame later, which counts as zero survival.
            let mut cols = line.split('\t');
            if let (Some(added), Some(_), Some(path)) = (cols.next(), cols.next(), cols.next())
                && let Ok(added) = added.parse::<u64>()
                && let Some(commit) = commits.last_mut()
            {
                commit.added += added;
                commit.files.push((path.to_owned(), added));
            }
        }
    }
    Ok(commits)
}

/// Blames every file touched by the given commits once and counts, per
/// commit and per file, how many lines in `HEAD` still belong to them.
pub(crate) fn survival(root: &Path, commits: &[AiCommit]) -> Survival {
    let shas: HashSet<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
    let files: BTreeSet<&str> = commits
        .iter()
        .flat_map(|c| c.files.iter().map(|(file, _)| file.as_str()))
        .collect();
    let mut result = Survival::default();
    for file in files {
        // Deleted or renamed files fail here: zero surviving lines is the
        // correct answer for them.
        let Ok(out) = git(root, &["blame", "--line-porcelain", "HEAD", "--", file]) else {
            continue;
        };
        for line in out.lines() {
            // --line-porcelain repeats a `<sha> <orig> <final>` header for
            // every line; content lines start with a tab, metadata lines
            // with a word — neither can collide with a full sha token.
            if let Some(token) = line.split(' ').next()
                && shas.contains(token)
            {
                *result.per_commit.entry(token.to_owned()).or_insert(0) += 1;
                *result.per_file.entry(file.to_owned()).or_insert(0) += 1;
            }
        }
    }
    result
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run git — is it installed?")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_resolves_inside_a_repo() {
        // This test file lives in a git repository (benclaude itself).
        let here = std::env::current_dir().expect("cwd");
        let root = repo_root(&here).expect("benclaude is a git repo");
        assert!(root.join(".git").exists());
    }

    #[test]
    fn repo_root_is_none_outside_a_repo() {
        assert!(repo_root(Path::new("/")).is_none());
    }
}
