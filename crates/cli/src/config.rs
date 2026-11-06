use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_CONFIG: &str = r#"# li 配置。api_key 从环境变量 LI_API_KEY 读取，不写进文件。
[model]
default = "workbuddy/deepseek-v4.1-flash"
base_url = "http://127.0.0.1:3425/v1"
context_window = 1000000

[context]
compact_at_percent = 80

[tools]
# auto：全部直接执行；danger：默认直接执行，rm、sudo、git reset --hard 等危险命令才询问。
bash_approval = "danger"

[project]
rules_file = "AGENTS.md"
"#;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub model: ModelConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub project: ProjectConfig,
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub default: String,
    pub base_url: String,
    pub context_window: u32,
}

#[derive(Debug, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_compact_at")]
    pub compact_at_percent: u8,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_at_percent: default_compact_at(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BashApproval {
    /// Run everything without asking.
    Auto,
    /// Run everything except recognized destructive commands.
    Danger,
}

#[derive(Debug, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_bash_approval")]
    pub bash_approval: BashApproval,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            bash_approval: default_bash_approval(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ProjectConfig {
    #[serde(default = "default_rules_file")]
    pub rules_file: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            rules_file: default_rules_file(),
        }
    }
}

fn default_compact_at() -> u8 {
    80
}

fn default_rules_file() -> String {
    "AGENTS.md".to_string()
}

fn default_bash_approval() -> BashApproval {
    BashApproval::Danger
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.default.trim().is_empty() {
            return Err("model.default 为空".to_string());
        }
        if self.model.base_url.trim().is_empty() {
            return Err("model.base_url 为空".to_string());
        }
        if self.model.context_window == 0 {
            return Err("model.context_window 必须大于 0".to_string());
        }
        Ok(())
    }
}

pub fn config_dir(home: &Path) -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value).join("li"),
        _ => home.join(".config").join("li"),
    }
}

pub fn load(path: &Path) -> Result<Option<Config>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let config = toml::from_str(&text)
                .with_context(|| format!("无法解析配置文件 {}", path.display()))?;
            Ok(Some(config))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("无法读取配置文件 {}", path.display()))
        }
    }
}

pub fn write_default(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建配置目录 {}", parent.display()))?;
    }
    std::fs::write(path, DEFAULT_CONFIG)
        .with_context(|| format!("无法写入默认配置 {}", path.display()))
}

/// Collects every `AGENTS.md` between `start` and the repository root.
///
/// The walk stops at the first directory containing `.git` (inclusive), so
/// rules from outside the repository never leak in. Returns outermost first:
/// later sections are closer to the working directory and win attention.
pub fn project_rules(start: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut cursor = Some(start);

    while let Some(dir) = cursor {
        let candidate = dir.join(name);
        if candidate.is_file() {
            found.push(candidate);
        }
        if dir.join(".git").exists() {
            break;
        }
        cursor = dir.parent();
    }

    found.reverse();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    #[test]
    fn collects_every_layer_outermost_first() {
        let dir = tempfile::tempdir().expect("temp");
        let repo = dir.path().join("repo");
        let deep = repo.join("pkg/src");

        std::fs::create_dir_all(&deep).expect("deep");
        std::fs::create_dir_all(repo.join(".git")).expect("git");
        touch(&repo.join("AGENTS.md"), "root");
        touch(&repo.join("pkg/AGENTS.md"), "pkg");
        touch(&deep.join("AGENTS.md"), "deep");

        let found = project_rules(&deep, "AGENTS.md");
        assert_eq!(
            found,
            vec![
                repo.join("AGENTS.md"),
                repo.join("pkg/AGENTS.md"),
                deep.join("AGENTS.md"),
            ]
        );
    }

    #[test]
    fn stops_at_repository_root() {
        let dir = tempfile::tempdir().expect("temp");
        let outside = dir.path().join("outside");
        let repo = outside.join("repo/sub");

        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(outside.join(".git")).expect("outer git");
        std::fs::create_dir_all(outside.join("repo/.git")).expect("inner git");
        touch(&outside.join("AGENTS.md"), "outside");
        touch(&outside.join("repo/AGENTS.md"), "inner");

        let found = project_rules(&repo, "AGENTS.md");
        assert_eq!(found, vec![outside.join("repo/AGENTS.md")]);
    }

    #[test]
    fn rejects_unknown_bash_approval() {
        let parsed: Result<Config, _> = toml::from_str(
            "[model]\ndefault = \"m\"\nbase_url = \"http://x/v1\"\ncontext_window = 1\n\n[tools]\nbash_approval = \"yolo\"\n",
        );
        assert!(parsed.is_err());

        let accepted: Config = toml::from_str(
            "[model]\ndefault = \"m\"\nbase_url = \"http://x/v1\"\ncontext_window = 1\n\n[tools]\nbash_approval = \"danger\"\n",
        )
        .expect("parse");
        assert_eq!(accepted.tools.bash_approval, BashApproval::Danger);
    }

    #[test]
    fn skips_directories_without_the_file() {
        let dir = tempfile::tempdir().expect("temp");
        let repo = dir.path().join("repo");
        let deep = repo.join("a/b/c");

        std::fs::create_dir_all(&deep).expect("deep");
        std::fs::create_dir_all(repo.join(".git")).expect("git");
        touch(&repo.join("AGENTS.md"), "root");

        assert_eq!(project_rules(&deep, "AGENTS.md"), vec![repo.join("AGENTS.md")]);
    }
}
