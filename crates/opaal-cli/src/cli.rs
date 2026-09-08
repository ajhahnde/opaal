//! Command-line invocation parsing and classification.
//!
//! OPAAL exposes one language personality. Parsing is kept separate from
//! startup so no option can silently select another source mode.

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatOperation {
    Check,
    Write,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Help,
    Version,
    CheckHelp,
    Check {
        source: PathBuf,
    },
    ProjectCheck {
        project: PathBuf,
        task: String,
        environment: String,
        authority: PathBuf,
        tools: PathBuf,
        inputs: Vec<(String, String)>,
    },
    TaskHelp,
    TaskInspect {
        project: PathBuf,
        task: String,
    },
    PlanHelp,
    Plan {
        source: PathBuf,
    },
    FormatHelp,
    Format {
        operation: FormatOperation,
        paths: Vec<PathBuf>,
    },
    Script {
        path: PathBuf,
        arguments: Vec<String>,
    },
    Interactive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub mode: Mode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliError {
    UnknownOption(String),
    InvalidScriptArgument,
    DuplicateOption(&'static str),
    MissingFormatOperation,
    ConflictingFormatOperations,
    MissingFormatPath,
    StdinFormatPath,
    MissingCheckSource,
    UnexpectedCheckSource(String),
    StdinCheckSource,
    MissingPlanSource,
    UnexpectedPlanSource(String),
    StdinPlanSource,
    InvalidProjectArgument(String),
}

impl CliError {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::UnknownOption(option) => format!("unknown option '{option}'"),
            Self::InvalidScriptArgument => "script arguments must be valid UTF-8".to_owned(),
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
            Self::MissingCheckSource => "check requires exactly one source path".to_owned(),
            Self::UnexpectedCheckSource(_) => "check accepts exactly one source path".to_owned(),
            Self::StdinCheckSource => {
                "'-' is not supported as a checker source; name a file".to_owned()
            }
            Self::MissingPlanSource => "plan requires exactly one source path".to_owned(),
            Self::UnexpectedPlanSource(_) => "plan accepts exactly one source path".to_owned(),
            Self::StdinPlanSource => {
                "'-' is not supported as a planner source; name a file".to_owned()
            }
            Self::InvalidProjectArgument(message) => message.clone(),
        }
    }
}

pub fn parse_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let Some(first) = arguments.first() else {
        return Ok(Invocation {
            mode: Mode::Interactive,
        });
    };

    match first.to_str() {
        Some("--help" | "-h") => return Ok(Invocation { mode: Mode::Help }),
        Some("--version" | "-V") => {
            return Ok(Invocation {
                mode: Mode::Version,
            });
        }
        Some("check") => return parse_check_args(arguments.into_iter().skip(1)),
        Some("task") => return parse_task_args(arguments.into_iter().skip(1)),
        Some("plan") => return parse_plan_args(arguments.into_iter().skip(1)),
        Some("format") => return parse_format_args(arguments.into_iter().skip(1)),
        Some(text) if text.starts_with('-') && text != "-" && text != "--" => {
            return Err(CliError::UnknownOption(text.to_owned()));
        }
        _ => {}
    }

    let mut operands = arguments.into_iter();
    let first = operands.next().expect("a first argument was observed");
    let path = if first == "--" {
        operands
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| CliError::UnknownOption("--".to_owned()))?
    } else {
        PathBuf::from(first)
    };
    let arguments = operands
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| CliError::InvalidScriptArgument)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Invocation {
        mode: Mode::Script { path, arguments },
    })
}

fn parse_plan_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    parse_single_source(arguments, true)
}

fn parse_check_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.iter().any(|argument| argument == "--project") {
        parse_project_check_args(&arguments)
    } else {
        parse_single_source(arguments, false)
    }
}

fn parse_project_check_args(arguments: &[OsString]) -> Result<Invocation, CliError> {
    let mut project = None;
    let mut task = None;
    let mut environment = None;
    let mut authority = None;
    let mut tools = None;
    let mut inputs = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].to_str().ok_or_else(|| {
            CliError::InvalidProjectArgument("project option names must be UTF-8".to_owned())
        })?;
        index += 1;
        match option {
            "--project" => set_path_option(&mut project, "--project", arguments, &mut index)?,
            "--task" => set_text_option(&mut task, "--task", arguments, &mut index)?,
            "--environment" => {
                set_text_option(&mut environment, "--environment", arguments, &mut index)?;
            }
            "--authority" => {
                set_path_option(&mut authority, "--authority", arguments, &mut index)?;
            }
            "--tools" => set_path_option(&mut tools, "--tools", arguments, &mut index)?,
            "--input" => {
                let value = text_option_value("--input", arguments, &mut index)?;
                let (name, value) = value.split_once('=').ok_or_else(|| {
                    CliError::InvalidProjectArgument("--input requires name=value".to_owned())
                })?;
                if name.is_empty() {
                    return Err(CliError::InvalidProjectArgument(
                        "--input name cannot be empty".to_owned(),
                    ));
                }
                if inputs.iter().any(|(existing, _)| existing == name) {
                    return Err(CliError::InvalidProjectArgument(format!(
                        "duplicate --input name '{name}'"
                    )));
                }
                inputs.push((name.to_owned(), value.to_owned()));
            }
            "--help" if arguments.len() == 1 => {
                return Ok(Invocation {
                    mode: Mode::CheckHelp,
                });
            }
            value => {
                return Err(CliError::InvalidProjectArgument(format!(
                    "unexpected project check argument '{value}'"
                )));
            }
        }
    }
    Ok(Invocation {
        mode: Mode::ProjectCheck {
            project: required_option(project, "--project")?,
            task: required_option(task, "--task")?,
            environment: required_option(environment, "--environment")?,
            authority: required_option(authority, "--authority")?,
            tools: required_option(tools, "--tools")?,
            inputs,
        },
    })
}

fn parse_task_args<I>(arguments: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.as_slice() == [OsString::from("--help")] {
        return Ok(Invocation {
            mode: Mode::TaskHelp,
        });
    }
    if arguments.first().and_then(|value| value.to_str()) != Some("inspect") {
        return Err(CliError::InvalidProjectArgument(
            "task requires `inspect --project opaal.toml TASK`".to_owned(),
        ));
    }
    let mut project = None;
    let mut task = None;
    let mut index = 1;
    while index < arguments.len() {
        if arguments[index] == "--project" {
            index += 1;
            if project.is_some() {
                return Err(CliError::DuplicateOption("--project"));
            }
            project = Some(PathBuf::from(arguments.get(index).ok_or_else(|| {
                CliError::InvalidProjectArgument("--project requires a path".to_owned())
            })?));
            index += 1;
        } else if task.is_none() {
            task = Some(arguments[index].clone().into_string().map_err(|_| {
                CliError::InvalidProjectArgument("task name must be UTF-8".to_owned())
            })?);
            index += 1;
        } else {
            return Err(CliError::InvalidProjectArgument(
                "task inspect accepts exactly one task name".to_owned(),
            ));
        }
    }
    Ok(Invocation {
        mode: Mode::TaskInspect {
            project: required_option(project, "--project")?,
            task: required_option(task, "TASK")?,
        },
    })
}

fn required_option<T>(value: Option<T>, name: &'static str) -> Result<T, CliError> {
    value.ok_or_else(|| CliError::InvalidProjectArgument(format!("missing required {name}")))
}

fn set_path_option(
    slot: &mut Option<PathBuf>,
    name: &'static str,
    arguments: &[OsString],
    index: &mut usize,
) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError::DuplicateOption(name));
    }
    *slot = Some(PathBuf::from(arguments.get(*index).ok_or_else(|| {
        CliError::InvalidProjectArgument(format!("{name} requires a value"))
    })?));
    *index += 1;
    Ok(())
}

fn set_text_option(
    slot: &mut Option<String>,
    name: &'static str,
    arguments: &[OsString],
    index: &mut usize,
) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError::DuplicateOption(name));
    }
    *slot = Some(text_option_value(name, arguments, index)?.to_owned());
    Ok(())
}

fn text_option_value<'a>(
    name: &str,
    arguments: &'a [OsString],
    index: &mut usize,
) -> Result<&'a str, CliError> {
    let value = arguments
        .get(*index)
        .ok_or_else(|| CliError::InvalidProjectArgument(format!("{name} requires a value")))?
        .to_str()
        .ok_or_else(|| CliError::InvalidProjectArgument(format!("{name} requires UTF-8")))?;
    *index += 1;
    Ok(value)
}

fn parse_single_source<I>(arguments: I, plan: bool) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut help = false;
    let mut source = None;
    let mut options_ended = false;
    for argument in arguments {
        if !options_ended {
            match argument.to_str() {
                Some("--help") => {
                    if help {
                        return Err(CliError::DuplicateOption("--help"));
                    }
                    help = true;
                    continue;
                }
                Some("--") => {
                    options_ended = true;
                    continue;
                }
                Some("-") => {
                    return Err(if plan {
                        CliError::StdinPlanSource
                    } else {
                        CliError::StdinCheckSource
                    });
                }
                Some(text) if text.starts_with('-') => {
                    return Err(CliError::UnknownOption(text.to_owned()));
                }
                _ => {}
            }
        } else if argument == "-" {
            return Err(if plan {
                CliError::StdinPlanSource
            } else {
                CliError::StdinCheckSource
            });
        }
        if source.replace(PathBuf::from(&argument)).is_some() {
            return Err(if plan {
                CliError::UnexpectedPlanSource(argument.to_string_lossy().into_owned())
            } else {
                CliError::UnexpectedCheckSource(argument.to_string_lossy().into_owned())
            });
        }
    }
    if help {
        if let Some(source) = source {
            return Err(if plan {
                CliError::UnexpectedPlanSource(source.to_string_lossy().into_owned())
            } else {
                CliError::UnexpectedCheckSource(source.to_string_lossy().into_owned())
            });
        }
        return Ok(Invocation {
            mode: if plan {
                Mode::PlanHelp
            } else {
                Mode::CheckHelp
            },
        });
    }
    let source = source.ok_or(if plan {
        CliError::MissingPlanSource
    } else {
        CliError::MissingCheckSource
    })?;
    Ok(Invocation {
        mode: if plan {
            Mode::Plan { source }
        } else {
            Mode::Check { source }
        },
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
        if !options_ended {
            match argument.to_str() {
                Some("--check") => {
                    select_format_operation(&mut operation, FormatOperation::Check)?;
                }
                Some("--write") => {
                    select_format_operation(&mut operation, FormatOperation::Write)?;
                }
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
        } else if argument == "-" {
            return Err(CliError::StdinFormatPath);
        } else {
            paths.push(PathBuf::from(argument));
        }
    }
    if help {
        return Ok(Invocation {
            mode: Mode::FormatHelp,
        });
    }
    let operation = operation.ok_or(CliError::MissingFormatOperation)?;
    if paths.is_empty() {
        return Err(CliError::MissingFormatPath);
    }
    Ok(Invocation {
        mode: Mode::Format { operation, paths },
    })
}

fn select_format_operation(
    selected: &mut Option<FormatOperation>,
    candidate: FormatOperation,
) -> Result<(), CliError> {
    match *selected {
        None => *selected = Some(candidate),
        Some(existing) if existing == candidate => {
            return Err(CliError::DuplicateOption(match candidate {
                FormatOperation::Check => "--check",
                FormatOperation::Write => "--write",
            }));
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
    fn top_level_modes_have_one_opaal_personality() {
        assert_eq!(parse(&[]).unwrap().mode, Mode::Interactive);
        assert_eq!(parse(&["--help"]).unwrap().mode, Mode::Help);
        assert_eq!(parse(&["--version"]).unwrap().mode, Mode::Version);
        for legacy in [
            "--no-config",
            "--no-history",
            "--async-capsule",
            "--opaal-repl-fixture",
        ] {
            assert_eq!(
                parse(&[legacy]),
                Err(CliError::UnknownOption(legacy.to_owned()))
            );
        }
    }

    #[test]
    fn script_path_and_arguments_are_lossless_and_ordered() {
        assert_eq!(
            parse(&["--", "run.opaal", "--flag", "value"]).unwrap().mode,
            Mode::Script {
                path: PathBuf::from("run.opaal"),
                arguments: vec!["--flag".to_owned(), "value".to_owned()],
            }
        );
    }

    #[test]
    fn check_and_plan_require_one_explicit_source() {
        assert_eq!(parse(&["check"]), Err(CliError::MissingCheckSource));
        assert_eq!(parse(&["plan"]), Err(CliError::MissingPlanSource));
        assert_eq!(
            parse(&["check", "root.opaal"]).unwrap().mode,
            Mode::Check {
                source: PathBuf::from("root.opaal")
            }
        );
        assert_eq!(
            parse(&["plan", "root.opaal"]).unwrap().mode,
            Mode::Plan {
                source: PathBuf::from("root.opaal")
            }
        );
    }

    #[test]
    fn format_requires_one_operation_and_at_least_one_path() {
        assert_eq!(parse(&["format"]), Err(CliError::MissingFormatOperation));
        assert_eq!(
            parse(&["format", "--check"]),
            Err(CliError::MissingFormatPath)
        );
        assert_eq!(
            parse(&["format", "--write", "a.opaal", "b.opaal"])
                .unwrap()
                .mode,
            Mode::Format {
                operation: FormatOperation::Write,
                paths: vec![PathBuf::from("a.opaal"), PathBuf::from("b.opaal")],
            }
        );
    }

    #[test]
    fn project_check_and_task_inspection_require_the_exact_explicit_shape() {
        assert_eq!(
            parse(&[
                "check",
                "--project",
                "opaal.toml",
                "--task",
                "release",
                "--environment",
                "ci",
                "--authority",
                "authority.toml",
                "--tools",
                "tools.toml",
                "--input",
                "candidate=artifact.tar",
            ])
            .unwrap()
            .mode,
            Mode::ProjectCheck {
                project: PathBuf::from("opaal.toml"),
                task: "release".to_owned(),
                environment: "ci".to_owned(),
                authority: PathBuf::from("authority.toml"),
                tools: PathBuf::from("tools.toml"),
                inputs: vec![("candidate".to_owned(), "artifact.tar".to_owned())],
            }
        );
        assert_eq!(
            parse(&["task", "inspect", "--project", "opaal.toml", "release"])
                .unwrap()
                .mode,
            Mode::TaskInspect {
                project: PathBuf::from("opaal.toml"),
                task: "release".to_owned(),
            }
        );
        assert_eq!(
            parse(&["check", "--project", "one.toml", "--project", "two.toml",]),
            Err(CliError::DuplicateOption("--project"))
        );
        assert_eq!(
            parse(&["check", "--project", "opaal.toml", "--format", "json",]),
            Err(CliError::InvalidProjectArgument(
                "unexpected project check argument '--format'".to_owned()
            ))
        );
        assert_eq!(
            parse(&[
                "check",
                "--project",
                "opaal.toml",
                "--input",
                "missing-separator",
            ]),
            Err(CliError::InvalidProjectArgument(
                "--input requires name=value".to_owned()
            ))
        );
        assert_eq!(
            parse(&[
                "check",
                "--project",
                "opaal.toml",
                "--input",
                "candidate=one",
                "--input",
                "candidate=two",
            ]),
            Err(CliError::InvalidProjectArgument(
                "duplicate --input name 'candidate'".to_owned()
            ))
        );
    }
}
