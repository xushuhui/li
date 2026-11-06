mod config;
mod tui;
mod ui;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use baize_core::prompt::build_system_prompt;
use baize_core::{
    compact, Content, Message, Role, Session, encode_dir, latest_summary, list_summaries,
    sessions_root,
};
use baize_provider::{Delta, OpenAiProvider, ToolCall, WireMessage};
use baize_tools::{Approver, SnapshotStore, Tool, ToolContext, ToolOutcome};
use clap::Parser;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const MAX_TOOL_ROUNDS: usize = 64;

/// Room reserved for the system prompt and the tool definitions.
const SYSTEM_AND_TOOLS_TOKENS: usize = 800;

#[derive(Clone, Copy)]
struct Budget {
    context_window: u32,
    compact_at_percent: u8,
}

#[derive(Clone, Copy)]
struct Settings<'a> {
    system: &'a str,
    model: &'a str,
    budget: Budget,
    ui: &'a dyn ui::Ui,
}

#[derive(Parser)]
#[command(name = "baize", version, about = "终端里的编码助手")]
struct Cli {
    /// 要发送给模型的问题；省略则进入交互模式
    prompt: Option<String>,

    /// 覆盖配置文件里的默认模型
    #[arg(long)]
    model: Option<String>,

    /// 继续当前目录最近的会话
    #[arg(long = "continue", conflicts_with = "resume")]
    continue_session: bool,

    /// 从最近 10 个会话里选一个继续
    #[arg(long, conflicts_with = "continue_session")]
    resume: bool,

    /// 全屏界面：上方对话记录，下方输入行
    #[arg(long)]
    tui: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let home = home_dir()?;
    let config_path = config::config_dir(&home).join("config.toml");

    let config = match config::load(&config_path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            config::write_default(&config_path)?;
            eprintln!("已创建默认配置：{}", config_path.display());
            config::load(&config_path)?.expect("刚写入的配置应当可读")
        }
        Err(error) => {
            eprintln!("配置错误：{error:#}");
            return Ok(ExitCode::from(2));
        }
    };

    if let Err(message) = config.validate() {
        eprintln!("配置错误：{message}");
        return Ok(ExitCode::from(2));
    }

    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| config.model.default.clone());
    let api_key = std::env::var("BAIZE_API_KEY")
        .ok()
        .filter(|value| !value.is_empty());
    let provider = OpenAiProvider::new(config.model.base_url.clone(), api_key)?;
    let budget = Budget {
        context_window: config.model.context_window,
        compact_at_percent: config.context.compact_at_percent,
    };

    let cwd = std::env::current_dir()?;
    let cwd_text = cwd.to_string_lossy().into_owned();
    let session_dir = sessions_root(&home).join(encode_dir(&cwd_text));
    let mut session = if cli.continue_session {
        let Some(summary) = latest_summary(&session_dir)? else {
            eprintln!("当前目录还没有会话，直接运行 `baize` 开一个新的。");
            return Ok(ExitCode::from(1));
        };
        eprintln!("继续会话：{}", if summary.title.is_empty() { &summary.id } else { &summary.title });
        Session::load(&summary.path)?
    } else if cli.resume {
        match choose_session(&session_dir)? {
            Some(path) => Session::load(&path)?,
            None => return Ok(ExitCode::SUCCESS),
        }
    } else {
        Session::create(&session_dir, &cwd_text, env!("CARGO_PKG_VERSION"))?
    };

    if cli.model.is_some() && model != config.model.default {
        session.append_model_change(&model)?;
    }

    let global_rules = read_optional(&config::config_dir(&home).join("AGENTS.md"));
    let project_rules: Vec<(String, String)> =
        config::project_rules(&cwd, &config.project.rules_file)
            .into_iter()
            .filter_map(|path| {
                let content = std::fs::read_to_string(&path).ok()?;
                (!content.trim().is_empty())
                    .then(|| (path.display().to_string(), content))
            })
            .collect();
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let system = build_system_prompt(
        &cwd_text,
        &now,
        global_rules.as_deref(),
        &project_rules,
    );

    let tools = baize_tools::registry();
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let inner: Arc<dyn Approver> = if cli.tui {
        Arc::new(tui::TuiApprover::new(ui_tx.clone()))
    } else {
        Arc::new(StdinApprover)
    };
    let approver: Arc<dyn Approver> = Arc::new(PolicyApprover {
        mode: config.tools.bash_approval,
        inner,
    });
    let context = ToolContext {
        cwd: cwd.clone(),
        snapshots: Arc::new(Mutex::new(SnapshotStore::default())),
        approver,
        lsp: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    };

    if cli.tui {
        tui::run(tui::Launch {
            session,
            provider,
            system,
            model,
            budget,
            tools,
            context,
            ui: tui::TuiUi::new(ui_tx),
            events: ui_rx,
        })
        .await?;
        return Ok(ExitCode::SUCCESS);
    }

    let ui = ui::PlainUi;

    if let Some(prompt) = cli.prompt.as_deref() {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(ExitCode::SUCCESS);
        }
        turn(
            &mut session,
            &provider,
            Settings {
                system: &system,
                model: &model,
                budget,
                ui: &ui,
            },
            &tools,
            &context,
            prompt,
        )
        .await?;
        return Ok(ExitCode::SUCCESS);
    }

    repl(
        &mut session,
        &provider,
        Settings {
            system: &system,
            model: &model,
            budget,
            ui: &ui,
        },
        &tools,
        &context,
    )
    .await?;
    Ok(ExitCode::SUCCESS)
}

async fn repl(
    session: &mut Session,
    provider: &OpenAiProvider,
    settings: Settings<'_>,
    tools: &[Box<dyn Tool>],
    context: &ToolContext,
) -> Result<()> {
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            println!();
            break;
        }

        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        turn(session, provider, settings, tools, context, text).await?;
    }
    Ok(())
}

async fn turn(
    session: &mut Session,
    provider: &OpenAiProvider,
    settings: Settings<'_>,
    tools: &[Box<dyn Tool>],
    context: &ToolContext,
    text: &str,
) -> Result<()> {
    let mut corrected = 0u64;
    let incoming = compact::estimate_text(text) + 4;
    if maybe_compact(session, provider, settings, corrected, incoming).await? {
        corrected = 0;
    }
    session.append_message(Message::user(text))?;

    for _ in 0..MAX_TOOL_ROUNDS {
        let history = build_history(session);
        let definitions: Vec<Value> = tools
            .iter()
            .map(|tool| baize_tools::definition(tool.as_ref()))
            .collect();

        let mut stream = provider.stream(
            settings.model,
            settings.system,
            history,
            definitions,
        );
        let mut answer = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut usage = None;
        let mut interrupted = false;
        let mut failure = None;

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    interrupted = true;
                    stream.abort();
                    break;
                }
                item = stream.rx.recv() => {
                    match item {
                        Some(Ok(Delta::Text(chunk))) => {
                            settings.ui.assistant(&chunk);
                            answer.push_str(&chunk);
                        }
                        Some(Ok(Delta::Usage(value))) => usage = Some(value),
                        Some(Ok(Delta::ToolCalls(value))) => calls = value,
                        Some(Err(error)) => {
                            failure = Some(error);
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        if interrupted {
            if !answer.is_empty() {
                settings.ui.assistant_end();
            }
            if calls.is_empty() {
                let mut message = Message::assistant(answer, usage);
                message.interrupted = true;
                session.append_message(message)?;
            } else {
                let content = calls.iter().map(to_content).collect();
                session.append_message(Message::assistant_turn(answer, content, usage))?;
                for call in &calls {
                    session.append_message(Message::tool_result(
                        &call.id,
                        &call.name,
                        "执行被中断",
                        true,
                    ))?;
                }
            }
            settings.ui.status("已中断，保留已生成的内容。");
            if tokio::time::timeout(std::time::Duration::from_secs(1), tokio::signal::ctrl_c())
                .await
                .is_ok()
            {
                std::process::exit(130);
            }
            return Ok(());
        }

        if let Some(error) = failure {
            if !answer.is_empty() {
                settings.ui.assistant_end();
            }
            return Err(error.into());
        }

        if !answer.is_empty() {
            settings.ui.assistant_end();
        }

        if let Some(value) = &usage {
            corrected = value.input;
        }

        if calls.is_empty() {
            session.append_message(Message::assistant(answer, usage))?;
            return Ok(());
        }

        let content: Vec<Content> = calls.iter().map(to_content).collect();
        session.append_message(Message::assistant_turn(answer, content, usage))?;

        for outcome in execute_batch(tools, context, settings.ui, &calls).await {
            let call = &calls[outcome.0];
            session.append_message(Message::tool_result(
                &call.id,
                &call.name,
                outcome.1.text,
                outcome.1.is_error,
            ))?;
        }

        if maybe_compact(session, provider, settings, corrected, 0).await? {
            corrected = 0;
        }
    }

    settings.ui.status(&format!("工具调用超过 {MAX_TOOL_ROUNDS} 轮，已停止。"));
    Ok(())
}

fn build_history(session: &Session) -> Vec<WireMessage> {
    let mut history = Vec::new();
    if let Some(summary) = session.summary() {
        history.push(WireMessage::text(
            "user",
            format!("以下是更早对话的压缩摘要，作为你的背景：\n\n{summary}"),
        ));
    }
    history.extend(session.messages().iter().map(|message| to_wire(message)));
    history
}

fn context_tokens_with(session: &Session, corrected: u64, extra: usize) -> usize {
    (context_tokens(session, corrected) + extra).max(corrected as usize)
}

fn context_tokens(session: &Session, corrected: u64) -> usize {
    let summary = session
        .summary()
        .map(compact::estimate_text)
        .unwrap_or(0);
    let messages: usize = session
        .messages()
        .iter()
        .map(|message| compact::estimate_message(message))
        .sum();
    (summary + messages + SYSTEM_AND_TOOLS_TOKENS).max(corrected as usize)
}

async fn maybe_compact(
    session: &mut Session,
    provider: &OpenAiProvider,
    settings: Settings<'_>,
    corrected: u64,
    extra: usize,
) -> Result<bool> {
    if session.messages().is_empty() {
        return Ok(false);
    }

    let window = settings.budget.context_window as usize;
    let threshold = window * settings.budget.compact_at_percent.min(100) as usize / 100;
    let tokens = context_tokens_with(session, corrected, extra);
    if tokens <= threshold {
        return Ok(false);
    }

    settings.ui.status(&format!("上下文约 {tokens} tokens，超过阈值 {threshold}，开始压缩……"));
    let transcript = compact::render_transcript(session.summary(), &session.messages(), window);
    let summary = run_summary(provider, settings.model, &transcript).await?;
    if summary.trim().is_empty() {
        settings.ui.status("压缩失败：模型没有返回摘要，继续使用完整上下文。");
        return Ok(false);
    }

    session.append_compaction(&summary)?;
    settings.ui.status(&format!(
        "压缩完成：摘要 {} 字符，原消息保留在会话文件里。",
        summary.chars().count()
    ));
    Ok(true)
}

async fn run_summary(provider: &OpenAiProvider, model: &str, transcript: &str) -> Result<String> {
    let prompt = format!("{}{transcript}", compact::SUMMARY_PROMPT);
    let messages = vec![WireMessage::text("user", prompt)];
    let mut stream = provider.stream(model, "", messages, Vec::new());
    let mut out = String::new();
    while let Some(item) = stream.rx.recv().await {
        match item {
            Ok(Delta::Text(chunk)) => out.push_str(&chunk),
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(out.trim().to_string())
}

async fn execute_batch(
    tools: &[Box<dyn Tool>],
    context: &ToolContext,
    ui: &dyn ui::Ui,
    calls: &[ToolCall],
) -> Vec<(usize, ToolOutcome)> {
    let mut results = Vec::new();
    let mut index = 0;

    while index < calls.len() {
        if is_read_only(&calls[index].name) {
            let mut batch = Vec::new();
            while index < calls.len() && is_read_only(&calls[index].name) {
                batch.push(index);
                index += 1;
            }
            let futures = batch
                .iter()
                .map(|position| execute_one(tools, context, ui, &calls[*position]));
            let outcomes = futures::future::join_all(futures).await;
            results.extend(batch.into_iter().zip(outcomes));
        } else {
            let outcome = execute_one(tools, context, ui, &calls[index]).await;
            results.push((index, outcome));
            index += 1;
        }
    }

    results
}

async fn execute_one(
    tools: &[Box<dyn Tool>],
    context: &ToolContext,
    ui: &dyn ui::Ui,
    call: &ToolCall,
) -> ToolOutcome {
    ui.tool(&format!("▶ {} {}", call.name, summarize(&call.arguments)));

    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        return ToolOutcome::error(format!("未知工具：{}", call.name));
    };

    let args: Value = match serde_json::from_str(&call.arguments) {
        Ok(value) => value,
        Err(error) => return ToolOutcome::error(format!("参数不是有效 JSON：{error}")),
    };

    let outcome = tool.execute(args, context).await;
    preview(ui, &outcome);
    outcome
}

fn preview(ui: &dyn ui::Ui, outcome: &ToolOutcome) {
    let lines: Vec<&str> = outcome.text.lines().collect();
    if !outcome.is_error && lines.len() <= 5 {
        return;
    }
    let head = lines.iter().take(3);
    let tail = lines.iter().rev().take(3).rev();
    for line in head {
        ui.tool(&format!("  {line}"));
    }
    if lines.len() > 6 {
        ui.tool("  ...");
    }
    for line in tail {
        ui.tool(&format!("  {line}"));
    }
}

fn summarize(arguments: &str) -> String {
    let flattened = arguments.replace(['\n', '\r'], " ");
    let mut brief: String = flattened.chars().take(80).collect();
    if flattened.chars().count() > 80 {
        brief.push('…');
    }
    brief
}

fn is_read_only(name: &str) -> bool {
    matches!(name, "read" | "grep" | "glob")
}

fn to_content(call: &ToolCall) -> Content {
    Content::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: serde_json::from_str(&call.arguments).unwrap_or(Value::Null),
    }
}

fn to_wire(message: &Message) -> WireMessage {
    match message.role {
        Role::User => WireMessage::text("user", message.text()),
        Role::ToolResult => WireMessage::tool_result(
            message.tool_call_id.clone().unwrap_or_default(),
            message.text(),
        ),
        Role::Assistant => {
            let calls = message
                .tool_calls()
                .iter()
                .map(|(id, name, arguments)| ToolCall {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    arguments: serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()),
                })
                .collect();
            WireMessage::assistant(Some(message.text()), calls)
        }
    }
}

struct PolicyApprover {
    mode: config::BashApproval,
    inner: Arc<dyn Approver>,
}

impl Approver for PolicyApprover {
    fn approve(&self, command: &str) -> bool {
        match self.mode {
            config::BashApproval::Auto => true,
            config::BashApproval::Danger => {
                !baize_tools::bash::is_dangerous(command) || self.inner.approve(command)
            }
        }
    }
}

struct StdinApprover;

impl Approver for StdinApprover {
    fn approve(&self, command: &str) -> bool {
        eprint!("执行命令？\n  {command}\n[y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

fn choose_session(dir: &Path) -> Result<Option<PathBuf>> {
    let summaries = list_summaries(dir, 10)?;
    if summaries.is_empty() {
        eprintln!("当前目录还没有会话，直接运行 `baize` 开一个新的。");
        return Ok(None);
    }

    for (index, summary) in summaries.iter().enumerate() {
        let when = summary
            .timestamp
            .get(0..16)
            .unwrap_or(&summary.timestamp)
            .replace('T', " ");
        eprintln!(
            "{:2}  {when}  {}  {}",
            index + 1,
            summary.cwd,
            summary.title
        );
    }

    for attempt in 0..2 {
        eprint!("选择 [1-{}，直接回车取消]：", summaries.len());
        std::io::stderr().flush()?;

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        match trimmed.parse::<usize>() {
            Ok(index) if (1..=summaries.len()).contains(&index) => {
                return Ok(Some(summaries[index - 1].path.clone()));
            }
            _ if attempt == 0 => eprintln!("编号超出范围"),
            _ => {}
        }
    }

    eprintln!("已取消");
    Ok(None)
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME 环境变量未设置")
}

fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}
