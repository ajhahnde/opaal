#![no_main]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flash_runtime::ScopeStack;
use flash_runtime::eval::{CancellationToken, EvalLimits, ResourceBudget, evaluate_with_limits};
use flash_runtime::module::{
    AnalysisControl, AnalysisLimitKind, AnalysisLimits, ModuleAnalysisOutcome, ModuleCanonicalizer,
    ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use flash_syntax::LanguageMajor;
use libfuzzer_sys::fuzz_target;

struct FuzzSource<'input> {
    bytes: &'input [u8],
}

impl ModuleCanonicalizer for FuzzSource<'_> {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        (candidate == Path::new("fuzz.fsh"))
            .then(|| candidate.to_path_buf())
            .ok_or_else(|| ModulePathError::new("fuzz imports are unavailable"))
    }
}

impl ModuleSourceLoader for FuzzSource<'_> {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        (module.path() == Path::new("fuzz.fsh"))
            .then(|| self.bytes[..self.bytes.len().min(maximum)].to_vec())
            .ok_or_else(|| ModuleSourceError::new("fuzz imports are unavailable"))
    }
}

fuzz_target!(|data: &[u8]| {
    let (knobs, source) = data.split_at(data.len().min(12));
    let knob = |index: usize| u64::from(knobs.get(index).copied().unwrap_or(32));
    let limits = AnalysisLimits::unlimited()
        .with_limit(AnalysisLimitKind::SourceBytes, knob(0) * 16)
        .with_limit(AnalysisLimitKind::Modules, knob(1) % 8)
        .with_limit(AnalysisLimitKind::ModuleDepth, knob(2) % 8)
        .with_limit(AnalysisLimitKind::AstNodes, knob(3) * 8)
        .with_limit(AnalysisLimitKind::TypeDepth, knob(4) % 32)
        .with_limit(AnalysisLimitKind::GenericInstantiations, knob(5) % 32)
        .with_limit(AnalysisLimitKind::OverloadCandidates, knob(6) % 32)
        .with_limit(AnalysisLimitKind::Diagnostics, knob(7) % 32)
        .with_limit(AnalysisLimitKind::WorkUnits, knob(8) * 32);
    let fuzz = FuzzSource { bytes: source };
    let outcome = ModuleProgramLoader::for_language(&fuzz, &fuzz, LanguageMajor::V2)
        .analyze_with_limits_controlled(Path::new("fuzz.fsh"), &AnalysisControl::never(), limits);

    let ModuleAnalysisOutcome::Complete(report) = outcome else {
        assert!(matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(_)));
        return;
    };
    let Some(program) = report.program() else {
        return;
    };
    let root = program.graph().root();
    let script = program
        .sources()
        .script(root)
        .expect("a complete program retains its root syntax");
    let source = program
        .sources()
        .source(root)
        .expect("a complete program retains its root source");
    let polls = Arc::new(AtomicUsize::new(0));
    let cancel_after = usize::from(knobs.get(9).copied().unwrap_or(u8::MAX));
    let token = CancellationToken::from_fn({
        let polls = Arc::clone(&polls);
        move || polls.fetch_add(1, Ordering::Relaxed) >= cancel_after
    });
    let budget = ResourceBudget::steps(knob(10) * 16)
        .with_call_depth(knob(11) % 32)
        .with_collection_items(knob(3) * 8)
        .with_collection_bytes(knob(4) * 32);
    let _ = evaluate_with_limits(
        script,
        source,
        &mut ScopeStack::new(),
        &EvalLimits::pure_v2(token, budget),
    );
});
