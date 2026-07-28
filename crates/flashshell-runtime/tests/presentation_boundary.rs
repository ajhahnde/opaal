//! Acceptance coverage for the carrier-aware terminal-presentation boundary.
//!
//! Human rendering is available only to a final structured carrier at an
//! interactive terminal. Every byte-producing destination stays byte-native.

use flashshell_runtime::command::Carrier;
use flashshell_runtime::presentation::{OutputDestination, select_terminal_presentation};

#[test]
fn an_interactive_structured_carrier_receives_the_terminal_width() {
    for carrier in [Carrier::Value, Carrier::ValueStream] {
        let selected = select_terminal_presentation(
            carrier,
            OutputDestination::InteractiveTerminal { columns: 73 },
        )
        .expect("interactive structured output should be presentable")
        .expect("structured output should receive a presentation token");

        assert_eq!(selected.columns(), 73);
    }
}

#[test]
fn every_noninteractive_structured_destination_requires_serialization() {
    for destination in [
        OutputDestination::Redirected,
        OutputDestination::Captured,
        OutputDestination::ExternalProcess,
        OutputDestination::NonInteractive,
    ] {
        let error = select_terminal_presentation(Carrier::ValueStream, destination)
            .expect_err("structured display must not cross a byte destination");

        assert_eq!(error.carrier(), Carrier::ValueStream);
        assert_eq!(error.destination(), destination);
        assert!(error.to_string().contains("explicit `encode`/`to`"));
        assert!(error.to_string().contains("terminal rendering"));
    }
}

#[test]
fn byte_and_empty_carriers_never_enter_terminal_presentation() {
    for carrier in [Carrier::ByteStream, Carrier::Empty] {
        for destination in [
            OutputDestination::InteractiveTerminal { columns: 80 },
            OutputDestination::Redirected,
            OutputDestination::Captured,
            OutputDestination::ExternalProcess,
            OutputDestination::NonInteractive,
        ] {
            assert_eq!(
                select_terminal_presentation(carrier, destination)
                    .expect("byte-native carriers need no serializer"),
                None
            );
        }
    }
}
