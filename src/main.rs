//! benclaude — a live outcome-analytics side pane for Claude Code.
//!
//! Runs in a narrow terminal split beside a Claude Code session, tails the
//! session transcript read-only, and annotates the live activity with
//! 30-day history from past transcripts of the same project. The report,
//! heatmap, and sessions views join those transcripts with git history.

mod git;
mod report;
mod state;
mod transcript;
mod ui;

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use report::ReportData;
use state::{History, SessionState};
use transcript::Tailer;

const TICK: Duration = Duration::from_millis(250);
const RESCAN_SESSIONS: Duration = Duration::from_secs(5);
const TOAST_FOR: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Watch,
    Doctor,
    Report,
    Heatmap,
    Sessions,
}

/// Which screen the TUI is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Watch,
    Report,
}

/// Shared app state handed to the renderer.
#[derive(Debug)]
pub(crate) struct App {
    pub(crate) project_name: String,
    pub(crate) session: Option<SessionState>,
    pub(crate) history: History,
    pub(crate) toast: Option<String>,
    pub(crate) view: View,
    pub(crate) report: Option<ReportData>,
    /// True after a manual session switch: auto-follow is paused until 'a'.
    pub(crate) pinned: bool,
    /// Session ids hidden from view — transcripts themselves are never touched.
    hidden: HashSet<String>,
    hidden_path: PathBuf,
    toast_until: Option<Instant>,
    cwd: PathBuf,
    project_dir: PathBuf,
    tailer: Option<Tailer>,
    last_session_check: Option<Instant>,
}

fn main() -> Result<()> {
    let (command, cwd) = parse_args()?;
    let project_dir = transcript::project_dir(&cwd)?;

    let hidden_path = hidden_path()?;
    let hidden = load_hidden(&hidden_path);

    match command {
        Command::Doctor => return doctor(&cwd, &project_dir, &hidden),
        Command::Report | Command::Heatmap | Command::Sessions => {
            let history = History::scan(&project_dir, Utc::now(), &hidden);
            let data = report::build(&cwd, &history)?;
            match command {
                Command::Report => report::print_report(&data),
                Command::Heatmap => report::print_heatmap(&data),
                _ => report::print_sessions(&data),
            }
            return Ok(());
        }
        Command::Watch => {}
    }
    if !std::io::stdout().is_terminal() {
        bail!("benclaude watch needs a TTY — run it inside a terminal pane");
    }

    let history = History::scan(&project_dir, Utc::now(), &hidden);
    // Inline heatmap/sessions blocks want the git join up front; a failure
    // (no repo, no git) just degrades those cells to "—".
    let initial_report = report::build(&cwd, &history).ok();
    let mut app = App {
        project_name: project_name(&cwd),
        session: None,
        history,
        toast: None,
        view: View::Watch,
        report: initial_report,
        pinned: false,
        hidden,
        hidden_path,
        toast_until: None,
        cwd,
        project_dir,
        tailer: None,
        last_session_check: None,
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

/// `parent/dir` of the watched project, for the header.
fn project_name(cwd: &Path) -> String {
    let dir = cwd.file_name().map_or_else(
        || cwd.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    cwd.parent()
        .and_then(|p| p.file_name())
        .map_or_else(|| dir.clone(), |p| format!("{}/{dir}", p.to_string_lossy()))
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.refresh()?;
        terminal.draw(|frame| ui::render(frame, app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(());
            }
            match (app.view, key.code) {
                (View::Watch, KeyCode::Char('q') | KeyCode::Esc) => return Ok(()),
                (View::Watch, KeyCode::Char('r')) => app.open_report(),
                (View::Watch, KeyCode::Char('s')) => app.cycle_session(),
                (View::Watch, KeyCode::Char('x')) => app.hide_current(),
                (View::Watch, KeyCode::Char('u')) => app.unhide_all(),
                (View::Watch, KeyCode::Char('a')) => {
                    app.pinned = false;
                    app.show_toast("auto-following the newest session");
                }
                (View::Report, KeyCode::Char('q' | 'b') | KeyCode::Esc) => {
                    app.view = View::Watch;
                }
                (View::Report, KeyCode::Char('r')) => {
                    app.report = None;
                    app.open_report();
                }
                _ => {}
            }
        }
    }
}

impl App {
    /// Picks up new transcript lines, switches to a newer session when one
    /// appears, and expires the toast.
    fn refresh(&mut self) -> Result<()> {
        if !self.pinned
            && self
                .last_session_check
                .is_none_or(|checked| checked.elapsed() >= RESCAN_SESSIONS)
        {
            self.last_session_check = Some(Instant::now());
            if let Some(latest) = self.visible_sessions().into_iter().next()
                && self.tailer.as_ref().is_none_or(|t| t.path() != latest)
            {
                self.watch_path(latest);
            }
        }
        if let (Some(tailer), Some(session)) = (&mut self.tailer, &mut self.session) {
            for event in tailer.poll()? {
                session.apply(&event);
            }
        }
        if self
            .toast_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.toast = None;
            self.toast_until = None;
        }
        Ok(())
    }

    /// Starts (or restarts) watching one transcript from the top.
    fn watch_path(&mut self, path: PathBuf) {
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.session = Some(SessionState::new(id));
        self.tailer = Some(Tailer::new(path));
    }

    /// Manually steps to the next-most-recent session and pins it, so
    /// auto-follow stops fighting the choice (two live sessions otherwise
    /// bounce the view to whichever wrote last).
    /// All transcripts, newest first, minus hidden ones.
    fn visible_sessions(&self) -> Vec<PathBuf> {
        transcript::sessions_by_recency(&self.project_dir)
            .into_iter()
            .filter(|path| !self.hidden.contains(&stem(path)))
            .collect()
    }

    /// Hides the watched session from benclaude's view (list, stats,
    /// cycling). The transcript file is NOT touched — undo with 'u'.
    fn hide_current(&mut self) {
        let Some(id) = self.session.as_ref().map(|s| s.session_id.clone()) else {
            return;
        };
        self.hidden.insert(id);
        if let Err(error) = save_hidden(&self.hidden_path, &self.hidden) {
            self.show_toast(&format!("could not persist hide: {error:#}"));
            return;
        }
        self.session = None;
        self.tailer = None;
        self.pinned = false;
        self.last_session_check = None;
        self.history = History::scan(&self.project_dir, Utc::now(), &self.hidden);
        self.report = None;
        self.show_toast("session hidden from view — press u to restore all");
    }

    /// Clears every hide and rescans.
    fn unhide_all(&mut self) {
        if self.hidden.is_empty() {
            self.show_toast("nothing hidden");
            return;
        }
        self.hidden.clear();
        if let Err(error) = save_hidden(&self.hidden_path, &self.hidden) {
            self.show_toast(&format!("could not persist unhide: {error:#}"));
            return;
        }
        self.last_session_check = None;
        self.history = History::scan(&self.project_dir, Utc::now(), &self.hidden);
        self.report = None;
        self.show_toast("all sessions visible again");
    }

    fn cycle_session(&mut self) {
        let sessions = self.visible_sessions();
        if sessions.len() < 2 {
            self.show_toast("no other session to switch to");
            return;
        }
        let current = self.tailer.as_ref().map(|t| t.path().to_path_buf());

        let index = current
            .and_then(|c| sessions.iter().position(|p| *p == c))
            .unwrap_or(0);
        let next = sessions[(index + 1) % sessions.len()].clone();
        self.watch_path(next);
        self.pinned = true;
    }

    /// Switches to the report view, running the git join if needed.
    fn open_report(&mut self) {
        if self.report.is_none() {
            match report::build(&self.cwd, &self.history) {
                Ok(data) => self.report = Some(data),
                Err(error) => {
                    self.show_toast(&format!("git join failed: {error:#}"));
                    return;
                }
            }
        }
        self.view = View::Report;
    }

    fn show_toast(&mut self, message: &str) {
        self.toast = Some(message.to_owned());
        self.toast_until = Some(Instant::now() + TOAST_FOR);
    }
}

/// Tiny by design; reach for clap when the flags stop being trivial.
fn parse_args() -> Result<(Command, PathBuf)> {
    let mut command = Command::Watch;
    let mut project = std::env::current_dir().context("cannot resolve cwd")?;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "watch" => command = Command::Watch,
            "doctor" => command = Command::Doctor,
            "report" => command = Command::Report,
            "heatmap" => command = Command::Heatmap,
            "sessions" => command = Command::Sessions,
            "--project" | "-p" => {
                project = PathBuf::from(args.next().context("--project needs a path")?);
            }
            "--help" | "-h" => {
                println!(
                    "benclaude — outcome analytics side pane for Claude Code\n\n\
                     Usage: benclaude [command] [--project <path>]\n\n\
                     watch     live TUI for the newest session (default)\n\
                     report    AI commits + line survival, plain text\n\
                     heatmap   per-file agent friction, plain text\n\
                     sessions  per-session results, plain text\n\
                     doctor    print what benclaude can see, no TUI"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok((command, project))
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `~/.config/benclaude/hidden` — one hidden session id per line. Session
/// ids are UUIDs, so one global file covers every project.
fn hidden_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/benclaude/hidden"))
}

fn load_hidden(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn save_hidden(path: &Path, hidden: &HashSet<String>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut ids: Vec<&str> = hidden.iter().map(String::as_str).collect();
    ids.sort_unstable();
    std::fs::write(path, ids.join("\n"))?;
    Ok(())
}

/// Non-TUI sanity check: what project dir resolves to, which sessions are
/// visible, and whether the newest one parses.
fn doctor(cwd: &Path, project_dir: &Path, hidden: &HashSet<String>) -> Result<()> {
    println!("cwd          {}", cwd.display());
    println!("project dir  {}", project_dir.display());
    println!("exists       {}", project_dir.is_dir());
    match git::repo_root(cwd) {
        Some(root) => println!("git repo     {}", root.display()),
        None => println!("git repo     none"),
    }
    let sessions = transcript::recent_sessions(project_dir, 30);
    println!("sessions 30d {}", sessions.len());
    println!("hidden       {}", hidden.len());
    if let Some(latest) = transcript::latest_session(project_dir) {
        println!("latest       {}", latest.display());
        let mut tailer = Tailer::new(latest);
        let events = tailer.poll()?;
        let mut session = SessionState::new(String::new());
        for event in &events {
            session.apply(event);
        }
        println!("events       {}", events.len());
        println!("prompts      {}", session.prompts);
        println!("tool calls   {}", session.tools.len());
        println!(
            "tokens       {} in / {} out",
            session.input_tokens, session.output_tokens
        );
        println!("files (turn) {:?}", session.files_this_turn);
    } else {
        println!("latest       none found");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_round_trips() {
        let path = std::env::temp_dir().join(format!("benclaude-hidden-{}", std::process::id()));
        let mut set = HashSet::new();
        set.insert("abc".to_owned());
        set.insert("def".to_owned());
        save_hidden(&path, &set).expect("save");
        assert_eq!(load_hidden(&path), set);
        std::fs::remove_file(&path).expect("cleanup");
    }
}
