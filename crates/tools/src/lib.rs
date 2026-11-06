//! Tool interface, registry, and shared execution context.

pub mod bash;
pub mod edit;
pub mod lsp;
pub mod read;
pub mod search;
mod snapshot;
pub mod write;

pub use snapshot::{tag_of, SnapshotStore};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Asks the user whether a command may run.
pub trait Approver: Send + Sync {
    fn approve(&self, command: &str) -> bool;
}

/// Approver used by tests and by `--yes` style callers.
pub struct AllowAll;

impl Approver for AllowAll {
    fn approve(&self, _command: &str) -> bool {
        true
    }
}

pub struct ToolContext {
    pub cwd: PathBuf,
    pub snapshots: Arc<Mutex<SnapshotStore>>,
    pub approver: Arc<dyn Approver>,
    /// One language server per (binary, project root), reused across calls.
    pub lsp: Arc<tokio::sync::Mutex<HashMap<String, Arc<lsp::client::Client>>>>,
}

impl ToolContext {
    pub fn resolve(&self, path: &str) -> PathBuf {
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            candidate
        } else {
            self.cwd.join(candidate)
        }
    }

    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.cwd) {
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolOutcome;
}

pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

pub fn registry() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(read::ReadTool),
        Box::new(write::WriteTool),
        Box::new(edit::EditTool),
        Box::new(bash::BashTool),
        Box::new(search::GrepTool),
        Box::new(search::GlobTool),
        Box::new(lsp::DiagnosticsTool),
        Box::new(lsp::DefinitionTool),
        Box::new(lsp::ReferencesTool),
    ]
}

pub fn definition(tool: &dyn Tool) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.name(),
            "description": tool.description(),
            "parameters": tool.schema(),
        }
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Temp directory plus a context whose snapshot store starts empty.
    pub struct Rig {
        dir: tempfile::TempDir,
    }

    impl Rig {
        pub fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("temp dir"),
            }
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        pub fn context(&self) -> ToolContext {
            self.context_with(Arc::new(AllowAll))
        }

        pub fn context_with(&self, approver: Arc<dyn Approver>) -> ToolContext {
            ToolContext {
                cwd: self.dir.path().to_path_buf(),
                snapshots: Arc::new(Mutex::new(SnapshotStore::default())),
                approver,
                lsp: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            }
        }

        pub fn write(&self, name: &str, content: &str) {
            let path = self.dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create fixture dir");
            }
            std::fs::write(&path, content).expect("write fixture");
        }

        pub fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(name)).expect("read fixture")
        }
    }

    /// Extracts `1A2B` from a `[path#1A2B]` header line.
    pub fn tag_from(text: &str) -> String {
        text.lines()
            .next()
            .and_then(|line| line.rsplit('#').next())
            .map(|tag| tag.trim_end_matches(']').to_string())
            .expect("header line")
    }
}
