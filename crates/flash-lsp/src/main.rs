#![forbid(unsafe_code)]

use std::io;
use std::process::ExitCode;

use flash_lsp::protocol::{ExitStatus, run};

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    match run(&mut input, &mut output) {
        Ok(ExitStatus::Success) => ExitCode::SUCCESS,
        Ok(ExitStatus::Failure) | Err(_) => ExitCode::FAILURE,
    }
}
