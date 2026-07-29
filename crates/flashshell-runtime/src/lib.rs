#![forbid(unsafe_code)]

//! Platform-independent runtime contracts for FlashShell.

pub mod background;
pub mod builtin;
pub mod closure;
pub mod command;
pub mod convert;
pub mod directory;
mod environment;
pub mod eval;
pub mod execute;
pub mod file;
pub mod format;
pub mod internal;
pub mod job;
pub mod operation;
pub mod plan;
pub mod presentation;
pub mod resolve;
mod scope;
pub mod script;
pub mod session;
pub mod stream;
pub mod structured;
mod value;

pub use environment::Environment;
pub use scope::*;
pub use value::*;

/// Returns the FlashShell runtime version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
