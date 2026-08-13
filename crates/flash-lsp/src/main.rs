#![forbid(unsafe_code)]

use std::io;
use std::process::ExitCode;

use flash_lsp::protocol::ExitStatus;
use flash_lsp::server::run;

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    match run(io::BufReader::new(stdin), &mut output) {
        Ok(ExitStatus::Success) => ExitCode::SUCCESS,
        Ok(ExitStatus::Failure) | Err(_) => ExitCode::FAILURE,
    }
}
