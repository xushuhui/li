//! One line in the session file. Entries form a tree through `parentId`.

use serde::{Deserialize, Serialize};

use crate::message::Message;

pub type EntryId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Entry {
    #[serde(rename = "session")]
    Session {
        id: EntryId,
        cwd: String,
        version: String,
        timestamp: String,
    },
    #[serde(rename = "message")]
    Message {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        message: Message,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        model: String,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        summary: String,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Session { id, .. }
            | Entry::Message { id, .. }
            | Entry::ModelChange { id, .. }
            | Entry::Compaction { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Session { .. } => None,
            Entry::Message { parent_id, .. }
            | Entry::ModelChange { parent_id, .. }
            | Entry::Compaction { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn message(&self) -> Option<&Message> {
        match self {
            Entry::Message { message, .. } => Some(message),
            _ => None,
        }
    }
}
