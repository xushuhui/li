//! `grep` and `glob`: repository search built on ripgrep's traversal layer.
//!
//! Both tools share `ignore::WalkBuilder`, which applies `.gitignore`,
//! `.ignore`, `.git/info/exclude` and the global gitignore before any file is
//! visited. That is the whole reason we do not hand-roll a recursive walk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use globset::{Glob, GlobMatcher};
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutcome};

const DEFAULT_GREP_LIMIT: usize = 100;
const MAX_GREP_LIMIT: usize = 1000;
const DEFAULT_GLOB_LIMIT: usize = 200;
const MAX_GLOB_LIMIT: usize = 2000;

// --- grep -------------------------------------------------------------------

pub struct GrepTool;

struct Hit {
    path: PathBuf,
    line: u64,
    text: String,
}

struct Collector {
    path: PathBuf,
    hits: Arc<Mutex<Vec<Hit>>>,
    seen: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
    limit: usize,
}

impl Sink for Collector {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, matcher: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let previous = self.seen.fetch_add(1, Ordering::Relaxed);
        if previous >= self.limit {
            self.truncated.store(true, Ordering::Relaxed);
            return Ok(false);
        }

        let text = String::from_utf8_lossy(matcher.bytes());
        let text = text.trim_end_matches(['\n', '\r']).to_string();
        self.hits.lock().expect("hits lock").push(Hit {
            path: self.path.clone(),
            line: matcher.line_number().unwrap_or(0),
            text,
        });
        Ok(true)
    }
}

struct GrepRequest {
    root: PathBuf,
    pattern: String,
    include: Option<String>,
    ignore_case: bool,
    limit: usize,
}

fn run_grep(request: GrepRequest) -> Result<(Vec<Hit>, bool), String> {
    let matcher = RegexMatcherBuilder::new()
        .line_terminator(Some(b'\n'))
        .case_insensitive(request.ignore_case)
        .build(&request.pattern)
        .map_err(|error| format!("正则语法错误：{error}"))?;

    let include = match &request.include {
        Some(pattern) => Some(
            Glob::new(pattern)
                .map_err(|error| format!("include 不是有效的 glob：{error}"))?
                .compile_matcher(),
        ),
        None => None,
    };

    let searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    let hits: Arc<Mutex<Vec<Hit>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let root = Arc::new(request.root);
    let limit = request.limit;

    let walker = WalkBuilder::new(root.as_path())
        .require_git(false)
        .build_parallel();

    walker.run(|| {
        let matcher = matcher.clone();
        let mut searcher = searcher.clone();
        let include: Option<GlobMatcher> = include.clone();
        let hits = Arc::clone(&hits);
        let seen = Arc::clone(&seen);
        let truncated = Arc::clone(&truncated);
        let root = Arc::clone(&root);

        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return WalkState::Continue;
            }

            let path = entry.path();
            let relative = path.strip_prefix(root.as_path()).unwrap_or(path).to_path_buf();
            if let Some(glob) = &include
                && !glob.is_match(&relative)
            {
                return WalkState::Continue;
            }

            if seen.load(Ordering::Relaxed) >= limit {
                truncated.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }

            let sink = Collector {
                path: relative,
                hits: Arc::clone(&hits),
                seen: Arc::clone(&seen),
                truncated: Arc::clone(&truncated),
                limit,
            };
            // A single unreadable file must not abort the whole search.
            let _ = searcher.search_path(&matcher, path, sink);

            if seen.load(Ordering::Relaxed) >= limit {
                truncated.store(true, Ordering::Relaxed);
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    let mut hits = std::mem::take(&mut *hits.lock().expect("hits lock"));
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok((hits, truncated.load(Ordering::Relaxed)))
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "在仓库里按正则搜索文件内容，遵守 .gitignore。返回 路径:行号:内容，最多 100 条。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust 正则语法" },
                "path": { "type": "string", "description": "搜索根目录或单个文件，默认工作目录" },
                "include": { "type": "string", "description": "文件名 glob 过滤，如 *.rs；不含 / 时匹配任意深度" },
                "ignore_case": { "type": "boolean", "description": "默认大小写敏感" },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_GREP_LIMIT, "description": "最多返回多少条，默认 100" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(pattern) = args["pattern"].as_str() else {
            return ToolOutcome::error("缺少 pattern 参数");
        };
        if pattern.is_empty() {
            return ToolOutcome::error("pattern 不能为空");
        }

        let path = args["path"].as_str().unwrap_or(".");
        let root = ctx.resolve(path);
        if !root.exists() {
            return ToolOutcome::error(format!("无法访问 {path}"));
        }

        let limit = args["limit"]
            .as_u64()
            .unwrap_or(DEFAULT_GREP_LIMIT as u64)
            .clamp(1, MAX_GREP_LIMIT as u64) as usize;

        let request = GrepRequest {
            root,
            pattern: pattern.to_string(),
            include: args["include"].as_str().map(str::to_string),
            ignore_case: args["ignore_case"].as_bool().unwrap_or(false),
            limit,
        };

        let result = tokio::task::spawn_blocking(move || run_grep(request)).await;
        let (hits, truncated) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return ToolOutcome::error(error),
            Err(error) => return ToolOutcome::error(format!("搜索任务失败：{error}")),
        };

        if hits.is_empty() {
            return ToolOutcome::ok("没有匹配");
        }

        let mut output = String::new();
        for hit in &hits {
            output.push_str(&format!("{}:{}:{}\n", hit.path.display(), hit.line, hit.text));
        }
        if truncated {
            output.push_str(&format!(
                "[已显示 {limit} 条，可能还有更多，请收窄 pattern 或 path]\n"
            ));
        }
        ToolOutcome::ok(output)
    }
}

// --- glob -------------------------------------------------------------------

pub struct GlobTool;

struct GlobRequest {
    root: PathBuf,
    pattern: String,
    limit: usize,
}

fn run_glob(request: GlobRequest) -> Result<(Vec<(String, SystemTime)>, usize), String> {
    let matcher = Glob::new(&request.pattern)
        .map_err(|error| format!("glob 语法错误：{error}"))?
        .compile_matcher();

    let entries: Arc<Mutex<Vec<(String, SystemTime)>>> = Arc::new(Mutex::new(Vec::new()));
    let root = Arc::new(request.root);

    let walker = WalkBuilder::new(root.as_path())
        .require_git(false)
        .build_parallel();

    walker.run(|| {
        let matcher = matcher.clone();
        let entries = Arc::clone(&entries);
        let root = Arc::clone(&root);

        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return WalkState::Continue;
            }

            let path = entry.path();
            let relative = path.strip_prefix(root.as_path()).unwrap_or(path);
            if !matcher.is_match(relative) {
                return WalkState::Continue;
            }

            // SIMPLIFICATION: 每个命中都 stat 一次拿修改时间；匹配数上万时会多花约一秒。
            // 升级路径是遍历中维护一个小顶堆，但这只省内存不省 stat，收益有限。
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);

            entries
                .lock()
                .expect("entries lock")
                .push((relative.display().to_string(), modified));
            WalkState::Continue
        })
    });

    let mut entries = std::mem::take(&mut *entries.lock().expect("entries lock"));
    let total = entries.len();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    entries.truncate(request.limit);
    Ok((entries, total))
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "按 glob 模式列出文件，遵守 .gitignore，按修改时间倒序，最多 200 条。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "globset 语法，支持 **，如 *.rs 或 src/**/*.ts" },
                "path": { "type": "string", "description": "搜索根目录，默认工作目录" },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_GLOB_LIMIT, "description": "最多返回多少条，默认 200" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(pattern) = args["pattern"].as_str() else {
            return ToolOutcome::error("缺少 pattern 参数");
        };
        if pattern.is_empty() {
            return ToolOutcome::error("pattern 不能为空");
        }

        let path = args["path"].as_str().unwrap_or(".");
        let root = ctx.resolve(path);
        if !root.exists() {
            return ToolOutcome::error(format!("无法访问 {path}"));
        }

        let limit = args["limit"]
            .as_u64()
            .unwrap_or(DEFAULT_GLOB_LIMIT as u64)
            .clamp(1, MAX_GLOB_LIMIT as u64) as usize;

        let request = GlobRequest {
            root,
            pattern: pattern.to_string(),
            limit,
        };

        let result = tokio::task::spawn_blocking(move || run_glob(request)).await;
        let (entries, total) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return ToolOutcome::error(error),
            Err(error) => return ToolOutcome::error(format!("遍历任务失败：{error}")),
        };

        if entries.is_empty() {
            return ToolOutcome::ok("没有匹配");
        }

        let mut output = String::new();
        for (path, _) in &entries {
            output.push_str(path);
            output.push('\n');
        }
        if total > entries.len() {
            output.push_str(&format!("[已显示 {} 条，共 {total} 条匹配]\n", entries.len()));
        }
        ToolOutcome::ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Rig;
    use std::time::Duration;

    async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> ToolOutcome {
        tool.execute(args, ctx).await
    }

    #[tokio::test]
    async fn grep_reports_path_line_and_text() {
        let rig = Rig::new();
        rig.write("a.rs", "fn main() {\n    run();\n}\n");
        rig.write("b.rs", "fn helper() {}\n");
        let ctx = rig.context();

        let outcome = call(&GrepTool, &ctx, json!({ "pattern": "fn " })).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("a.rs:1:fn main() {"));
        assert!(outcome.text.contains("b.rs:1:fn helper() {}"));
    }

    #[tokio::test]
    async fn grep_skips_gitignored_paths() {
        let rig = Rig::new();
        rig.write(".gitignore", "generated/\n");
        rig.write("a.rs", "let x = 1;\n");
        rig.write("generated/b.rs", "NEEDLE\n");
        let ctx = rig.context();

        let outcome = call(&GrepTool, &ctx, json!({ "pattern": "NEEDLE" })).await;

        assert!(!outcome.is_error);
        assert_eq!(outcome.text.trim(), "没有匹配");
    }

    #[tokio::test]
    async fn grep_caps_results_and_says_so() {
        let rig = Rig::new();
        let body = (0..150).map(|_| "NEEDLE").collect::<Vec<_>>().join("\n");
        rig.write("big.txt", &body);
        let ctx = rig.context();

        let outcome = call(&GrepTool, &ctx, json!({ "pattern": "NEEDLE" })).await;

        assert!(!outcome.is_error);
        let hits = outcome.text.lines().filter(|line| line.contains("big.txt:")).count();
        assert_eq!(hits, 100);
        assert!(outcome.text.contains("可能还有更多"));
    }

    #[tokio::test]
    async fn grep_include_narrows_files() {
        let rig = Rig::new();
        rig.write("a.rs", "MATCH\n");
        rig.write("a.txt", "MATCH\n");
        let ctx = rig.context();

        let outcome = call(
            &GrepTool,
            &ctx,
            json!({ "pattern": "MATCH", "include": "*.rs" }),
        )
        .await;

        assert!(outcome.text.contains("a.rs:1:MATCH"));
        assert!(!outcome.text.contains("a.txt"));
    }

    #[tokio::test]
    async fn grep_skips_binary_files() {
        let rig = Rig::new();
        rig.write("bin.dat", "NEEDLE\0MORE");
        let ctx = rig.context();

        let outcome = call(&GrepTool, &ctx, json!({ "pattern": "NEEDLE" })).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(outcome.text.trim(), "没有匹配");
    }

    #[tokio::test]
    async fn grep_rejects_bad_regex() {
        let rig = Rig::new();
        let ctx = rig.context();

        let outcome = call(&GrepTool, &ctx, json!({ "pattern": "(" })).await;

        assert!(outcome.is_error);
        assert!(outcome.text.contains("正则语法错误"));
    }

    #[tokio::test]
    async fn grep_respects_case_flag() {
        let rig = Rig::new();
        rig.write("a.txt", "Needle\n");
        let ctx = rig.context();

        let sensitive = call(&GrepTool, &ctx, json!({ "pattern": "needle" })).await;
        assert_eq!(sensitive.text.trim(), "没有匹配");

        let insensitive = call(
            &GrepTool,
            &ctx,
            json!({ "pattern": "needle", "ignore_case": true }),
        )
        .await;
        assert!(insensitive.text.contains("Needle"));
    }

    #[tokio::test]
    async fn glob_matches_across_directories() {
        let rig = Rig::new();
        rig.write("a.txt", "x");
        rig.write("sub/b.txt", "x");
        rig.write("sub/c.rs", "x");
        let ctx = rig.context();

        let outcome = call(&GlobTool, &ctx, json!({ "pattern": "*.txt" })).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("a.txt"));
        assert!(outcome.text.contains("sub/b.txt"));
        assert!(!outcome.text.contains("sub/c.rs"));
    }

    #[tokio::test]
    async fn glob_supports_recursive_patterns() {
        let rig = Rig::new();
        rig.write("deep/nested/a.rs", "x");
        rig.write("deep/a.txt", "x");
        let ctx = rig.context();

        let outcome = call(&GlobTool, &ctx, json!({ "pattern": "**/*.rs" })).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("deep/nested/a.rs"));
        assert!(!outcome.text.contains("a.txt"));
    }

    #[tokio::test]
    async fn glob_returns_files_only() {
        let rig = Rig::new();
        rig.write("dir/inner.txt", "x");
        let ctx = rig.context();

        let outcome = call(&GlobTool, &ctx, json!({ "pattern": "*" })).await;

        assert!(!outcome.is_error);
        assert!(!outcome.text.lines().any(|line| line == "dir"));
        assert!(outcome.text.contains("dir/inner.txt"));
    }

    #[tokio::test]
    async fn glob_caps_results_and_counts_total() {
        let rig = Rig::new();
        for index in 0..250 {
            rig.write(&format!("f{index}.txt"), "x");
        }
        let ctx = rig.context();

        let outcome = call(&GlobTool, &ctx, json!({ "pattern": "*.txt" })).await;

        let listed = outcome.text.lines().filter(|line| line.ends_with(".txt")).count();
        assert_eq!(listed, 200);
        assert!(outcome.text.contains("共 250 条匹配"));
    }

    #[tokio::test]
    async fn glob_sorts_by_newest_first() {
        let rig = Rig::new();
        rig.write("old.txt", "x");
        std::thread::sleep(Duration::from_millis(20));
        rig.write("new.txt", "x");
        let ctx = rig.context();

        let outcome = call(&GlobTool, &ctx, json!({ "pattern": "*.txt" })).await;

        let lines: Vec<&str> = outcome.text.lines().collect();
        assert_eq!(lines[0], "new.txt");
        assert_eq!(lines[1], "old.txt");
    }

    #[tokio::test]
    async fn glob_rejects_bad_pattern() {
        let rig = Rig::new();
        let ctx = rig.context();

        let outcome = call(&GlobTool, &ctx, json!({ "pattern": "[" })).await;

        assert!(outcome.is_error);
        assert!(outcome.text.contains("glob 语法错误"));
    }
}
