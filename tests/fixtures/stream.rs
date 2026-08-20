#![forbid(unsafe_code)]

use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().collect();
    match arguments.get(1).map(String::as_str) {
        Some("source") => source(&arguments),
        Some("binary") => binary(&arguments),
        Some("binary-sink") => binary_sink(),
        Some("relay") => relay(&arguments),
        Some("sink") => sink(&arguments),
        Some("both") => both(&arguments),
        Some("both-closed") => both_closed(&arguments),
        _ => ExitCode::from(90),
    }
}

fn binary(arguments: &[String]) -> ExitCode {
    let code = parse(arguments, 2) as u8;
    if io::stdout().write_all(&[0, 0xff, b'\n']).is_err() {
        return ExitCode::from(91);
    }
    ExitCode::from(code)
}

fn binary_sink() -> ExitCode {
    let mut input = Vec::new();
    if io::stdin().read_to_end(&mut input).is_err() {
        return ExitCode::from(93);
    }
    if input == [0, 0xff, b'\n'] {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(93)
    }
}

fn source(arguments: &[String]) -> ExitCode {
    let length = parse(arguments, 2);
    let code = parse(arguments, 3) as u8;
    let chunk = [b'x'; 16 * 1024];
    let mut remaining = length;
    let mut stdout = io::stdout().lock();
    while remaining != 0 {
        let amount = remaining.min(chunk.len());
        if stdout.write_all(&chunk[..amount]).is_err() {
            return ExitCode::from(91);
        }
        remaining -= amount;
    }
    ExitCode::from(code)
}

fn relay(arguments: &[String]) -> ExitCode {
    let code = parse(arguments, 2) as u8;
    match io::copy(&mut io::stdin().lock(), &mut io::stdout().lock()) {
        Ok(_) => ExitCode::from(code),
        Err(_) => ExitCode::from(92),
    }
}

fn sink(arguments: &[String]) -> ExitCode {
    let expected = parse(arguments, 2);
    let code = parse(arguments, 3) as u8;
    let mut stdin = io::stdin().lock();
    let mut chunk = [0; 16 * 1024];
    let mut received = 0;
    loop {
        let amount = match stdin.read(&mut chunk) {
            Ok(0) => break,
            Ok(amount) => amount,
            Err(_) => return ExitCode::from(93),
        };
        if chunk[..amount].iter().any(|byte| *byte != b'x') {
            return ExitCode::from(93);
        }
        received += amount;
    }
    if received == expected {
        ExitCode::from(code)
    } else {
        ExitCode::from(93)
    }
}

fn both(arguments: &[String]) -> ExitCode {
    let code = parse(arguments, 2) as u8;
    if io::stdout().write_all(b"xx").is_err() || io::stderr().write_all(b"xx").is_err() {
        return ExitCode::from(94);
    }
    ExitCode::from(code)
}

fn both_closed(arguments: &[String]) -> ExitCode {
    let descriptor = parse(arguments, 2);
    if PathBuf::from(format!("/dev/fd/{descriptor}")).exists() {
        return ExitCode::from(96);
    }
    let code = parse(arguments, 3) as u8;
    if io::stdout().write_all(b"xx").is_err() || io::stderr().write_all(b"xx").is_err() {
        return ExitCode::from(94);
    }
    ExitCode::from(code)
}

fn parse(arguments: &[String], index: usize) -> usize {
    arguments
        .get(index)
        .and_then(|argument| argument.parse().ok())
        .unwrap_or(95)
}
