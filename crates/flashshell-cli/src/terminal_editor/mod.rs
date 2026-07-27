//! A FlashShell-owned raw-mode line editor.
//!
//! The editor is portable: it compiles on every target and is exercised by the
//! host test suite. Only its selection in `main` is target-specific.

pub mod buffer;
pub mod key;
