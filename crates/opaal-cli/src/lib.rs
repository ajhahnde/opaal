#![forbid(unsafe_code)]

//! Interactive client boundaries for OPAAL.

pub mod check;
pub mod cli;
pub mod completion;
pub mod editor;
pub mod format;
pub mod highlight;
pub mod hint;
pub mod history;
pub mod interactive;
pub mod plan;
pub mod project;
pub mod report;
pub mod terminal_editor;

mod raw_editor;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod reedline_editor;

pub use terminal_editor::TerminalEditor;

pub use raw_editor::RawLineEditor;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use reedline_editor::ReedlineEditor;
