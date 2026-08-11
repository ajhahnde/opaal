//! Source-independent pipeline-carrier contracts and fault classification.
//!
//! Runtime preflight and static source analysis both consume this model. It
//! contains no source spans, scopes, environments, executable probes, or host
//! capabilities; callers attach their own presentation and error surfaces.

use std::collections::BTreeSet;

use flash_syntax::PipeOperator;

use crate::command::{Carrier, CommandOutput};

/// The explicit boundary that repairs a structured/byte carrier crossing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CarrierBridge {
    /// Serialize structured values before a byte consumer.
    StructuredToByte,
    /// Parse bytes before a structured consumer.
    ByteToStructured,
}

/// Actionable detail for one incompatible adjacent carrier edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CarrierMismatch {
    /// The producing stage's display name.
    pub producer_command: String,
    /// The carrier emitted by the producer.
    pub produced: Carrier,
    /// The consuming stage's display name.
    pub consumer_command: String,
    /// The consumer's deterministic accepted carrier set.
    pub accepted: Vec<Carrier>,
    /// The explicit codec/format boundary that repairs the crossing, if any.
    pub bridge: Option<CarrierBridge>,
}

/// One statically known or deliberately unknown stage carrier contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageCarrierContract {
    /// The stage name and complete registered carrier contract are known.
    Known {
        /// Stable diagnostic display name.
        display_name: String,
        /// Accepted input carriers.
        accepted: BTreeSet<Carrier>,
        /// Fixed or input-dependent output carrier.
        output: CommandOutput,
    },
    /// The stage head is dynamic, so no carrier claim is sound.
    Unknown,
}

impl StageCarrierContract {
    /// Builds one known stage contract.
    pub fn known(
        display_name: impl Into<String>,
        accepted: impl IntoIterator<Item = Carrier>,
        output: CommandOutput,
    ) -> Self {
        Self::Known {
            display_name: display_name.into(),
            accepted: accepted.into_iter().collect(),
            output,
        }
    }

    /// Builds a deliberately unknown stage contract.
    #[must_use]
    pub const fn unknown() -> Self {
        Self::Unknown
    }

    fn accepted(&self) -> Option<&BTreeSet<Carrier>> {
        match self {
            Self::Known { accepted, .. } => Some(accepted),
            Self::Unknown => None,
        }
    }

    fn display_name(&self) -> Option<&str> {
        match self {
            Self::Known { display_name, .. } => Some(display_name),
            Self::Unknown => None,
        }
    }

    fn resolve_output(&self, input: Option<Carrier>) -> Option<Carrier> {
        let Self::Known {
            accepted, output, ..
        } = self
        else {
            return None;
        };
        match output {
            CommandOutput::Fixed(output) => Some(*output),
            CommandOutput::SameAsInput => input.filter(|input| accepted.contains(input)),
        }
    }
}

/// One source-independent pipeline carrier fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PipelineCarrierFault {
    /// A known structured-only command cannot begin a pipeline.
    HeadInput {
        /// Zero-based stage index (always zero).
        stage: usize,
        /// The head command's display name.
        command: String,
        /// The command's accepted input carriers.
        accepted: Vec<Carrier>,
    },
    /// A `|&` edge follows a known non-byte producer.
    MergedEdgeNotByteStream {
        /// Zero-based edge index.
        edge: usize,
        /// The producer's display name.
        producer_command: String,
        /// The producer's output carrier.
        produced: Carrier,
    },
    /// A known producer carrier is not accepted by a known consumer.
    CarrierMismatch {
        /// Zero-based edge index.
        edge: usize,
        /// Complete actionable mismatch data.
        mismatch: CarrierMismatch,
    },
}

/// Classifies every independently knowable carrier fault in one pipeline.
///
/// Unknown stage heads suppress only answers that depend on their input or
/// output. Fixed output contracts after them remain available for later edges.
#[must_use]
pub fn analyze_pipeline_carriers(
    stages: &[StageCarrierContract],
    operators: &[PipeOperator],
) -> Vec<PipelineCarrierFault> {
    debug_assert_eq!(operators.len(), stages.len().saturating_sub(1));
    let mut faults = Vec::new();

    if let Some(StageCarrierContract::Known {
        display_name,
        accepted,
        ..
    }) = stages.first()
        && !accepted.contains(&Carrier::Empty)
        && !accepted.contains(&Carrier::ByteStream)
    {
        faults.push(PipelineCarrierFault::HeadInput {
            stage: 0,
            command: display_name.clone(),
            accepted: accepted.iter().copied().collect(),
        });
    }

    let mut outputs = Vec::with_capacity(stages.len());
    for (index, stage) in stages.iter().enumerate() {
        let input = if index == 0 {
            stage.accepted().and_then(|accepted| {
                if accepted.contains(&Carrier::Empty) {
                    Some(Carrier::Empty)
                } else if accepted.contains(&Carrier::ByteStream) {
                    Some(Carrier::ByteStream)
                } else {
                    None
                }
            })
        } else {
            outputs[index - 1]
        };
        outputs.push(stage.resolve_output(input));
    }

    for (edge, operator) in operators.iter().copied().enumerate() {
        let Some(produced) = outputs[edge] else {
            continue;
        };
        let Some(producer_command) = stages[edge].display_name() else {
            continue;
        };
        if operator == PipeOperator::StdoutAndStderr && produced != Carrier::ByteStream {
            faults.push(PipelineCarrierFault::MergedEdgeNotByteStream {
                edge,
                producer_command: producer_command.to_owned(),
                produced,
            });
            continue;
        }
        let Some(accepted) = stages[edge + 1].accepted() else {
            continue;
        };
        if accepted.contains(&produced) {
            continue;
        }
        let consumer_command = stages[edge + 1]
            .display_name()
            .expect("a known carrier set has a display name")
            .to_owned();
        let accepted = accepted.iter().copied().collect::<Vec<_>>();
        faults.push(PipelineCarrierFault::CarrierMismatch {
            edge,
            mismatch: CarrierMismatch {
                producer_command: producer_command.to_owned(),
                produced,
                consumer_command,
                bridge: bridge_for(produced, &accepted),
                accepted,
            },
        });
    }

    faults
}

fn bridge_for(produced: Carrier, accepted: &[Carrier]) -> Option<CarrierBridge> {
    let is_structured = |carrier: Carrier| matches!(carrier, Carrier::Value | Carrier::ValueStream);
    if is_structured(produced) && accepted.contains(&Carrier::ByteStream) {
        Some(CarrierBridge::StructuredToByte)
    } else if produced == Carrier::ByteStream && accepted.iter().copied().any(is_structured) {
        Some(CarrierBridge::ByteToStructured)
    } else {
        None
    }
}
