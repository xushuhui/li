//! Full-screen TUI: the transcript on top, an input line at the bottom.
//!
//! The agent runs in its own task; this module only renders `UiEvent`s and
//! turns keystrokes into prompts.

use std::io;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::Result;
use baize_core::Session;
use baize_provider::OpenAiProvider;
use baize_tools::{Approver, Tool, ToolContext};
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
    pub context: ToolContext,
    pub ui: TuiUi,
    pub events: mpsc::UnboundedReceiver<UiEvent>,
}

pub enum UiEvent {
    Assistant(String),
    AssistantEnd,
    Tool(String),
    Status(String),
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
                if !text.is_empty() {
                    self.push(Kind::User, &format!("> {text}"));
                    let _ = input.send(text);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
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
        context,
        ui,
        mut events,
    } = launch;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();

    let agent = tokio::spawn(async move {
        let mut session = session;
        while let Some(text) = input_rx.recv().await {
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

    let mut app = App::default();
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
        Constraint::Length(3),
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
        .block(Block::default().borders(Borders::ALL).title(" baize "))
        .scroll((offset, 0));
    frame.render_widget(paragraph, body);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(" 输入 "));
    frame.render_widget(input, input_area);
    let cursor = input_area.x + 1 + app.input.chars().count() as u16;
    frame.set_cursor_position((cursor.min(input_area.right().saturating_sub(2)), input_area.y + 1));

    let status = match &app.pending {
        Some((command, _)) => format!("确认执行？ {command}  [y/N]"),
        None => app.status.clone(),
    };
    frame.render_widget(Paragraph::new(status), bar);
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
