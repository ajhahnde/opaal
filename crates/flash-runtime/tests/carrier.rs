#![forbid(unsafe_code)]

//! Source-independent pipeline-carrier fault classification shared by runtime
//! preflight and non-executing static analysis.

use flash_runtime::carrier::{
    CarrierBridge, PipelineCarrierFault, StageCarrierContract, analyze_pipeline_carriers,
};
use flash_runtime::command::{Carrier, CommandOutput};
use flash_syntax::PipeOperator;

fn known(
    name: &str,
    accepted: impl IntoIterator<Item = Carrier>,
    output: Carrier,
) -> StageCarrierContract {
    StageCarrierContract::known(name, accepted, CommandOutput::Fixed(output))
}

#[test]
fn a_structured_only_head_reports_its_stage_and_contract() {
    let faults = analyze_pipeline_carriers(
        &[known("each", [Carrier::ValueStream], Carrier::ValueStream)],
        &[],
    );

    assert_eq!(
        faults,
        [PipelineCarrierFault::HeadInput {
            stage: 0,
            command: "each".to_owned(),
            accepted: vec![Carrier::ValueStream],
        }]
    );
}

#[test]
fn a_merged_edge_reports_before_a_general_acceptance_check() {
    let faults = analyze_pipeline_carriers(
        &[
            known("gen", [Carrier::Empty], Carrier::ValueStream),
            known("sink", [Carrier::ValueStream], Carrier::ValueStream),
        ],
        &[PipeOperator::StdoutAndStderr],
    );

    assert_eq!(
        faults,
        [PipelineCarrierFault::MergedEdgeNotByteStream {
            edge: 0,
            producer_command: "gen".to_owned(),
            produced: Carrier::ValueStream,
        }]
    );
}

#[test]
fn an_incompatible_edge_retains_actionable_repair_data() {
    let faults = analyze_pipeline_carriers(
        &[
            known("gen", [Carrier::Empty], Carrier::ValueStream),
            known("cat", [Carrier::ByteStream], Carrier::ByteStream),
            known("where", [Carrier::ValueStream], Carrier::ValueStream),
        ],
        &[PipeOperator::Stdout, PipeOperator::Stdout],
    );

    assert_eq!(faults.len(), 2);
    let PipelineCarrierFault::CarrierMismatch { edge, mismatch } = &faults[0] else {
        panic!("expected the structured-to-byte mismatch first: {faults:?}");
    };
    assert_eq!(*edge, 0);
    assert_eq!(mismatch.producer_command, "gen");
    assert_eq!(mismatch.consumer_command, "cat");
    assert_eq!(mismatch.bridge, Some(CarrierBridge::StructuredToByte));

    let PipelineCarrierFault::CarrierMismatch { edge, mismatch } = &faults[1] else {
        panic!("expected the byte-to-structured mismatch second: {faults:?}");
    };
    assert_eq!(*edge, 1);
    assert_eq!(mismatch.producer_command, "cat");
    assert_eq!(mismatch.consumer_command, "where");
    assert_eq!(mismatch.bridge, Some(CarrierBridge::ByteToStructured));
}

#[test]
fn unknown_contracts_suppress_only_dependent_edges() {
    let faults = analyze_pipeline_carriers(
        &[
            StageCarrierContract::unknown(),
            known("gen", [Carrier::Empty], Carrier::ValueStream),
            known("cat", [Carrier::ByteStream], Carrier::ByteStream),
        ],
        &[PipeOperator::StdoutAndStderr, PipeOperator::Stdout],
    );

    assert_eq!(faults.len(), 1);
    assert!(matches!(
        &faults[0],
        PipelineCarrierFault::CarrierMismatch { edge: 1, mismatch }
            if mismatch.bridge == Some(CarrierBridge::StructuredToByte)
    ));
}

#[test]
fn passthrough_output_becomes_unknown_only_when_its_input_is_unknown() {
    let faults = analyze_pipeline_carriers(
        &[
            StageCarrierContract::unknown(),
            StageCarrierContract::known(
                "pass",
                [Carrier::ByteStream, Carrier::ValueStream],
                CommandOutput::SameAsInput,
            ),
            known("cat", [Carrier::ByteStream], Carrier::ByteStream),
        ],
        &[PipeOperator::Stdout, PipeOperator::Stdout],
    );

    assert!(faults.is_empty());
}
