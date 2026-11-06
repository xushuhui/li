//! Session storage and the message model shared by the CLI and the provider.

pub mod compact;
pub mod entry;
pub mod message;
pub mod prompt;
pub mod session;

pub use entry::{Entry, EntryId};
pub use message::{Content, Message, Role, Usage};
pub use session::{
    Session, SessionSummary, encode_dir, latest_summary, list_summaries, sessions_root, short_id,
};
