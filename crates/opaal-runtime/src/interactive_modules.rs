//! Source snapshots and pure initialization for explicitly opted-in sessions.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opaal_syntax::{ModuleImportSource, Script, SourceFile, StatementKind};

use crate::eval::{
    EvalLimits, HostedEvaluationFailure, HostedEvaluationOutcome, ResourceBudget,
    evaluate_module_initializer,
};
use crate::module::{
    AnalysisControl, AnalysisLimitKind, AnalysisLimits, ModuleAnalysisOutcome, ModuleCanonicalizer,
    ModuleId, ModulePathError, ModuleProgram, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader, RuntimeBindingTypes,
};
use crate::script::{declare_qualified_alias_values, module_initialization_order};
use crate::{ScopeStack, Value};

pub(crate) const MAX_RETAINED_SOURCES: usize = 256;
pub(crate) const MAX_RETAINED_SOURCE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_RETAINED_ITEMS: u64 = 1_000_000;
pub(crate) const MAX_RETAINED_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct InteractiveModules {
    pub(crate) programs: Vec<Arc<ModuleProgram>>,
    pub(crate) sources: BTreeMap<ModuleId, SourceFile>,
    pub(crate) resolutions: BTreeMap<PathBuf, PathBuf>,
    pub(crate) instances: BTreeMap<ModuleId, BTreeMap<String, Value>>,
    pub(crate) binding_types: Option<Arc<RuntimeBindingTypes>>,
    pub(crate) cell_bytes: usize,
    pub(crate) cell_count: usize,
    pub(crate) items: u64,
    pub(crate) bytes: u64,
    pub(crate) admitted_statements: usize,
}

pub(crate) struct PreparedImports {
    pub(crate) programs: Vec<Arc<ModuleProgram>>,
    pub(crate) imports: BTreeMap<usize, Arc<ModuleProgram>>,
    pub(crate) resolutions: BTreeMap<PathBuf, PathBuf>,
}

pub(crate) enum PrepareError {
    Diagnostic(String),
    Cancelled,
}
impl From<String> for PrepareError {
    fn from(message: String) -> Self {
        Self::Diagnostic(message)
    }
}
impl From<&str> for PrepareError {
    fn from(message: &str) -> Self {
        Self::Diagnostic(message.to_owned())
    }
}

impl InteractiveModules {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &self,
        cwd: &Path,
        source: &SourceFile,
        script: &Script,
        canonicalizer: &dyn ModuleCanonicalizer,
        loader: &dyn ModuleSourceLoader,
        next_source: &mut u64,
        control: &AnalysisControl,
    ) -> Result<PreparedImports, PrepareError> {
        if control.is_cancelled() {
            return Err(PrepareError::Cancelled);
        }
        let snapshots = Snapshots {
            canonicalizer,
            loader,
            sources: RefCell::new(self.sources.clone()),
            resolutions: RefCell::new(self.resolutions.clone()),
        };
        let mut programs = self.programs.clone();
        let mut imports = BTreeMap::new();
        let mut work_remaining = 5_000_000;
        let mut nodes_remaining = 1_000_000;
        for statement in script.statements() {
            if control.is_cancelled() {
                return Err(PrepareError::Cancelled);
            }
            let StatementKind::ModuleImport(import) = statement.kind() else {
                continue;
            };
            let ModuleImportSource::Local { path } = import.source else {
                continue;
            };
            let quoted = source.slice(path).unwrap();
            let requested = cwd.join(&quoted[1..quoted.len() - 1]);
            let canonical = snapshots
                .canonicalize(&requested)
                .map_err(|error| error.to_string())?;
            if control.is_cancelled() {
                return Err(PrepareError::Cancelled);
            }
            let target = ModuleId::local(canonical.clone());
            let program = if let Some(program) = programs
                .iter()
                .find(|program| program.graph().root() == &target)
            {
                Arc::clone(program)
            } else {
                // Reserve the entire bounded graph before discovery. Failed analyses
                // also consume IDs, so later diagnostic sources cannot collide.
                let start = u32::try_from(*next_source).map_err(|_| "source identity exhausted")?;
                *next_source = next_source
                    .checked_add(MAX_RETAINED_SOURCES as u64)
                    .filter(|next| *next <= u64::from(u32::MAX))
                    .ok_or("source identity exhausted")?;
                let ids = snapshots
                    .sources
                    .borrow()
                    .iter()
                    .map(|(module, source)| (module.clone(), source.id()))
                    .collect();
                let graph_bytes = programs
                    .iter()
                    .flat_map(|program| program.sources().entries())
                    .map(|entry| entry.source().text().len())
                    .sum::<usize>();
                let graph_sources = programs
                    .iter()
                    .map(|program| program.sources().entries().count())
                    .sum::<usize>();
                let remaining = MAX_RETAINED_SOURCE_BYTES
                    .checked_sub(graph_bytes + self.cell_bytes + source.text().len())
                    .ok_or("interactive retained source limit exceeded")?;
                let module_limit = MAX_RETAINED_SOURCES
                    .checked_sub(graph_sources + self.cell_count + 1)
                    .ok_or("interactive retained source limit exceeded")?;
                let limits = AnalysisLimits::OPAAL
                    .with_limit(AnalysisLimitKind::SourceBytes, remaining as u64)
                    .with_limit(AnalysisLimitKind::Modules, module_limit as u64)
                    .with_limit(AnalysisLimitKind::WorkUnits, work_remaining)
                    .with_limit(AnalysisLimitKind::AstNodes, nodes_remaining);
                let report = match ModuleProgramLoader::new(&snapshots, &snapshots)
                    .with_source_ids(ids, start)
                    .analyze_with_limits_controlled(&canonical, control, limits)
                {
                    ModuleAnalysisOutcome::Complete(report) => *report,
                    ModuleAnalysisOutcome::Cancelled => return Err(PrepareError::Cancelled),
                    ModuleAnalysisOutcome::BudgetExceeded(exceeded) => {
                        return Err(format!("{exceeded}\n").into());
                    }
                };
                work_remaining -= report.usage().get(AnalysisLimitKind::WorkUnits);
                nodes_remaining -= report.usage().get(AnalysisLimitKind::AstNodes);
                let program = report.program().cloned().ok_or_else(|| {
                    crate::module::ModuleProgramLoadError::new(
                        report.issues()[0].error().clone(),
                        report.sources(),
                    )
                    .render()
                    .to_owned()
                })?;
                for entry in program.sources().entries() {
                    snapshots
                        .sources
                        .borrow_mut()
                        .insert(entry.module().clone(), entry.source().clone());
                }
                if snapshots.sources.borrow().len() + self.cell_count + 1 > MAX_RETAINED_SOURCES {
                    return Err("interactive retained source limit exceeded".into());
                }
                if snapshots
                    .sources
                    .borrow()
                    .values()
                    .map(|source| source.text().len())
                    .sum::<usize>()
                    + self.cell_bytes
                    + source.text().len()
                    > MAX_RETAINED_SOURCE_BYTES
                {
                    return Err("interactive retained source limit exceeded".into());
                }
                let program = Arc::new(program);
                programs.push(Arc::clone(&program));
                program
            };
            imports.insert(path.start(), program);
        }
        Ok(PreparedImports {
            programs,
            imports,
            resolutions: snapshots.resolutions.into_inner(),
        })
    }

    pub(crate) fn initialize(
        &mut self,
        program: &Arc<ModuleProgram>,
        limits: &EvalLimits,
        budget: &mut ResourceBudget,
        previous_items: u64,
        previous_bytes: u64,
    ) -> Result<HostedEvaluationOutcome, HostedEvaluationFailure> {
        let mut instances = self.instances.clone();
        let types = Arc::new(program.runtime_binding_types_in_context(vec![Arc::clone(program)]));
        for module in module_initialization_order(program) {
            if instances.contains_key(&module) {
                continue;
            }
            let mut scope = ScopeStack::new();
            for alias in program.aliases().aliases(&module) {
                declare_qualified_alias_values(
                    &mut scope,
                    program,
                    &instances,
                    alias.name(),
                    alias.target(),
                );
            }
            let source = program.sources().source(&module).unwrap();
            match evaluate_module_initializer(
                program.sources().script(&module).unwrap(),
                Arc::new(source.clone()),
                &mut scope,
                limits,
                budget,
                Arc::clone(&types),
            )? {
                HostedEvaluationOutcome::Value(_) => {
                    let exports = program
                        .names()
                        .exports(&module)
                        .filter_map(|export| {
                            scope
                                .get(export.name())
                                .cloned()
                                .map(|value| (export.name().to_owned(), value))
                        })
                        .collect();
                    instances.insert(module, exports);
                }
                other => return Ok(other),
            }
        }
        let cancellation = limits.cancellation_token();
        if cancellation.is_cancelled() {
            return Ok(HostedEvaluationOutcome::Cancelled(
                crate::eval::Cancellation::new(
                    cancellation.reason(),
                    program
                        .sources()
                        .script(program.graph().root())
                        .unwrap()
                        .span(),
                ),
            ));
        }
        if self
            .items
            .checked_add(budget.collection_items() - previous_items)
            .is_none_or(|used| used > MAX_RETAINED_ITEMS)
            || self
                .bytes
                .checked_add(budget.collection_bytes() - previous_bytes)
                .is_none_or(|used| used > MAX_RETAINED_BYTES)
        {
            return Err(HostedEvaluationFailure::Runtime(
                crate::eval::RuntimeError::new(
                    crate::eval::RuntimeErrorKind::ResourceBudgetExceeded,
                    program
                        .sources()
                        .script(program.graph().root())
                        .unwrap()
                        .span(),
                ),
            ));
        }
        self.instances = instances;
        if !self
            .programs
            .iter()
            .any(|previous| previous.graph().root() == program.graph().root())
        {
            self.programs.push(Arc::clone(program));
        }
        for entry in program.sources().entries() {
            self.sources
                .insert(entry.module().clone(), entry.source().clone());
        }
        Ok(HostedEvaluationOutcome::Value(Value::Null))
    }
}

struct Snapshots<'a> {
    canonicalizer: &'a dyn ModuleCanonicalizer,
    loader: &'a dyn ModuleSourceLoader,
    sources: RefCell<BTreeMap<ModuleId, SourceFile>>,
    resolutions: RefCell<BTreeMap<PathBuf, PathBuf>>,
}

impl ModuleCanonicalizer for Snapshots<'_> {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let candidate = candidate.components().collect::<PathBuf>();
        if let Some(canonical) = self.resolutions.borrow().get(&candidate) {
            return Ok(canonical.clone());
        }
        let canonical = self.canonicalizer.canonicalize(&candidate)?;
        self.resolutions
            .borrow_mut()
            .insert(candidate, canonical.clone());
        Ok(canonical)
    }
}

impl ModuleSourceLoader for Snapshots<'_> {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, MAX_RETAINED_SOURCE_BYTES + 1)
    }
    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        if let Some(source) = self.sources.borrow().get(module) {
            return Ok(source.text().as_bytes()[..source.text().len().min(maximum)].to_vec());
        }
        self.loader.load_bounded(module, maximum)
    }
}
