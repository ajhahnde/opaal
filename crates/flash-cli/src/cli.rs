//! Command-line invocation parsing and classification.
//!
//! Parsing is separated from process startup so the invocation matrix — help,
//! version, a script path with ordered arguments, or an interactive session,
//! each combined with
//! the `--no-config` and `--no-history` policies — is decided by one pure,
//! testable function before any environment, filesystem, or editor access.

use std::ffi::OsString;
use std::path::PathBuf;

/// The selected top-level program mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Print help and exit.
    Help,
    /// Print the version and exit.
    Version,
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
