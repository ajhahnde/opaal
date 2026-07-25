#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().collect();
    match arguments.get(1).map(String::as_str) {
        Some("exit") => exit(&arguments),
        Some("signal") if arguments.len() == 2 => std::process::abort(),
        _ => ExitCode::from(90),
    }
}

fn exit(arguments: &[String]) -> ExitCode {
    let Some(code) = arguments
        .get(2)
        .and_then(|argument| argument.parse::<u8>().ok())
    else {
        return ExitCode::from(91);
    };
    if arguments.len() != 3 {
        return ExitCode::from(91);
    }
    ExitCode::from(code)
}
