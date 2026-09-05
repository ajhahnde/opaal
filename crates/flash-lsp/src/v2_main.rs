#![forbid(unsafe_code)]

use std::io;
use std::process::ExitCode;

use flash_lsp::protocol::ExitStatus;
use flash_lsp::server::run_for_language;
use flash_syntax::LanguageMajor;

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    match run_for_language(io::BufReader::new(stdin), &mut output, LanguageMajor::V2) {
        Ok(ExitStatus::Success) => ExitCode::SUCCESS,
        Ok(ExitStatus::Failure) | Err(_) => ExitCode::FAILURE,
    }
}
