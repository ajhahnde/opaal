#![forbid(unsafe_code)]

//! Interactive client boundaries for FlashShell.

pub mod cli;
pub mod completion;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod config;
pub mod editor;
pub mod highlight;
pub mod hint;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod history;
pub mod interactive;
pub mod terminal_editor;

#[cfg(any(target_os = "redox", test))]
mod raw_editor;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod reedline_editor;

#[cfg(target_os = "redox")]
pub use raw_editor::RawLineEditor;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use reedline_editor::ReedlineEditor;
