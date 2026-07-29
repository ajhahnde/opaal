#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().collect();
    match arguments.get(1).map(String::as_str) {
        Some("exit") => exit(&arguments),
        Some("late") => late(&arguments),
        Some("signal") if arguments.len() == 2 => std::process::abort(),
        _ => ExitCode::from(90),
    }
}

/// Sleep, then leave a marker, then exit: `late <millis> <marker> <code>`.
///
/// The marker is written last, so its existence proves the caller waited for
/// this process rather than merely outliving its spawn.
fn late(arguments: &[String]) -> ExitCode {
    if arguments.len() != 5 {
        return ExitCode::from(92);
    }
    let (Ok(millis), Ok(code)) = (arguments[2].parse::<u64>(), arguments[4].parse::<u8>()) else {
        return ExitCode::from(92);
    };
    std::thread::sleep(std::time::Duration::from_millis(millis));
    if fs::write(&arguments[3], b"late").is_err() {
        return ExitCode::from(93);
    }
    ExitCode::from(code)
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
