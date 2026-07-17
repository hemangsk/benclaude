//! benclaude — a live outcome-analytics side pane for Claude Code.
//!
//! Runs in a narrow terminal split beside a Claude Code session, tails the
//! session transcript read-only, and annotates the live activity with
//! 30-day history from past transcripts of the same project.

mod state;
mod transcript;
mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use state::{History, SessionState};
use transcript::Tailer;

const TICK: Duration = Duration::from_millis(250);
const RESCAN_SESSIONS: Duration = Duration::from_secs(5);
const TOAST_FOR: Duration = Duration::from_secs(3);

/// Shared app state handed to the renderer.
#[derive(Debug)]
pub(crate) struct App {
    pub(crate) project_name: String,
    pub(crate) session: Option<SessionState>,
    pub(crate) history: History,
    pub(crate) toast: Option<String>,
    toast_until: Option<Instant>,
    project_dir: PathBuf,
    tailer: Option<Tailer>,
    last_session_check: Option<Instant>,
}

fn main() -> Result<()> {
    let cwd = parse_args()?;
    let project_dir = transcript::project_dir(&cwd)?;

    if std::env::args().nth(1).as_deref() == Some("doctor") {
        return doctor(&cwd, &project_dir);
    }
    if !std::io::stdout().is_terminal() {
        bail!("benclaude watch needs a TTY — run it inside a terminal pane");
    }

    let project_name = cwd.file_name().map_or_else(
        || cwd.display().to_string(),
        |n| {
            let parent = cwd
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned());
            parent.map_or_else(
                || n.to_string_lossy().into_owned(),
                |p| format!("{p}/{}", n.to_string_lossy()),
            )
        },
    );

    let mut app = App {
        project_name,
        session: None,
        history: History::scan(&project_dir, Utc::now()),
        toast: None,
        toast_until: None,
        project_dir,
        tailer: None,
        last_session_check: None,
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.refresh()?;
        terminal.draw(|frame| ui::render(frame, app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char('r' | 'h' | 's') => {
                    // ponytail: report/heatmap/sessions views land with the
                    // git-join indexer in v0.2.
                    app.show_toast("coming in v0.2 — needs the git indexer");
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

    fn show_toast(&mut self, message: &str) {
        self.toast = Some(message.to_owned());
        self.toast_until = Some(Instant::now() + TOAST_FOR);
    }
}

/// `benclaude [watch|doctor] [--project <path>]` — tiny by design; reach for
/// clap when a third subcommand shows up.
fn parse_args() -> Result<PathBuf> {
    let mut project = std::env::current_dir().context("cannot resolve cwd")?;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "watch" | "doctor" => {}
            "--project" | "-p" => {
                project = PathBuf::from(args.next().context("--project needs a path")?);
            }
            "--help" | "-h" => {
                println!(
                    "benclaude — live analytics side pane for Claude Code\n\n\
                     Usage: benclaude [watch] [--project <path>]\n       \
                     benclaude doctor [--project <path>]\n\n\
                     watch    live TUI for the newest session (default)\n\
                     doctor   print what benclaude can see, no TUI"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(project)
}

/// Non-TUI sanity check: what project dir resolves to, which sessions are
/// visible, and whether the newest one parses.
fn doctor(cwd: &std::path::Path, project_dir: &std::path::Path) -> Result<()> {
    println!("cwd          {}", cwd.display());
    println!("project dir  {}", project_dir.display());
    println!("exists       {}", project_dir.is_dir());
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
