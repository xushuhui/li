//! Slash commands handled before anything reaches the model (D29).
//!
//! Only registered names are intercepted. Anything else starting with `/`
//! (a path like `/usr/local/bin`) is sent to the model unchanged.

use li_core::skills::Skill;

/// What an input line turns into.
pub enum Action {
    /// Forward this text to the model.
    Send(String),
    /// Handled locally; show this instead of calling the model.
    Print(String),
    /// Leave the session.
    Exit,
}

/// Most completions shown on the hint line.
pub const MAX_HINTS: usize = 6;

/// Commands offered as completions, in display order.
pub fn command_names(skills: &[Skill]) -> Vec<String> {
    let mut names = vec!["/help".to_string(), "/exit".to_string()];
    names.extend(skills.iter().map(|skill| format!("/skill:{}", skill.name)));
    names
}

pub fn dispatch(input: &str, skills: &[Skill]) -> Action {
    let text = input.trim();
    if !text.starts_with('/') {
        return Action::Send(text.to_string());
    }

    let (head, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(head, rest)| (head, rest.trim()));

    match head {
        "/help" => Action::Print(help(skills)),
        "/exit" | "/quit" => Action::Exit,
        _ => match head.strip_prefix("/skill:") {
            Some(name) if !name.is_empty() => match skills.iter().find(|skill| skill.name == name) {
                Some(skill) => match skill_message(skill, rest) {
                    Ok(message) => Action::Send(message),
                    Err(error) => Action::Print(error),
                },
                None => Action::Print(format!(
                    "没有找到技能 {name}。输入 /help 查看可用列表。"
                )),
            },
            _ => Action::Send(text.to_string()),
        },
    }
}

/// Completions for a bare `/...` token, best first. Empty when the line is not a slash token.
pub fn completions(input: &str, skills: &[Skill]) -> Vec<String> {
    let text = input.trim_start();
    if !text.starts_with('/') || text.contains(char::is_whitespace) {
        return Vec::new();
    }

    let pattern = text.to_lowercase();
    let mut prefix = Vec::new();
    let mut fuzzy = Vec::new();
    for name in command_names(skills) {
        let lowered = name.to_lowercase();
        if lowered.starts_with(&pattern) {
            prefix.push(name);
        } else if is_subsequence(&pattern, &lowered) {
            fuzzy.push(name);
        }
    }
    prefix.extend(fuzzy);
    prefix
}

/// Every character of `pattern` appears in `text`, in order.
fn is_subsequence(pattern: &str, text: &str) -> bool {
    let mut remaining = text.chars();
    pattern.chars().all(|needle| remaining.any(|hay| hay == needle))
}

fn help(skills: &[Skill]) -> String {
    let mut text = String::from(
        "可用命令：\n/help              列出命令\n/exit              退出\n/skill:<名字> [参数]  把技能正文注入当前对话\n",
    );
    if skills.is_empty() {
        text.push_str("\n没有发现技能。技能放在 ~/.agents/skills 或项目内的 .agents/skills。");
        return text;
    }
    text.push_str(&format!("\n技能（{}）：\n", skills.len()));
    for skill in skills {
        text.push_str(&format!("- {}", skill.name));
        if !skill.description.trim().is_empty() {
            text.push('：');
            text.push_str(skill.description.trim());
        }
        text.push('\n');
    }
    text
}

/// Skill body without frontmatter, plus the skill directory and any extra arguments.
fn skill_message(skill: &Skill, arguments: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(&skill.path)
        .map_err(|error| format!("无法读取技能文件 {}：{error}", skill.path.display()))?;
    let body = strip_frontmatter(&text).trim();
    let directory = skill
        .path
        .parent()
        .map(|path| path.display().to_string())
        .unwrap_or_default();

    let mut message = format!("技能目录：{directory}\n\n{body}");
    if !arguments.is_empty() {
        message.push_str(&format!("\n\n参数：{arguments}"));
    }
    Ok(message)
}

fn strip_frontmatter(text: &str) -> &str {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return text;
    }
    let Some(end) = trimmed[3..].find("\n---") else {
        return text;
    };
    &trimmed[3 + end + 4..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, body: &str) -> (Skill, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).expect("skill dir");
        let path = root.join("SKILL.md");
        std::fs::write(&path, body).expect("skill file");
        (
            Skill {
                name: name.to_string(),
                description: format!("{name} description"),
                path,
            },
            dir,
        )
    }

    #[test]
    fn plain_text_goes_to_the_model() {
        match dispatch("修一下这个函数", &[]) {
            Action::Send(text) => assert_eq!(text, "修一下这个函数"),
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn unregistered_slash_path_goes_to_the_model() {
        match dispatch("/usr/local/bin", &[]) {
            Action::Send(text) => assert_eq!(text, "/usr/local/bin"),
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn help_lists_commands_and_skills() {
        let (example, _dir) = skill("tdd", "---\nname: tdd\n---\n");
        match dispatch("/help", &[example]) {
            Action::Print(text) => {
                assert!(text.contains("/skill:<名字>"));
                assert!(text.contains("tdd"));
            }
            _ => panic!("expected print"),
        }
    }

    #[test]
    fn skill_command_injects_the_body_without_frontmatter() {
        let (example, _dir) = skill(
            "tdd",
            "---\nname: tdd\ndescription: TDD\n---\n\n先写失败的测试。\n",
        );
        match dispatch("/skill:tdd 这个函数", &[example]) {
            Action::Send(text) => {
                assert!(text.contains("先写失败的测试。"));
                assert!(!text.contains("name: tdd"));
                assert!(text.contains("技能目录："));
                assert!(text.contains("参数：这个函数"));
            }
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn unknown_skill_prints_a_pointer_to_help() {
        match dispatch("/skill:nope", &[]) {
            Action::Print(text) => assert!(text.contains("/help")),
            _ => panic!("expected print"),
        }
    }

    #[test]
    fn exit_leaves_the_session() {
        assert!(matches!(dispatch("/exit", &[]), Action::Exit));
    }

    #[test]
    fn completions_rank_prefix_matches_first() {
        let (example, _dir) = skill("tdd", "---\nname: tdd\n---\n");
        let matches = completions("/e", &[example]);
        assert_eq!(matches.first().map(String::as_str), Some("/exit"));
    }

    #[test]
    fn completions_match_non_contiguous_characters() {
        let (example, _dir) = skill("code-review", "---\nname: code-review\n---\n");
        let matches = completions("/cr", &[example]);
        assert!(matches.contains(&"/skill:code-review".to_string()), "{matches:?}");
    }

    #[test]
    fn completions_stay_quiet_elsewhere() {
        assert!(completions("hello", &[]).is_empty());
        assert!(completions("/skill:name arg", &[]).is_empty());
        assert!(completions("/zzz", &[]).is_empty());
    }
}
