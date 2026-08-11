//! Command-line invocation parsing and classification.
//!
//! Parsing is separated from process startup so the invocation matrix — help,
//! version, formatter frontend, a script path with ordered arguments, or an
//! interactive session — is decided by one pure, testable function before any
//! environment, filesystem, runtime, or editor access.

use std::ffi::OsString;
use std::path::PathBuf;

/// The explicit formatter action selected by the launcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatOperation {
    /// Report sources whose canonical form differs without writing them.
    Check,
    /// Atomically replace sources whose canonical form differs.
    Write,
}

/// The selected top-level program mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Print help and exit.
    Help,
    /// Print the version and exit.
    Version,
    /// Print formatter subcommand help and exit.
    FormatHelp,
    /// Format explicit source paths without executing them.
    Format {
        operation: FormatOperation,
        paths: Vec<PathBuf>,
    },
    /// Run one script file non-interactively.
    Script {
        path: PathBuf,
        arguments: Vec<String>,
    },
    /// Run one isolated chain supplied by the parent shell.
    AsyncChain {
        text: String,
        pipefail: bool,
        capture_limit: usize,
    },
    /// Start an interactive session.
    Interactive,
}

/// A fully classified invocation with its startup policies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub mode: Mode,
    pub no_config: bool,
    pub no_history: bool,
}

/// A rejected command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliError {
    /// An unrecognized leading option.
    UnknownOption(String),
    /// More than one positional argument.
    UnexpectedArgument(String),
    /// A script argument could not be represented without loss.
    InvalidScriptArgument,
    /// An option that requires a following value had none.
    MissingOptionValue(&'static str),
    /// An option's value could not be decoded or parsed.
    InvalidOptionValue { option: &'static str, value: String },
    /// A reserved mode omitted one of its required options.
    MissingRequiredOption(&'static str),
    /// A singleton option appeared more than once.
    DuplicateOption(&'static str),
    /// The formatter did not select a check or write operation.
    MissingFormatOperation,
    /// The formatter selected both check and write operations.
    ConflictingFormatOperations,
    /// The formatter did not receive a source path.
    MissingFormatPath,
    /// The formatter received the unsupported stdin sentinel.
    StdinFormatPath,
}

impl CliError {
    /// The user-facing message for this error.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::UnknownOption(option) => format!("unknown option '{option}'"),
            Self::UnexpectedArgument(_) => "expected one script path".to_owned(),
            Self::InvalidScriptArgument => "script arguments must be valid UTF-8".to_owned(),
            Self::MissingOptionValue(option) => format!("option '{option}' requires a value"),
            Self::InvalidOptionValue { option, value } => {
                format!("invalid value '{value}' for option '{option}'")
            }
            Self::MissingRequiredOption(option) => {
                format!("reserved invocation requires option '{option}'")
            }
            Self::DuplicateOption(option) => format!("option '{option}' may appear only once"),
            Self::MissingFormatOperation => {
                "format requires exactly one of '--check' or '--write'".to_owned()
            }
            Self::ConflictingFormatOperations => {
                "format options '--check' and '--write' cannot be combined".to_owned()
            }
            Self::MissingFormatPath => "format requires at least one path".to_owned(),
            Self::StdinFormatPath => {
                "'-' is not supported as a formatter path; name a file".to_owned()
            }
        }
    }
}

/// Classify one command line, excluding argv zero.
///
/// Leading options are order-independent; the first non-option token is the
/// script path and terminates option parsing. `--help`/`-h` and
/// `--version`/`-V` win over a script path. Every token after the script path is
/// retained as an ordered UTF-8 script argument. An unrecognized leading option
/// is rejected.
pub fn parse_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut help = false;
    let mut version = false;
    let mut no_config = false;
    let mut no_history = false;
    let mut script: Option<PathBuf> = None;
    let mut script_arguments = Vec::new();
    let mut async_chain: Option<String> = None;
    let mut async_pipefail = false;
    let mut async_capture_limit: Option<usize> = None;
    let mut options_ended = false;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        if script.is_some() {
            script_arguments.push(
                argument
                    .into_string()
                    .map_err(|_| CliError::InvalidScriptArgument)?,
            );
            continue;
        }

        if options_ended {
            if async_chain.is_some() || async_pipefail || async_capture_limit.is_some() {
                return Err(CliError::UnexpectedArgument(
                    argument.to_string_lossy().into_owned(),
                ));
            }
            script = Some(PathBuf::from(argument));
            continue;
        }

        match argument.to_str() {
            Some("--help" | "-h") => help = true,
            Some("--version" | "-V") => version = true,
            Some("--no-config") => no_config = true,
            Some("--no-history") => no_history = true,
            Some("--async-chain") => {
                let value = arguments
                    .next()
                    .ok_or(CliError::MissingOptionValue("--async-chain"))?;
                let text = value
                    .into_string()
                    .map_err(|value| CliError::InvalidOptionValue {
                        option: "--async-chain",
                        value: value.to_string_lossy().into_owned(),
                    })?;
                if async_chain.replace(text).is_some() {
                    return Err(CliError::DuplicateOption("--async-chain"));
                }
            }
            Some("--async-pipefail") => {
                if async_pipefail {
                    return Err(CliError::DuplicateOption("--async-pipefail"));
                }
                async_pipefail = true;
            }
            Some("--async-capture-limit") => {
                let value = arguments
                    .next()
                    .ok_or(CliError::MissingOptionValue("--async-capture-limit"))?;
                let rendered = value.to_string_lossy().into_owned();
                let limit =
                    rendered
                        .parse::<usize>()
                        .map_err(|_| CliError::InvalidOptionValue {
                            option: "--async-capture-limit",
                            value: rendered,
                        })?;
                if async_capture_limit.replace(limit).is_some() {
                    return Err(CliError::DuplicateOption("--async-capture-limit"));
                }
            }
            Some("--") => options_ended = true,
            Some("format") => {
                if help {
                    return Ok(Invocation {
                        mode: Mode::Help,
                        no_config,
                        no_history,
                    });
                }
                if version {
                    return Ok(Invocation {
                        mode: Mode::Version,
                        no_config,
                        no_history,
                    });
                }
                if no_config {
                    return Err(CliError::UnknownOption("--no-config".to_owned()));
                }
                if no_history {
                    return Err(CliError::UnknownOption("--no-history".to_owned()));
                }
                if async_chain.is_some() || async_pipefail || async_capture_limit.is_some() {
                    return Err(CliError::UnexpectedArgument("format".to_owned()));
                }
                return parse_format_args(arguments);
            }
            Some(text) if text.starts_with('-') && text != "-" => {
                return Err(CliError::UnknownOption(text.to_owned()));
            }
            _ => script = Some(PathBuf::from(argument)),
        }
    }

    let mode = if help {
        Mode::Help
    } else if version {
        Mode::Version
    } else if let Some(text) = async_chain {
        if let Some(path) = script {
            return Err(CliError::UnexpectedArgument(
                path.to_string_lossy().into_owned(),
            ));
        }
        Mode::AsyncChain {
            text,
            pipefail: async_pipefail,
            capture_limit: async_capture_limit
                .ok_or(CliError::MissingRequiredOption("--async-capture-limit"))?,
        }
    } else if async_pipefail || async_capture_limit.is_some() {
        return Err(CliError::MissingRequiredOption("--async-chain"));
    } else if let Some(path) = script {
        Mode::Script {
            path,
            arguments: script_arguments,
        }
    } else {
        Mode::Interactive
    };

    Ok(Invocation {
        mode,
        no_config,
        no_history,
    })
}

fn parse_format_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut operation = None;
    let mut help = false;
    let mut paths = Vec::new();
    let mut options_ended = false;

    for argument in arguments {
        if options_ended {
            if argument == "-" {
                return Err(CliError::StdinFormatPath);
            }
            paths.push(PathBuf::from(argument));
            continue;
        }

        match argument.to_str() {
            Some("--check") => select_format_operation(&mut operation, FormatOperation::Check)?,
            Some("--write") => select_format_operation(&mut operation, FormatOperation::Write)?,
            Some("--help") => {
                if help {
                    return Err(CliError::DuplicateOption("--help"));
                }
                help = true;
            }
            Some("--") => options_ended = true,
            Some("-") => return Err(CliError::StdinFormatPath),
            Some(text) if text.starts_with('-') => {
                return Err(CliError::UnknownOption(text.to_owned()));
            }
            _ => paths.push(PathBuf::from(argument)),
        }
    }

    if help {
        return Ok(Invocation {
            mode: Mode::FormatHelp,
            no_config: false,
            no_history: false,
        });
    }
    let operation = operation.ok_or(CliError::MissingFormatOperation)?;
    if paths.is_empty() {
        return Err(CliError::MissingFormatPath);
    }

    Ok(Invocation {
        mode: Mode::Format { operation, paths },
        no_config: false,
        no_history: false,
    })
}

fn select_format_operation(
    selected: &mut Option<FormatOperation>,
    candidate: FormatOperation,
) -> Result<(), CliError> {
    match *selected {
        None => *selected = Some(candidate),
        Some(existing) if existing == candidate => {
            let option = match candidate {
                FormatOperation::Check => "--check",
                FormatOperation::Write => "--write",
            };
            return Err(CliError::DuplicateOption(option));
        }
        Some(_) => return Err(CliError::ConflictingFormatOperations),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Invocation, CliError> {
        parse_args(arguments.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_selects_an_interactive_session() {
        let invocation = parse(&[]).expect("empty command line is valid");
        assert_eq!(invocation.mode, Mode::Interactive);
        assert!(!invocation.no_config);
        assert!(!invocation.no_history);
    }

    #[test]
    fn interactive_policies_are_order_independent() {
        let a = parse(&["--no-config", "--no-history"]).expect("valid");
        let b = parse(&["--no-history", "--no-config"]).expect("valid");
        assert_eq!(a, b);
        assert_eq!(a.mode, Mode::Interactive);
        assert!(a.no_config && a.no_history);
    }

    #[test]
    fn a_single_positional_is_a_script_path() {
        let invocation = parse(&["run.fsh"]).expect("valid");
        assert_eq!(
            invocation.mode,
            Mode::Script {
                path: PathBuf::from("run.fsh"),
                arguments: Vec::new(),
            }
        );
    }

    #[test]
    fn operands_after_the_script_path_are_ordered_script_arguments() {
        let invocation = parse(&["run.fsh", "", "--flag"]).expect("valid script invocation");
        assert_eq!(
            invocation.mode,
            Mode::Script {
                path: PathBuf::from("run.fsh"),
                arguments: vec![String::new(), "--flag".to_owned()],
            }
        );
    }

    #[test]
    fn help_and_version_win_over_a_script_path() {
        assert_eq!(parse(&["--help", "run.fsh"]).unwrap().mode, Mode::Help);
        assert_eq!(parse(&["-h"]).unwrap().mode, Mode::Help);
        assert_eq!(
            parse(&["--version", "run.fsh"]).unwrap().mode,
            Mode::Version
        );
        assert_eq!(parse(&["-V"]).unwrap().mode, Mode::Version);
    }

    #[test]
    fn a_double_dash_forces_the_next_token_to_be_a_script_path() {
        let invocation = parse(&["--", "--no-config"]).expect("valid");
        assert_eq!(
            invocation.mode,
            Mode::Script {
                path: PathBuf::from("--no-config"),
                arguments: Vec::new(),
            }
        );
        assert!(
            !invocation.no_config,
            "the flag after -- is the path, not a policy"
        );
    }

    #[test]
    fn an_unknown_leading_option_is_rejected() {
        assert_eq!(
            parse(&["--nope"]),
            Err(CliError::UnknownOption("--nope".to_owned()))
        );
    }

    #[test]
    fn a_second_positional_is_a_script_argument() {
        let invocation = parse(&["one.fsh", "two.fsh"]).expect("valid script invocation");
        assert_eq!(
            invocation.mode,
            Mode::Script {
                path: PathBuf::from("one.fsh"),
                arguments: vec!["two.fsh".to_owned()],
            }
        );
    }

    #[test]
    fn formatter_check_and_write_preserve_ordered_paths() {
        let check = parse(&["format", "--check", "one.fsh", "two.fsh"])
            .expect("formatter check invocation is valid");
        assert_eq!(
            check.mode,
            Mode::Format {
                operation: FormatOperation::Check,
                paths: vec![PathBuf::from("one.fsh"), PathBuf::from("two.fsh")],
            }
        );

        let write =
            parse(&["format", "--write", "one.fsh"]).expect("formatter write invocation is valid");
        assert_eq!(
            write.mode,
            Mode::Format {
                operation: FormatOperation::Write,
                paths: vec![PathBuf::from("one.fsh")],
            }
        );
    }

    #[test]
    fn formatter_options_may_be_interspersed_before_double_dash() {
        let invocation = parse(&["format", "one.fsh", "--check", "two.fsh"])
            .expect("interspersed formatter operation is valid");
        assert_eq!(
            invocation.mode,
            Mode::Format {
                operation: FormatOperation::Check,
                paths: vec![PathBuf::from("one.fsh"), PathBuf::from("two.fsh")],
            }
        );
    }

    #[test]
    fn formatter_double_dash_makes_every_remaining_operand_a_path() {
        let invocation = parse(&["format", "--write", "--", "--check", "--odd.fsh"])
            .expect("dash-leading formatter paths are valid after --");
        assert_eq!(
            invocation.mode,
            Mode::Format {
                operation: FormatOperation::Write,
                paths: vec![PathBuf::from("--check"), PathBuf::from("--odd.fsh")],
            }
        );
    }

    #[test]
    fn formatter_help_is_a_distinct_launcher_mode() {
        assert_eq!(parse(&["format", "--help"]).unwrap().mode, Mode::FormatHelp);
        assert_eq!(
            parse(&["format", "--help", "--check", "ignored.fsh"])
                .unwrap()
                .mode,
            Mode::FormatHelp,
            "subcommand help wins over formatter modes and paths"
        );
        assert_eq!(
            parse(&["--help", "format", "--check", "one.fsh"])
                .unwrap()
                .mode,
            Mode::Help,
            "top-level help keeps precedence before the formatter operand"
        );
        assert_eq!(
            parse(&["--version", "format", "--check", "one.fsh"])
                .unwrap()
                .mode,
            Mode::Version,
            "top-level version keeps precedence before the formatter operand"
        );
    }

    #[test]
    fn formatter_rejects_missing_conflicting_and_duplicate_modes() {
        for arguments in [
            &["format", "one.fsh"][..],
            &["format", "--check", "--write", "one.fsh"],
            &["format", "--write", "--check", "one.fsh"],
            &["format", "--check", "--check", "one.fsh"],
            &["format", "--write", "--write", "one.fsh"],
            &["format", "--help", "--help"],
        ] {
            assert!(
                parse(arguments).is_err(),
                "formatter mode misuse must fail: {arguments:?}"
            );
        }
    }

    #[test]
    fn formatter_rejects_missing_or_unsupported_paths() {
        for arguments in [
            &["format", "--check"][..],
            &["format", "--write"],
            &["format", "--check", "-"],
            &["format", "--write", "--", "-"],
        ] {
            assert!(
                parse(arguments).is_err(),
                "unsupported formatter path must fail: {arguments:?}"
            );
        }
    }

    #[test]
    fn formatter_rejects_unknown_and_session_options() {
        for arguments in [
            &["format", "--check", "--unknown", "one.fsh"][..],
            &["format", "--check", "--version", "one.fsh"],
            &["format", "--check", "--no-config", "one.fsh"],
            &["format", "--check", "--no-history", "one.fsh"],
            &["--no-config", "format", "--check", "one.fsh"],
            &["--no-history", "format", "--check", "one.fsh"],
        ] {
            assert!(
                parse(arguments).is_err(),
                "inapplicable formatter option must fail: {arguments:?}"
            );
        }
    }

    #[test]
    fn formatter_cannot_combine_with_the_reserved_chain_mode() {
        assert!(
            parse(&[
                "--async-chain",
                "^tool",
                "--async-capture-limit",
                "4096",
                "format",
                "--check",
                "one.fsh",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "format",
                "--check",
                "one.fsh",
                "--async-chain",
                "^tool",
                "--async-capture-limit",
                "4096",
            ])
            .is_err()
        );
    }

    #[test]
    fn formatter_spelling_remains_reachable_as_a_script_path() {
        assert_eq!(
            parse(&["./format", "--check"]).unwrap().mode,
            Mode::Script {
                path: PathBuf::from("./format"),
                arguments: vec!["--check".to_owned()],
            }
        );
        assert_eq!(
            parse(&["--", "format", "--check"]).unwrap().mode,
            Mode::Script {
                path: PathBuf::from("format"),
                arguments: vec!["--check".to_owned()],
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn formatter_paths_preserve_native_non_utf8_spelling() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(b"source-\xff.fsh".to_vec());
        let invocation = parse_args([
            OsString::from("format"),
            OsString::from("--check"),
            path.clone(),
        ])
        .expect("native formatter path is valid");
        assert_eq!(
            invocation.mode,
            Mode::Format {
                operation: FormatOperation::Check,
                paths: vec![PathBuf::from(path)],
            }
        );
    }

    #[test]
    fn an_option_after_the_script_path_is_a_script_argument() {
        let invocation = parse(&["run.fsh", "--no-config"]).expect("valid script invocation");
        assert_eq!(
            invocation.mode,
            Mode::Script {
                path: PathBuf::from("run.fsh"),
                arguments: vec!["--no-config".to_owned()],
            }
        );
    }

    #[test]
    fn the_reserved_chain_mode_carries_its_text_and_options() {
        let invocation = parse(&[
            "--async-chain",
            "^tool && ^other",
            "--async-pipefail",
            "--async-capture-limit",
            "4096",
        ])
        .expect("the reserved invocation is valid");
        assert_eq!(
            invocation.mode,
            Mode::AsyncChain {
                text: "^tool && ^other".to_owned(),
                pipefail: true,
                capture_limit: 4096,
            }
        );
    }

    #[test]
    fn reserved_chain_values_are_required_and_validated() {
        assert_eq!(
            parse(&["--async-chain"]),
            Err(CliError::MissingOptionValue("--async-chain"))
        );
        assert_eq!(
            parse(&["--async-chain", "^tool"]),
            Err(CliError::MissingRequiredOption("--async-capture-limit"))
        );
        assert_eq!(
            parse(&["--async-chain", "^tool", "--async-capture-limit", "many"]),
            Err(CliError::InvalidOptionValue {
                option: "--async-capture-limit",
                value: "many".to_owned(),
            })
        );
        assert_eq!(
            parse(&["--async-pipefail"]),
            Err(CliError::MissingRequiredOption("--async-chain"))
        );
        assert_eq!(
            parse(&[
                "--async-chain",
                "^tool",
                "--async-pipefail",
                "--async-pipefail",
                "--async-capture-limit",
                "4096",
            ]),
            Err(CliError::DuplicateOption("--async-pipefail"))
        );
    }
}
