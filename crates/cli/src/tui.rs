//! Full-screen TUI: the transcript on top, an input line at the bottom.
//!
//! The agent runs in its own task; this module only renders `UiEvent`s and
//! turns keystrokes into prompts.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::Result;
use li_core::Session;
use li_core::skills::Skill;
use li_provider::OpenAiProvider;
use li_tools::{Approver, Tool, ToolContext};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;

use crate::slash::{self, Action};
use crate::ui::Ui;
use crate::{Budget, Settings, turn};

/// Everything the agent task needs, moved out of `main`.
pub struct Launch {
    pub session: Session,
    pub provider: OpenAiProvider,
    pub system: String,
    pub model: String,
    pub budget: Budget,
    pub tools: Vec<Box<dyn Tool>>,
    pub skills: Vec<Skill>,
    pub context: ToolContext,
    pub ui: TuiUi,
    pub events: mpsc::UnboundedReceiver<UiEvent>,
}

pub enum UiEvent {
    Assistant(String),
    AssistantEnd,
    Tool(String),
    Status(String),
    /// Refreshed numbers for the status bar; sent once per turn.
    Bar {
        used: u64,
        changed: usize,
    },
    AskApproval {
        command: String,
        reply: std_mpsc::Sender<bool>,
    },
}

pub struct TuiUi {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl TuiUi {
    pub fn new(tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }

    pub fn bar(&self, used: u64, changed: usize) {
        let _ = self.tx.send(UiEvent::Bar { used, changed });
    }
}

impl Ui for TuiUi {
    fn assistant(&self, text: &str) {
        let _ = self.tx.send(UiEvent::Assistant(text.to_string()));
    }

    fn assistant_end(&self) {
        let _ = self.tx.send(UiEvent::AssistantEnd);
    }

    fn tool(&self, line: &str) {
        let _ = self.tx.send(UiEvent::Tool(line.to_string()));
    }

    fn status(&self, line: &str) {
        let _ = self.tx.send(UiEvent::Status(line.to_string()));
    }
}

/// Blocks the asking task until the TUI answers with y/n.
pub struct TuiApprover {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl TuiApprover {
    pub fn new(tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }
}

impl Approver for TuiApprover {
    fn approve(&self, command: &str) -> bool {
        let (reply, wait) = std_mpsc::channel();
        if self
            .tx
            .send(UiEvent::AskApproval {
                command: command.to_string(),
                reply,
            })
            .is_err()
        {
            return false;
        }
        wait.recv().unwrap_or(false)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Assistant,
    Tool,
    User,
}

struct Entry {
    kind: Kind,
    text: String,
}

#[derive(Default)]
struct App {
    entries: Vec<Entry>,
    input: String,
    status: String,
    pending: Option<(String, std_mpsc::Sender<bool>)>,
    skills: Vec<Skill>,
    /// Startup facts for the status bar.
    cwd: PathBuf,
    branch: Option<String>,
    model: String,
    window: u32,
    /// Refreshed after each turn.
    used: u64,
    changed: usize,
}

impl App {
    fn push(&mut self, kind: Kind, text: &str) {
        self.entries.push(Entry {
            kind,
            text: text.to_string(),
        });
    }

    fn assistant(&mut self, text: &str) {
        if let Some(last) = self.entries.last_mut()
            && last.kind == Kind::Assistant
        {
            last.text.push_str(text);
            return;
        }
        self.push(Kind::Assistant, text);
    }

    fn assistant_end(&mut self) {
        self.push(Kind::Assistant, "");
    }

    fn handle_key(&mut self, key: KeyEvent, input: &mpsc::UnboundedSender<String>) -> bool {
        if let Some((_, reply)) = self.pending.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let _ = reply.send(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                    let _ = reply.send(false);
                }
                _ => {
                    self.pending = Some((String::new(), reply));
                }
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => return self.input.is_empty(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                self.input.clear();
                if text == "exit" || text == "quit" {
                    return true;
                }
                if text.is_empty() {
                    return false;
                }
                match slash::dispatch(&text, &self.skills, &self.cwd) {
                    Action::Exit => return true,
                    Action::Print(output) => {
                        for line in output.lines() {
                            self.push(Kind::Tool, line);
                        }
                    }
                    Action::Send(message) => {
                        self.push(Kind::User, &format!("> {text}"));
                        let _ = input.send(message);
                    }
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Tab => {
                if let Some(first) = slash::completions(&self.input, &self.skills).first() {
                    self.input = first.clone();
                }
            }
            KeyCode::Char(ch) => self.input.push(ch),
            _ => {}
        }
        false
    }
}

pub async fn run(launch: Launch) -> Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let result = serve(&mut terminal, launch).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn serve(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    launch: Launch,
) -> Result<()> {
    let Launch {
        session,
        provider,
        system,
        model,
        budget,
        tools,
        skills,
        context,
        ui,
        mut events,
    } = launch;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    let cwd = context.cwd.clone();
    let agent_cwd = cwd.clone();
    // Git lookups run once per turn, between turns, so blocking here is fine.
    let branch = git_branch(&cwd);
    let changed = git_changed(&cwd);
    let used = latest_input_tokens(&session);
    let window = budget.context_window;
    let model_name = model.clone();

    let agent = tokio::spawn(async move {
        let mut session = session;
        while let Some(text) = input_rx.recv().await {
            // Clear the previous turn's status line; the bar takes the row back.
            ui.status("");
            let result = {
                let settings = Settings {
                    system: &system,
                    model: &model,
                    budget,
                    ui: &ui,
                };
                turn(&mut session, &provider, settings, &tools, &context, &text).await
            };
            if let Err(error) = result {
                ui.status(&format!("出错：{error:#}"));
            }
            ui.bar(latest_input_tokens(&session), git_changed(&agent_cwd));
        }
    });

    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => {
                        if key_tx.send(key).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut app = App {
        skills,
        cwd,
        branch,
        model: model_name,
        window,
        used,
        changed,
        ..App::default()
    };
    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        let mut quit = false;
        tokio::select! {
            Some(event) = events.recv() => {
                match event {
                    UiEvent::Assistant(text) => app.assistant(&text),
                    UiEvent::AssistantEnd => app.assistant_end(),
                    UiEvent::Tool(line) => app.push(Kind::Tool, &line),
                    UiEvent::Status(line) => app.status = line,
                    UiEvent::Bar { used, changed } => {
                        app.used = used;
                        app.changed = changed;
                    }
                    UiEvent::AskApproval { command, reply } => {
                        app.pending = Some((command, reply));
                    }
                }
            }
            Some(key) = key_rx.recv() => {
                quit = app.handle_key(key, &input_tx);
            }
            else => break,
        }
        if quit {
            break;
        }
    }

    drop(input_tx);
    let _ = agent.await;
    Ok(())
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let [body, input_area, bar] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.entries {
        let style = match entry.kind {
            Kind::Assistant => Style::default(),
            Kind::Tool => Style::default().fg(Color::DarkGray),
            Kind::User => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        };
        for text in entry.text.split('\n') {
            lines.push(Line::from(Span::styled(text.to_string(), style)));
        }
    }

    let inner_width = (body.width.saturating_sub(2) as usize).max(1);
    let inner_height = body.height.saturating_sub(2) as usize;
    let mut total = 0usize;
    for entry in &app.entries {
        for text in entry.text.split('\n') {
            let width = text.chars().count().max(1);
            total += width.div_ceil(inner_width);
        }
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let offset = total.saturating_sub(inner_height) as u16;
    let paragraph = paragraph
        .block(Block::default().borders(Borders::ALL).title(" li "))
        .scroll((offset, 0));
    frame.render_widget(paragraph, body);

    frame.render_widget(Paragraph::new(app.input.as_str()), input_area);
    let cursor = input_area.x + app.input.chars().count() as u16;
    frame.set_cursor_position((cursor.min(input_area.right().saturating_sub(1)), input_area.y));

    let completions = slash::completions(&app.input, &app.skills);
    let line = match &app.pending {
        Some((command, _)) => Line::from(format!("确认执行？ {command}  [y/N]")),
        None if !completions.is_empty() => hint_line(&completions),
        None if !app.status.is_empty() => Line::from(app.status.clone()),
        None => bar_line(app, bar.width),
    };
    frame.render_widget(Paragraph::new(line), bar);
}

/// One row: where we are on the left, how much context is gone on the right.
fn bar_line(app: &App, width: u16) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let left = bar_left(app);
    let mut right = bar_right(app, true);
    if left.chars().count() + right.chars().count() + 1 > width as usize {
        right = bar_right(app, false);
    }
    let used = left.chars().count() + right.chars().count();
    let gap = (width as usize).saturating_sub(used).max(1);
    Line::from(vec![
        Span::styled(left, dim),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, dim),
    ])
}

fn bar_left(app: &App) -> String {
    let mut left = shorten_home(&app.cwd);
    if let Some(branch) = &app.branch {
        left.push_str("  ");
        left.push_str(branch);
        if app.changed > 0 {
            left.push_str(&format!(" *{}", app.changed));
        }
    }
    left
}

fn bar_right(app: &App, with_model: bool) -> String {
    let mut right = format!(
        "{}%  {}",
        percent(app.used, app.window),
        human_window(app.window)
    );
    if with_model {
        right.push_str("  ");
        right.push_str(&app.model);
    }
    right
}

fn percent(used: u64, window: u32) -> u64 {
    if window == 0 {
        return 0;
    }
    used.saturating_mul(100) / u64::from(window)
}

fn human_window(window: u32) -> String {
    if window >= 1_000_000 && window.is_multiple_of(1_000_000) {
        format!("{}M", window / 1_000_000)
    } else if window >= 1_000 && window.is_multiple_of(1_000) {
        format!("{}K", window / 1_000)
    } else {
        window.to_string()
    }
}

fn shorten_home(path: &Path) -> String {
    let text = path.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text.into_owned(),
    }
}

/// Input tokens of the newest request, which is the size of the live context.
fn latest_input_tokens(session: &Session) -> u64 {
    session
        .messages()
        .iter()
        .rev()
        .find_map(|message| message.usage.as_ref())
        .map_or(0, |usage| usage.input)
}

fn git_branch(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn git_changed(cwd: &Path) -> usize {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).lines().count()
        }
        _ => 0,
    }
}

/// Hint line: the first candidate is selected, so Tab fills it in.
fn hint_line(completions: &[String]) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    for (index, name) in completions.iter().take(slash::MAX_HINTS).enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if index == 0 {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            dim
        };
        spans.push(Span::styled(name.clone(), style));
    }
    if completions.len() > slash::MAX_HINTS {
        spans.push(Span::styled(
            format!("  …共 {} 项", completions.len()),
            dim,
        ));
    }
    spans.push(Span::styled("  Tab 补全", dim));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App) -> String {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let raw: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // Wide characters leave a blank cell behind; compare without spacing.
        raw.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn renders_transcript_input_and_status() {
        let mut app = App::default();
        app.push(Kind::User, "> 你好");
        app.assistant("答复内容");
        app.input = "下一个问题".to_string();
        app.status = "就绪".to_string();

        let screen = render(&app);
        assert!(screen.contains("你好"), "{screen}");
        assert!(screen.contains("答复内容"), "{screen}");
        assert!(screen.contains("下一个问题"), "{screen}");
        assert!(screen.contains("就绪"), "{screen}");
    }

    fn demo_skill() -> Skill {
        Skill {
            name: "tdd".to_string(),
            description: String::new(),
            path: "/skills/tdd/SKILL.md".into(),
        }
    }

    #[test]
    fn slash_input_shows_completion_hints() {
        let app = App {
            skills: vec![demo_skill()],
            input: "/s".to_string(),
            ..App::default()
        };

        let screen = render(&app);
        assert!(screen.contains("/skill:tdd"), "{screen}");
        assert!(screen.contains("Tab补全"), "{screen}");
    }

    #[test]
    fn tab_fills_the_first_completion() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App {
            skills: vec![demo_skill()],
            input: "/he".to_string(),
            ..App::default()
        };

        let quit = app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert!(!quit);
        assert_eq!(app.input, "/help");
    }

    #[test]
    fn bar_shows_branch_changes_and_context() {
        let app = App {
            cwd: PathBuf::from("/tmp/repo"),
            branch: Some("master".to_string()),
            model: "deepseek".to_string(),
            window: 1_000_000,
            used: 350_000,
            changed: 3,
            ..App::default()
        };

        let left = bar_left(&app);
        assert!(left.ends_with("master *3"), "{left}");
        let right = bar_right(&app, true);
        assert!(right.starts_with("35%  1M"), "{right}");
        assert!(right.ends_with("deepseek"), "{right}");
    }

    #[test]
    fn bar_drops_the_model_when_the_row_is_too_narrow() {
        let app = App {
            cwd: PathBuf::from("/tmp/repo"),
            model: "deepseek".to_string(),
            window: 1_000_000,
            ..App::default()
        };

        let line = bar_line(&app, 20);
        let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("0%  1M"), "{text}");
        assert!(!text.contains("deepseek"), "{text}");
        assert_eq!(text.chars().count(), 20, "{text}");
    }

    #[test]
    fn idle_row_renders_the_bar() {
        let app = App {
            cwd: PathBuf::from("/tmp/repo"),
            branch: Some("master".to_string()),
            model: "m1".to_string(),
            window: 200_000,
            used: 50_000,
            changed: 2,
            ..App::default()
        };

        let screen = render(&app);
        assert!(screen.contains("master*2"), "{screen}");
        assert!(screen.contains("25%200K"), "{screen}");
        assert!(screen.contains("m1"), "{screen}");
    }

    #[test]
    fn status_line_replaces_the_bar() {
        let app = App {
            status: "出错：网络断开".to_string(),
            model: "m1".to_string(),
            window: 200_000,
            ..App::default()
        };

        let screen = render(&app);
        assert!(screen.contains("出错"), "{screen}");
        assert!(!screen.contains("0%"), "{screen}");
    }

    #[test]
    fn init_refusal_lands_in_the_transcript() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("AGENTS.md"), "# 已有").expect("agents file");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App {
            cwd: dir.path().to_path_buf(),
            input: "/init".to_string(),
            ..App::default()
        };

        let quit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(!quit);
        assert!(app.input.is_empty());
        assert!(
            app.entries.iter().any(|entry| entry.text.contains("已存在")),
            "{:?}",
            app.entries.iter().map(|entry| &entry.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn input_line_has_no_border_or_title() {
        let app = App {
            input: "hello".to_string(),
            ..App::default()
        };
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");

        let buffer = terminal.backend().buffer();
        let row: String = buffer
            .content()
            .iter()
            .skip(8 * 40)
            .take(40)
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(row, format!("hello{}", " ".repeat(35)));
    }

    #[test]
    fn approval_prompt_takes_over_the_status_bar() {
        let mut app = App::default();
        let (reply, _wait) = std_mpsc::channel();
        app.pending = Some(("rm -rf build".to_string(), reply));

        let screen = render(&app);
        assert!(screen.contains("确认执行"), "{screen}");
        assert!(screen.contains("rm-rfbuild"), "{screen}");
    }
}
