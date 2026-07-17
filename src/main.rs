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
    toast_until: Option<Instant>,
    cwd: PathBuf,
    project_dir: PathBuf,
    tailer: Option<Tailer>,
    last_session_check: Option<Instant>,
}

fn main() -> Result<()> {
    let (command, cwd) = parse_args()?;
    let project_dir = transcript::project_dir(&cwd)?;

    match command {
        Command::Doctor => return doctor(&cwd, &project_dir),
        Command::Report | Command::Heatmap | Command::Sessions => {
            let history = History::scan(&project_dir, Utc::now());
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

    let history = History::scan(&project_dir, Utc::now());
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
        if self
            .last_session_check
            .is_none_or(|checked| checked.elapsed() >= RESCAN_SESSIONS)
        {
            self.last_session_check = Some(Instant::now());
            if let Some(latest) = transcript::latest_session(&self.project_dir)
                && self.tailer.as_ref().is_none_or(|t| t.path() != latest)
            {
                let id = latest
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.session = Some(SessionState::new(id));
                self.tailer = Some(Tailer::new(latest));
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

/// Non-TUI sanity check: what project dir resolves to, which sessions are
/// visible, and whether the newest one parses.
fn doctor(cwd: &Path, project_dir: &Path) -> Result<()> {
    println!("cwd          {}", cwd.display());
    println!("project dir  {}", project_dir.display());
    println!("exists       {}", project_dir.is_dir());
    match git::repo_root(cwd) {
        Some(root) => println!("git repo     {}", root.display()),
        None => println!("git repo     none"),
    }
    let sessions = transcript::recent_sessions(project_dir, 30);
    println!("sessions 30d {}", sessions.len());
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
