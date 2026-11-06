//! Appending JSONL session file plus the in-memory tree built from it.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use crate::entry::{Entry, EntryId};
use crate::message::{Message, Role};

const FILENAME_STAMP: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]Z");

/// Characters kept from the first user message when deriving a title.
const TITLE_CHARS: usize = 40;

pub struct Session {
    entries: Vec<Entry>,
    leaf: Option<EntryId>,
    file: File,
    /// Holds the exclusive flock for the lifetime of the session.
    _lock: File,
    path: PathBuf,
}

impl Session {
    pub fn create(dir: &Path, cwd: &str, version: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("无法创建会话目录 {}", dir.display()))?;

        let now = OffsetDateTime::now_utc();
        let stamp = now.format(FILENAME_STAMP)?;
        let path = dir.join(format!("{stamp}_{}.jsonl", short_id()));

        let lock = acquire_lock(&path)?;
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .read(true)
            .open(&path)
            .with_context(|| format!("无法创建会话文件 {}", path.display()))?;

        let mut session = Self {
            entries: Vec::new(),
            leaf: None,
            file,
            _lock: lock,
            path,
        };
        session.append(Entry::Session {
            id: short_id(),
            cwd: cwd.to_string(),
            version: version.to_string(),
            timestamp: now.format(&Rfc3339)?,
        })?;
        Ok(session)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let lock = acquire_lock(path)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("无法打开会话文件 {}", path.display()))?;

        let reader = BufReader::new(file.try_clone()?);
        let mut entries = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(error) => eprintln!("warning: 跳过会话文件第 {} 行：{error}", index + 1),
            }
        }

        let leaf = entries.last().map(|entry| entry.id().to_string());
        let mut session = Self {
            entries,
            leaf,
            file,
            _lock: lock,
            path: path.to_path_buf(),
        };
        session.repair_dangling_tool_calls()?;
        Ok(session)
    }

    pub fn append(&mut self, entry: Entry) -> Result<()> {
        let line = serde_json::to_string(&entry)?;
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        self.leaf = Some(entry.id().to_string());
        self.entries.push(entry);
        Ok(())
    }

    pub fn append_message(&mut self, message: Message) -> Result<EntryId> {
        let id = short_id();
        let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
        self.append(Entry::Message {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            timestamp: now,
            message,
        })?;
        Ok(id)
    }

    pub fn append_model_change(&mut self, model: &str) -> Result<()> {
        let id = short_id();
        let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
        self.append(Entry::ModelChange {
            id,
            parent_id: self.leaf.clone(),
            timestamp: now,
            model: model.to_string(),
        })
    }

    pub fn append_compaction(&mut self, summary: &str) -> Result<()> {
        let id = short_id();
        let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
        self.append(Entry::Compaction {
            id,
            parent_id: self.leaf.clone(),
            timestamp: now,
            summary: summary.to_string(),
        })
    }

    /// Appends `toolResult` entries for calls that never received one.
    ///
    /// A killed process leaves an assistant turn with `toolCall` blocks and no
    /// matching results. Providers reject that shape on the next request, so
    /// the gap is closed at load time instead of at request time.
    pub fn repair_dangling_tool_calls(&mut self) -> Result<usize> {
        let missing: Vec<(String, String)> = {
            let mut called: Vec<(String, String)> = Vec::new();
            let mut answered: HashSet<String> = HashSet::new();

            for entry in self.visible_path() {
                let Some(message) = entry.message() else {
                    continue;
                };
                match message.role {
                    Role::Assistant => {
                        for (id, name, _) in message.tool_calls() {
                            called.push((id.to_string(), name.to_string()));
                        }
                    }
                    Role::ToolResult => {
                        if let Some(id) = message.tool_call_id.as_deref() {
                            answered.insert(id.to_string());
                        }
                    }
                    Role::User => {}
                }
            }

            called
                .into_iter()
                .filter(|(id, _)| !answered.contains(id))
                .collect()
        };

        let count = missing.len();
        for (id, name) in missing {
            self.append_message(Message::tool_result(&id, &name, "执行被中断", true))?;
        }
        Ok(count)
    }

    pub fn messages(&self) -> Vec<&Message> {
        self.visible_path()
            .into_iter()
            .filter_map(|entry| entry.message())
            .collect()
    }

    pub fn visible_path(&self) -> Vec<&Entry> {
        let Some(leaf) = self.leaf.as_deref() else {
            return Vec::new();
        };
        let index: HashMap<&str, &Entry> =
            self.entries.iter().map(|entry| (entry.id(), entry)).collect();

        let mut path = Vec::new();
        let mut cursor = Some(leaf);
        while let Some(id) = cursor {
            let Some(entry) = index.get(id) else {
                break;
            };
            path.push(*entry);
            if matches!(entry, Entry::Compaction { .. }) {
                break;
            }
            cursor = entry.parent_id();
        }
        path.reverse();
        path
    }

    /// The summary that starts the current context, if the session was compacted.
    pub fn summary(&self) -> Option<&str> {
        self.visible_path().first().and_then(|entry| match entry {
            Entry::Compaction { summary, .. } => Some(summary.as_str()),
            _ => None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What the `--resume` picker needs from a session file.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub timestamp: String,
    pub title: String,
}

/// Newest sessions first. Unreadable files are skipped, not fatal.
pub fn list_summaries(dir: &Path, limit: usize) -> Result<Vec<SessionSummary>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();

    // File names start with an ISO timestamp, so lexical order is time order.
    paths.sort();
    paths.reverse();
    paths.truncate(limit);

    Ok(paths
        .iter()
        .filter_map(|path| read_summary(path).ok())
        .collect())
}

pub fn latest_summary(dir: &Path) -> Result<Option<SessionSummary>> {
    Ok(list_summaries(dir, 1)?.into_iter().next())
}

fn read_summary(path: &Path) -> Result<SessionSummary> {
    let reader = BufReader::new(File::open(path)?);
    let mut header: Option<(String, String, String)> = None;
    let mut title = String::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };
        match entry {
            Entry::Session {
                id, cwd, timestamp, ..
            } => header = Some((id, cwd, timestamp)),
            Entry::Message { message, .. } => {
                if message.role == Role::User {
                    title = summarize_title(&message.text());
                    break;
                }
            }
            Entry::ModelChange { .. } | Entry::Compaction { .. } => {}
        }
    }

    let Some((id, cwd, timestamp)) = header else {
        anyhow::bail!("会话文件缺少会话头");
    };
    Ok(SessionSummary {
        path: path.to_path_buf(),
        id,
        cwd,
        timestamp,
        title,
    })
}

fn summarize_title(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = flattened.chars().take(TITLE_CHARS).collect();
    if flattened.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    title
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn acquire_lock(path: &Path) -> Result<File> {
    let lock_path = lock_path(path);
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("无法创建锁文件 {}", lock_path.display()))?;
    lock.try_lock_exclusive().with_context(|| {
        format!(
            "会话正被另一个 baize 进程使用：{}",
            path.display()
        )
    })?;
    Ok(lock)
}

pub fn encode_dir(cwd: &str) -> String {
    format!("-{}", cwd.trim_start_matches('/').replace('/', "-"))
}

pub fn short_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    nanos.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

pub fn sessions_root(home: &Path) -> PathBuf {
    home.join(".baize").join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Content;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn write_session(dir: &Path, stem: &str, cwd: &str, title: &str) -> PathBuf {
        let path = dir.join(format!("{stem}.jsonl"));
        let mut file = File::create(&path).expect("session file");
        writeln!(
            file,
            r#"{{"type":"session","id":"id-{stem}","cwd":"{cwd}","version":"0.1.0","timestamp":"2026-11-06T10:00:00Z"}}"#
        )
        .expect("header");
        writeln!(
            file,
            r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-06T10:00:01Z","message":{{"role":"user","content":[{{"type":"text","text":"{title}"}}]}}}}"#
        )
        .expect("message");
        path
    }

    #[test]
    fn encodes_directory_like_omp() {
        assert_eq!(encode_dir("/Users/xsh/gp/dotfiles"), "-Users-xsh-gp-dotfiles");
    }

    #[test]
    fn short_id_is_eight_hex_chars() {
        let id = short_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn load_skips_corrupt_lines() {
        let dir = temp_dir();
        let mut session = Session::create(dir.path(), "/work", "0.1.0").unwrap();
        session.append_message(Message::user("hi")).unwrap();
        let path = session.path().to_path_buf();
        drop(session);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{broken").unwrap();
        drop(file);

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.messages().len(), 1);
    }

    #[test]
    fn load_repairs_dangling_tool_calls_once() {
        let dir = temp_dir();
        let mut session = Session::create(dir.path(), "/work", "0.1.0").unwrap();
        session.append_message(Message::user("go")).unwrap();
        session
            .append_message(Message::assistant_turn(
                String::new(),
                vec![Content::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "a.txt" }),
                }],
                None,
            ))
            .unwrap();
        let path = session.path().to_path_buf();
        drop(session);

        let loaded = Session::load(&path).unwrap();
        let messages = loaded.messages();
        assert_eq!(messages.len(), 3);
        let repaired = messages.last().unwrap();
        assert_eq!(repaired.role, Role::ToolResult);
        assert_eq!(repaired.tool_call_id.as_deref(), Some("call_1"));
        assert!(repaired.is_error);
        assert!(repaired.text().contains("执行被中断"));
        drop(loaded);

        let again = Session::load(&path).unwrap();
        assert_eq!(again.messages().len(), 3);
    }

    #[test]
    fn list_summaries_orders_newest_first() {
        let dir = temp_dir();
        write_session(dir.path(), "2026-11-05T10-00-00Z_aaaa", "/old", "旧会话");
        write_session(dir.path(), "2026-11-06T10-00-00Z_bbbb", "/new", "新会话");

        let summaries = list_summaries(dir.path(), 10).unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].title, "新会话");
        assert_eq!(summaries[0].cwd, "/new");
        assert_eq!(summaries[1].title, "旧会话");
    }

    #[test]
    fn list_summaries_honours_limit() {
        let dir = temp_dir();
        write_session(dir.path(), "2026-11-05T10-00-00Z_aaaa", "/a", "一");
        write_session(dir.path(), "2026-11-06T10-00-00Z_bbbb", "/b", "二");

        let summaries = list_summaries(dir.path(), 1).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "二");
    }

    #[test]
    fn title_is_truncated_at_forty_characters() {
        let long = "一".repeat(41);
        let title = summarize_title(&long);
        assert_eq!(title.chars().count(), 41);
        assert!(title.ends_with('…'));

        let short = summarize_title("第一行\n第二行");
        assert_eq!(short, "第一行 第二行");
    }

    #[test]
    fn second_load_fails_while_first_holds_the_lock() {
        let dir = temp_dir();
        let session = Session::create(dir.path(), "/work", "0.1.0").unwrap();
        let path = session.path().to_path_buf();

        let error = match Session::load(&path) {
            Ok(_) => panic!("second load should fail"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("另一个 baize"), "error: {error}");
    }

    #[test]
    fn compaction_starts_a_new_context() {
        let dir = temp_dir();
        let mut session = Session::create(dir.path(), "/work", "0.1.0").unwrap();
        session.append_message(Message::user("很早以前的问题")).unwrap();
        session.append_message(Message::assistant("很早以前的回答", None)).unwrap();
        session.append_compaction("## 目标\n修复登录").unwrap();
        session.append_message(Message::user("压缩之后的问题")).unwrap();

        let messages = session.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "压缩之后的问题");

        let summary = session.summary().expect("summary");
        assert!(summary.contains("修复登录"));

        let path = session.path().to_path_buf();
        drop(session);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("很早以前的问题"), "原消息必须留在文件里");
        assert!(raw.contains("\"type\":\"compaction\""));
    }

    #[test]
    fn compaction_survives_reload() {
        let dir = temp_dir();
        let mut session = Session::create(dir.path(), "/work", "0.1.0").unwrap();
        session.append_message(Message::user("旧问题")).unwrap();
        session.append_compaction("## 目标\n旧目标").unwrap();
        session.append_message(Message::user("新问题")).unwrap();
        let path = session.path().to_path_buf();
        drop(session);

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.summary(), Some("## 目标\n旧目标"));
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.messages()[0].text(), "新问题");
    }

    #[test]
    fn listing_does_not_take_the_lock() {
        let dir = temp_dir();
        let mut session = Session::create(dir.path(), "/work", "0.1.0").unwrap();
        session.append_message(Message::user("标题")).unwrap();
        let path = session.path().to_path_buf();

        let summaries = list_summaries(dir.path(), 10).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "标题");

        drop(session);
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.messages().len(), 1);
    }
}
