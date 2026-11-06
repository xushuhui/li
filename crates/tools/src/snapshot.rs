//! Content snapshots recorded by `read` and validated by `edit`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_VERSIONS: usize = 4;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub tag: String,
    /// Original file text, trailing whitespace preserved.
    pub content: String,
    /// 1-based inclusive ranges that were actually shown to the model.
    pub visible: Vec<(usize, usize)>,
}

#[derive(Default)]
pub struct SnapshotStore {
    entries: HashMap<PathBuf, Vec<Snapshot>>,
}

impl SnapshotStore {
    pub fn record(
        &mut self,
        path: &Path,
        tag: String,
        content: String,
        visible: Vec<(usize, usize)>,
    ) {
        let list = self.entries.entry(path.to_path_buf()).or_default();
        list.retain(|snapshot| snapshot.tag != tag);
        list.push(Snapshot {
            tag,
            content,
            visible,
        });
        if list.len() > MAX_VERSIONS {
            list.remove(0);
        }
    }

    pub fn find(&self, path: &Path, tag: &str) -> Option<&Snapshot> {
        self.entries
            .get(path)?
            .iter()
            .find(|snapshot| snapshot.tag == tag)
    }

    pub fn contains_visible(&self, snapshot: &Snapshot, range: (usize, usize)) -> bool {
        snapshot
            .visible
            .iter()
            .any(|visible| visible.0 <= range.0 && range.1 <= visible.1)
    }
}

/// Normalized text used only for hashing, never written back.
pub fn normalize(content: &str) -> String {
    content
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Four uppercase hex characters derived from the normalized content.
pub fn tag_of(content: &str) -> String {
    let normalized = normalize(content);
    let mut hash: u32 = 0x811c_9dc5;
    for byte in normalized.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{:04X}", (hash >> 16) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_ignores_line_endings_and_trailing_space() {
        assert_eq!(tag_of("a\r\nb\n"), tag_of("a\nb"));
        assert_eq!(tag_of("a  \nb\n"), tag_of("a\nb"));
    }

    #[test]
    fn tag_changes_with_content() {
        assert_ne!(tag_of("a\n"), tag_of("b\n"));
        assert_eq!(tag_of("x\n").len(), 4);
    }

    #[test]
    fn keeps_only_latest_versions() {
        let mut store = SnapshotStore::default();
        let path = Path::new("/tmp/x");
        for index in 0..6 {
            store.record(path, format!("000{index}"), String::new(), vec![]);
        }
        assert!(store.find(path, "0000").is_none());
        assert!(store.find(path, "0005").is_some());
    }
}
