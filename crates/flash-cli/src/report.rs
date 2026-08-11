//! Host-free process-status and stream-reporting policy.

use std::fmt;
use std::io::{self, Write};

use flash_runtime::Status;

/// The classified host result of one CLI report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostExit {
    /// Successful CLI work without a distinct completed program status.
    Success,
    /// A real completed program status, including ordinary codes 1 and 2.
    Code(u8),
    /// A diagnosed shell-owned failure.
    Failure,
    /// Launcher misuse.
    Misuse,
}

impl HostExit {
    /// Return the exact eight-bit status exposed by the host process.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Code(code) => code,
            Self::Failure => 1,
            Self::Misuse => 2,
        }
    }
}

/// A complete CLI report whose streams can be supplied by the caller.
#[derive(Clone, Copy, Debug)]
pub enum HostReport<'a> {
    /// Successful CLI output with no completed program status.
    Success { output: &'a [u8] },
    /// Program output followed by a real completed status.
    Completed {
        status: &'a Status,
        output: &'a [u8],
        diagnostic: &'a [u8],
    },
    /// A pre-rendered shell-owned diagnostic.
    Failure { diagnostic: &'a [u8] },
    /// One launcher-misuse message, without the `fsh:` prefix or newline.
    Misuse { message: &'a str },
}

impl<'a> HostReport<'a> {
    /// Build successful CLI output.
    #[must_use]
    pub const fn success(output: &'a [u8]) -> Self {
        Self::Success { output }
    }

    /// Build program output with its completed status.
    #[must_use]
    pub const fn completed(status: &'a Status, output: &'a [u8]) -> Self {
        Self::Completed {
            status,
            output,
            diagnostic: b"",
        }
    }

    /// Build program output and ordered diagnostics with a completed status.
    #[must_use]
    pub const fn completed_with_diagnostic(
        status: &'a Status,
        output: &'a [u8],
        diagnostic: &'a [u8],
    ) -> Self {
        Self::Completed {
            status,
            output,
            diagnostic,
        }
    }

    /// Build a shell-owned failure from one already-rendered diagnostic.
    #[must_use]
    pub const fn failure(diagnostic: &'a [u8]) -> Self {
        Self::Failure { diagnostic }
    }

    /// Build one launcher-misuse report.
    #[must_use]
    pub const fn misuse(message: &'a str) -> Self {
        Self::Misuse { message }
    }
}

/// Why a completed Flash status cannot be exposed as an eight-bit host status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusMappingError {
    /// The status has neither or both of a code and a signal.
    InvalidStructure,
    /// A completed exit code is outside the host range.
    CodeOutOfRange(i64),
    /// The signal has a name but no numeric identity.
    MissingSignalNumber,
    /// A numeric signal is outside the portable mapping range.
    SignalOutOfRange(i64),
}

impl fmt::Display for StatusMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStructure => {
                formatter.write_str("status must contain exactly one code or signal")
            }
            Self::CodeOutOfRange(code) => {
                write!(formatter, "exit code {code} is outside 0..=255")
            }
            Self::MissingSignalNumber => formatter.write_str("signal has no numeric identity"),
            Self::SignalOutOfRange(signal) => {
                write!(formatter, "signal {signal} is outside 1..=127")
            }
        }
    }
}

/// Map one completed Flash status to the exact host status byte.
pub fn map_status(status: &Status) -> Result<u8, StatusMappingError> {
    map_status_fields(StatusFields {
        code: status.code(),
        signal_number: status.signal().map(flash_runtime::Signal::number),
    })
}

#[derive(Clone, Copy)]
struct StatusFields {
    code: Option<i64>,
    signal_number: Option<Option<i64>>,
}

fn map_status_fields(fields: StatusFields) -> Result<u8, StatusMappingError> {
    match (fields.code, fields.signal_number) {
        (Some(code), None) => {
            u8::try_from(code).map_err(|_| StatusMappingError::CodeOutOfRange(code))
        }
        (None, Some(Some(signal @ 1..=127))) => {
            Ok(128
                + u8::try_from(signal).map_err(|_| StatusMappingError::SignalOutOfRange(signal))?)
        }
        (None, Some(Some(signal))) => Err(StatusMappingError::SignalOutOfRange(signal)),
        (None, Some(None)) => Err(StatusMappingError::MissingSignalNumber),
        _ => Err(StatusMappingError::InvalidStructure),
    }
}

/// Write one report to injected streams and return its classified host result.
///
/// Required writes and flushes are checked. A standard-output failure is
/// reported once through standard error when possible. A standard-error
/// failure is never reported recursively through the same stream.
pub fn write_report<O, D>(report: HostReport<'_>, output: &mut O, diagnostics: &mut D) -> HostExit
where
    O: Write + ?Sized,
    D: Write + ?Sized,
{
    match report {
        HostReport::Success { output: bytes } => {
            if let Err(error) = write_required(output, bytes) {
                report_output_failure(diagnostics, &error);
                HostExit::Failure
            } else {
                HostExit::Success
            }
        }
        HostReport::Completed {
            status,
            output: bytes,
            diagnostic,
        } => {
            if let Err(error) = write_required(output, bytes) {
                report_output_failure(diagnostics, &error);
                return HostExit::Failure;
            }
            if write_required(diagnostics, diagnostic).is_err() {
                return HostExit::Failure;
            }
            match map_status(status) {
                Ok(code) => HostExit::Code(code),
                Err(error) => {
                    let rendered = format!("fsh: cannot represent completed status: {error}\n");
                    let _ = write_required(diagnostics, rendered.as_bytes());
                    HostExit::Failure
                }
            }
        }
        HostReport::Failure { diagnostic } => {
            let _ = write_required(diagnostics, diagnostic);
            HostExit::Failure
        }
        HostReport::Misuse { message } => {
            debug_assert!(!message.contains(['\n', '\r']));
            let rendered = format!("fsh: {message}\n");
            let _ = write_required(diagnostics, rendered.as_bytes());
            HostExit::Misuse
        }
    }
}

fn write_required(writer: &mut (impl Write + ?Sized), bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    writer.write_all(bytes)?;
    writer.flush()
}

fn report_output_failure(diagnostics: &mut (impl Write + ?Sized), error: &io::Error) {
    let rendered = format!("fsh: cannot write standard output: {error}\n");
    let _ = write_required(diagnostics, rendered.as_bytes());
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use flash_runtime::{Duration, Signal, Status};

    use super::{
        HostExit, HostReport, StatusFields, StatusMappingError, map_status, map_status_fields,
        write_report,
    };

    fn exited(code: i64) -> Status {
        Status::exit(code, Duration::ZERO).expect("zero-duration status is valid")
    }

    fn signaled(number: Option<i64>, name: Option<&str>) -> Status {
        let signal = Signal::new(number, name.map(str::to_owned)).expect("signal has an identity");
        Status::signaled(signal, Duration::ZERO).expect("zero-duration status is valid")
    }

    #[test]
    fn exact_completed_codes_are_preserved() {
        for code in [0_u8, 1, 2, 125, 126, 127, 128, 255] {
            assert_eq!(map_status(&exited(i64::from(code))), Ok(code));
        }
    }

    #[test]
    fn numeric_signals_use_the_bounded_portable_mapping() {
        for (signal, expected) in [(1, 129), (2, 130), (15, 143), (127, 255)] {
            assert_eq!(map_status(&signaled(Some(signal), None)), Ok(expected));
        }
    }

    #[test]
    fn unrepresentable_statuses_are_rejected_without_wrapping() {
        assert_eq!(
            map_status(&exited(-1)),
            Err(StatusMappingError::CodeOutOfRange(-1))
        );
        assert_eq!(
            map_status(&exited(256)),
            Err(StatusMappingError::CodeOutOfRange(256))
        );
        assert_eq!(
            map_status(&signaled(None, Some("TERM"))),
            Err(StatusMappingError::MissingSignalNumber)
        );
        for signal in [-1, 0, 128, 256] {
            assert_eq!(
                map_status(&signaled(Some(signal), None)),
                Err(StatusMappingError::SignalOutOfRange(signal))
            );
        }
    }

    #[test]
    fn structurally_invalid_status_fields_are_rejected() {
        assert_eq!(
            map_status_fields(StatusFields {
                code: None,
                signal_number: None,
            }),
            Err(StatusMappingError::InvalidStructure)
        );
        assert_eq!(
            map_status_fields(StatusFields {
                code: Some(0),
                signal_number: Some(Some(2)),
            }),
            Err(StatusMappingError::InvalidStructure)
        );
    }

    #[test]
    fn completion_reports_own_exact_stdout_and_remain_silent_on_stderr() {
        let mut stdout = RecordingWriter::default();
        let mut stderr = RecordingWriter::default();

        let exit = write_report(
            HostReport::completed(&exited(2), b"program output\n"),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, HostExit::Code(2));
        assert_eq!(stdout.bytes, b"program output\n");
        assert_eq!(stdout.flushes, 1);
        assert!(stderr.bytes.is_empty());
        assert_eq!(stderr.flushes, 0);
    }

    #[test]
    fn completed_status_can_carry_ordered_shell_diagnostics() {
        let mut stdout = RecordingWriter::default();
        let mut stderr = RecordingWriter::default();

        let exit = write_report(
            HostReport::completed_with_diagnostic(
                &exited(7),
                b"",
                b"fsh: first background failure\nfsh: second background failure\n",
            ),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, HostExit::Code(7));
        assert!(stdout.bytes.is_empty());
        assert_eq!(
            stderr.bytes,
            b"fsh: first background failure\nfsh: second background failure\n"
        );
        assert_eq!(stderr.flushes, 1);
    }

    #[test]
    fn empty_success_failure_and_misuse_have_distinct_host_results() {
        let mut stdout = RecordingWriter::default();
        let mut stderr = RecordingWriter::default();
        assert_eq!(
            write_report(HostReport::success(b""), &mut stdout, &mut stderr),
            HostExit::Success
        );
        assert!(stdout.bytes.is_empty());
        assert!(stderr.bytes.is_empty());

        assert_eq!(
            write_report(
                HostReport::failure(b"error[RUN001]: failed\n"),
                &mut stdout,
                &mut stderr,
            ),
            HostExit::Failure
        );
        assert!(stdout.bytes.is_empty());
        assert_eq!(stderr.bytes, b"error[RUN001]: failed\n");

        stderr = RecordingWriter::default();
        assert_eq!(
            write_report(
                HostReport::misuse("unexpected option"),
                &mut stdout,
                &mut stderr,
            ),
            HostExit::Misuse
        );
        assert_eq!(stderr.bytes, b"fsh: unexpected option\n");
    }

    #[test]
    fn mapping_failure_is_reported_once_on_stderr() {
        let mut stdout = RecordingWriter::default();
        let mut stderr = RecordingWriter::default();

        let exit = write_report(
            HostReport::completed(&exited(256), b"before failure\n"),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, HostExit::Failure);
        assert_eq!(stdout.bytes, b"before failure\n");
        assert_eq!(stderr.writes, 1);
        assert_eq!(
            String::from_utf8(stderr.bytes).unwrap(),
            "fsh: cannot represent completed status: exit code 256 is outside 0..=255\n"
        );
    }

    #[test]
    fn stdout_write_or_flush_failure_becomes_a_reported_failure() {
        for fault in [Fault::Write, Fault::Flush] {
            let mut stdout = RecordingWriter::with_fault(fault);
            let mut stderr = RecordingWriter::default();

            let exit = write_report(
                HostReport::success(b"program output\n"),
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(exit, HostExit::Failure);
            assert_eq!(stderr.writes, 1);
            assert_eq!(stderr.flushes, 1);
            assert!(
                String::from_utf8(stderr.bytes)
                    .unwrap()
                    .starts_with("fsh: cannot write standard output: injected ")
            );
        }
    }

    #[test]
    fn stderr_failure_is_not_reported_recursively() {
        for fault in [Fault::Write, Fault::Flush] {
            let mut stdout = RecordingWriter::default();
            let mut stderr = RecordingWriter::with_fault(fault);

            let exit = write_report(
                HostReport::failure(b"error[RUN001]: failed\n"),
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(exit, HostExit::Failure);
            assert_eq!(stderr.writes, 1);
            assert!(stderr.flushes <= 1);
        }
    }

    #[derive(Clone, Copy)]
    enum Fault {
        Write,
        Flush,
    }

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
        fault: Option<Fault>,
    }

    impl RecordingWriter {
        fn with_fault(fault: Fault) -> Self {
            Self {
                fault: Some(fault),
                ..Self::default()
            }
        }
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if matches!(self.fault, Some(Fault::Write)) {
                return Err(io::Error::other("injected write failure"));
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if matches!(self.fault, Some(Fault::Flush)) {
                return Err(io::Error::other("injected flush failure"));
            }
            Ok(())
        }
    }
}
