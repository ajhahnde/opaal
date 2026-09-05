#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use flash_migrate::{MigrationFormat, NativeSourceReader, analyze_roots};

fn main() -> ExitCode {
    match parse_arguments(env::args_os().skip(1)) {
        Ok((format, roots)) => match analyze_roots(&NativeSourceReader, &roots) {
            Ok(report) => match report.render(format) {
                Ok(rendered) => {
                    println!("{rendered}");
                    ExitCode::from(report.exit_status())
                }
                Err(error) => render_error(&error, format),
            },
            Err(error) => render_error(&error, format),
        },
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("usage: fsh-migrate-v1-v2 [--format human|json] [--] ROOT...");
            ExitCode::from(2)
        }
    }
}

fn render_error(error: &flash_migrate::MigrationError, format: MigrationFormat) -> ExitCode {
    match format {
        MigrationFormat::Human => eprintln!("error: {}", error.render(format)),
        MigrationFormat::Json => println!("{}", error.render(format)),
    }
    ExitCode::from(1)
}

fn parse_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<(MigrationFormat, Vec<PathBuf>), String> {
    let mut format = MigrationFormat::Human;
    let mut roots = Vec::new();
    let mut options = true;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if options && argument == "--" {
            options = false;
        } else if options && argument == "--format" {
            let value = arguments
                .next()
                .ok_or_else(|| "--format requires `human` or `json`".to_owned())?;
            format = parse_format(&value)?;
        } else if options && argument.as_encoded_bytes().starts_with(b"--") {
            return Err(format!("unknown option `{}`", argument.to_string_lossy()));
        } else {
            roots.push(PathBuf::from(argument));
        }
    }
    if roots.is_empty() {
        return Err("at least one explicit root is required".to_owned());
    }
    Ok((format, roots))
}

fn parse_format(value: &OsString) -> Result<MigrationFormat, String> {
    if value == "human" {
        Ok(MigrationFormat::Human)
    } else if value == "json" {
        Ok(MigrationFormat::Json)
    } else {
        Err(format!(
            "unknown format `{}`; expected `human` or `json`",
            value.to_string_lossy()
        ))
    }
}
