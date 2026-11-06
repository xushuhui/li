//! Where a turn's output goes: the plain terminal, or the full-screen TUI.

use std::io::Write;

/// Output sink passed through `Settings`. Implementations must be safe to call
/// from the turn task while another thread renders.
pub trait Ui: Send + Sync {
    /// A streamed chunk of the assistant's answer.
    fn assistant(&self, text: &str);
    /// The assistant finished this message; start a new line.
    fn assistant_end(&self);
    /// A tool call or a compact summary of its result.
    fn tool(&self, line: &str);
    /// Progress, warnings, and interruptions.
    fn status(&self, line: &str);
}

/// Writes straight to the terminal; what the non-TUI modes have always done.
pub struct PlainUi;

impl Ui for PlainUi {
    fn assistant(&self, text: &str) {
        print!("{text}");
        // A failed flush only costs promptness; the bytes are already queued.
        let _ = std::io::stdout().flush();
    }

    fn assistant_end(&self) {
        println!();
    }

    fn tool(&self, line: &str) {
        eprintln!("{line}");
    }

    fn status(&self, line: &str) {
        eprintln!("{line}");
    }
}
