//! Command-line invocation parsing and classification.
//!
//! Parsing is separated from process startup so the invocation matrix — help,
//! version, a single script path, or an interactive session, each combined with
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
    Script { path: PathBuf },
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
}

impl CliError {
    /// The user-facing message for this error.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::UnknownOption(option) => format!("unknown option '{option}'"),
            Self::UnexpectedArgument(_) => "expected one script path".to_owned(),
        }
    }
}

/// Classify one command line, excluding argv zero.
///
/// Leading options are order-independent; the first non-option token is the
/// script path and terminates option parsing. `--help`/`-h` and
/// `--version`/`-V` win over a script path. Any token after the script path,
/// and any unrecognized leading option, is rejected.
pub fn parse_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut help = false;
    let mut version = false;
    let mut no_config = false;
    let mut no_history = false;
    let mut script: Option<PathBuf> = None;
    let mut options_ended = false;

    for argument in arguments {
        if script.is_some() {
            return Err(CliError::UnexpectedArgument(
                argument.to_string_lossy().into_owned(),
            ));
        }

        if options_ended {
            script = Some(PathBuf::from(argument));
            continue;
        }

        match argument.to_str() {
            Some("--help" | "-h") => help = true,
            Some("--version" | "-V") => version = true,
            Some("--no-config") => no_config = true,
            Some("--no-history") => no_history = true,
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
    } else if let Some(path) = script {
        Mode::Script { path }
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
                path: PathBuf::from("run.fsh")
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
                path: PathBuf::from("--no-config")
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
    fn a_second_positional_is_rejected() {
        assert_eq!(
            parse(&["one.fsh", "two.fsh"]),
            Err(CliError::UnexpectedArgument("two.fsh".to_owned()))
        );
    }

    #[test]
    fn an_option_after_the_script_path_is_rejected() {
        assert_eq!(
            parse(&["run.fsh", "--no-config"]),
            Err(CliError::UnexpectedArgument("--no-config".to_owned()))
        );
    }
}
