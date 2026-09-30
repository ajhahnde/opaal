#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opaal_platform::FakePlatform;
use opaal_runtime::eval::FakeClock;
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::session::{Session, SubmitOutcome};
use opaal_runtime::{Environment, Value};

#[derive(Default)]
struct Sources {
    text: RefCell<BTreeMap<PathBuf, String>>,
    reads: RefCell<Vec<PathBuf>>,
}

impl Sources {
    fn put(&self, path: &str, text: &str) {
        self.text
            .borrow_mut()
            .insert(PathBuf::from(path), text.to_owned());
    }
}

impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let candidate = candidate.components().collect::<PathBuf>();
        if self.text.borrow().contains_key(&candidate) {
            Ok(candidate)
        } else {
            Err(ModulePathError::new("missing source"))
        }
    }
}

impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.reads.borrow_mut().push(module.path().to_path_buf());
        self.text
            .borrow()
            .get(module.path())
            .map(|text| text.as_bytes().to_vec())
            .ok_or_else(|| ModuleSourceError::new("missing source"))
    }
}

struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("pure initialization must not probe executables")
    }
}

fn session() -> Session {
    Session::new("/work", Environment::new(), SessionOptions::default())
}

fn submit(session: &mut Session, sources: &Sources, text: &str) -> Value {
    let (outcome, value) = session
        .submit_with_source_loader(
            "cell",
            text,
            sources,
            sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_or_else(|error| panic!("{}\n{text}", error.render()));
    assert_eq!(outcome, SubmitOutcome::Continued);
    value
}

#[test]
fn same_report_module_builds_and_formats_values_across_cells() {
    let sources = Sources::default();
    sources.put(
        "/work/report.opaal",
        include_str!("../../../tests/golden/data-processing/report.opaal"),
    );
    let mut session = session();
    submit(&mut session, &sources, "import './report.opaal' as report");
    submit(&mut session, &sources, "import std::data as data");
    submit(
        &mut session,
        &sources,
        "let report_value = report::build({jobs: []})",
    );
    let encoded = submit(&mut session, &sources, "data::json_encode(report_value)");
    assert_eq!(
        encoded,
        Value::bytes(
            include_str!("../../../tests/golden/data-processing/valid/empty.expected.json")
                .trim_end()
                .as_bytes()
                .to_vec()
        )
    );
    assert_eq!(
        *sources.reads.borrow(),
        [PathBuf::from("/work/report.opaal")]
    );
}

#[test]
fn canonical_snapshots_survive_source_changes_and_transitive_nominals() {
    let sources = Sources::default();
    sources.put("/work/model.opaal", "type Item = { value: Int }\ndef get(item: Item) -> Int { item.value }\nexport { Item, get }\n");
    sources.put(
        "/work/facade.opaal",
        "import './model.opaal' as model\nexport { model }\n",
    );
    let mut session = session();
    submit(&mut session, &sources, "import './facade.opaal' as api");
    submit(
        &mut session,
        &sources,
        "let item: api::model::Item = api::model::Item {value: 7}",
    );
    sources.put("/work/model.opaal", "invalid source after import");
    submit(&mut session, &sources, "import './model.opaal' as same");
    assert_eq!(
        submit(&mut session, &sources, "same::get(item)"),
        Value::Int(7)
    );
    assert_eq!(sources.reads.borrow().len(), 2);
}

#[test]
fn failed_initialization_does_not_publish_and_prior_statements_survive() {
    let sources = Sources::default();
    sources.put("/work/good.opaal", "let answer = 42\nexport { answer }\n");
    sources.put("/work/bad.opaal", "let broken = 1 / 0\nexport { broken }\n");
    let mut session = session();
    let error = session
        .submit_with_source_loader(
            "cell",
            "let before = 9\nimport './good.opaal' as good\nimport './bad.opaal' as bad",
            &sources,
            &sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(error.render().contains("bad.opaal"), "{}", error.render());
    assert_eq!(
        submit(&mut session, &sources, "before + good::answer"),
        Value::Int(51)
    );
    assert!(session.scope().get("bad::broken").is_none());
    sources.put("/work/bad.opaal", "let broken = 3\nexport { broken }\n");
    submit(&mut session, &sources, "import './bad.opaal' as bad");
    assert_eq!(submit(&mut session, &sources, "bad::broken"), Value::Int(3));
}

#[test]
fn local_source_capability_is_explicit_and_initializers_have_no_host() {
    let sources = Sources::default();
    sources.put(
        "/work/hostile.opaal",
        "^danger\nlet answer = 1\nexport { answer }\n",
    );
    let snapshot = opaal_runtime::NativeSessionSnapshot::from_snapshot(
        "/work",
        Vec::<(String, String)>::new(),
        opaal_runtime::HostEnvironmentLimits::OPAAL,
    )
    .unwrap();
    let mut session = Session::from_ambient_snapshot(snapshot, SessionOptions::default());
    let denied = session
        .submit_with_value(
            "cell",
            "import './hostile.opaal' as hostile",
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(denied.render().contains("MOD013"));
    assert!(sources.reads.borrow().is_empty());
    let denied = session
        .submit_with_source_loader(
            "cell",
            "import './hostile.opaal' as hostile",
            &sources,
            &sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(denied.render().contains("hostile.opaal"));
    assert!(session.scope().get("hostile::answer").is_none());
}

#[test]
fn dependency_and_cell_share_the_step_budget_and_failed_cells_can_retry() {
    use opaal_runtime::eval::{CancellationToken, EvalLimits, ResourceBudget};
    let sources = Sources::default();
    sources.put("/work/a.opaal", "let answer = 42\nexport { answer }\n");
    let run = |steps, text: &str| {
        let mut session = session();
        let limits =
            EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::steps(steps));
        let outcome = session.submit_with_source_loader_and_limits(
            "cell",
            text,
            &sources,
            &sources,
            &limits,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        );
        (session, outcome)
    };
    let import = "import './a.opaal' as a";
    let whole = "import './a.opaal' as a\nlet answer = a::answer";
    let import_steps = (0..32).find(|steps| run(*steps, import).1.is_ok()).unwrap();
    let whole_steps = (0..32).find(|steps| run(*steps, whole).1.is_ok()).unwrap();
    assert!(whole_steps > import_steps);
    let (mut limited, failure) = run(whole_steps - 1, whole);
    assert!(failure.is_err());
    assert_eq!(limited.scope().get("a::answer"), Some(&Value::Int(42)));
    assert!(limited.scope().get("answer").is_none());
    assert_eq!(submit(&mut limited, &sources, "a::answer"), Value::Int(42));
    let (mut limited, failure) = run(import_steps - 1, import);
    assert!(failure.is_err());
    assert!(limited.scope().get("a::answer").is_none());
    submit(&mut limited, &sources, import);
    assert_eq!(submit(&mut limited, &sources, "a::answer"), Value::Int(42));
}

#[test]
fn cancellation_at_every_observed_import_poll_leaves_no_alias_or_snapshot() {
    use opaal_runtime::eval::{CancellationToken, EvalLimits, ResourceBudget};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sources = Sources::default();
    sources.put("/work/a.opaal", "let answer = 42\nexport { answer }\n");
    let run = |cancel_at| {
        let mut session = session();
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&polls);
        let token = CancellationToken::from_fn(move || {
            observed.fetch_add(1, Ordering::Relaxed) >= cancel_at
        });
        let limits = EvalLimits::pure_opaal(token, ResourceBudget::opaal());
        let outcome = session
            .submit_with_source_loader_and_limits(
                "cell",
                "import './a.opaal' as a",
                &sources,
                &sources,
                &limits,
                &NoExecutables,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap();
        (session, outcome.0, polls.load(Ordering::Relaxed))
    };
    let count = run(usize::MAX).2;
    for cancel_at in 0..count {
        let (mut cancelled, outcome, _) = run(cancel_at);
        assert!(
            matches!(outcome, SubmitOutcome::Cancelled(_)),
            "poll {cancel_at}: {outcome:?}"
        );
        assert!(cancelled.scope().get("a::answer").is_none());
        submit(&mut cancelled, &sources, "import './a.opaal' as a");
        assert_eq!(
            submit(&mut cancelled, &sources, "a::answer"),
            Value::Int(42)
        );
    }
}

#[test]
fn aliases_keep_distinct_nominals_and_standard_generic_outcomes_across_cells() {
    let sources = Sources::default();
    for path in ["/work/one.opaal", "/work/two.opaal"] {
        sources.put(path, "type Item = { value: Int }\ndef get(item: Item) -> Int { item.value }\nexport { Item, get }\n");
    }
    let mut session = session();
    submit(
        &mut session,
        &sources,
        "import './one.opaal' as one\nimport './one.opaal' as same\nimport './two.opaal' as two\nimport std::outcome as outcome\nimport std::list as list",
    );
    submit(
        &mut session,
        &sources,
        "let item: same::Item = one::Item {value: 3}",
    );
    assert_eq!(
        submit(&mut session, &sources, "same::get(item)"),
        Value::Int(3)
    );
    let failed = session.submit_with_source_loader(
        "cell",
        "two::get(item)",
        &sources,
        &sources,
        &NoExecutables,
        &FakePlatform::full(),
        &FakeClock::new(),
        &mut Vec::new(),
    );
    assert!(
        failed.is_err(),
        "different defining modules cannot share a nominal identity"
    );
    submit(
        &mut session,
        &sources,
        "let found: outcome::Option[Int] = list::find[Int]([1, 2], {|x| x == 2})",
    );
    assert_eq!(
        submit(
            &mut session,
            &sources,
            "match found { outcome::Option::Some(x) => { x }; outcome::Option::None => { 0 } }"
        ),
        Value::Int(2)
    );
    assert_eq!(sources.reads.borrow().len(), 2);
}

#[test]
fn alias_collisions_and_imported_syntax_errors_preserve_the_session() {
    let sources = Sources::default();
    sources.put("/work/bad.opaal", "def broken(\n");
    let mut session = session();
    submit(&mut session, &sources, "let existing = 4");
    let error = session
        .submit_with_source_loader(
            "cell",
            "import './bad.opaal' as broken",
            &sources,
            &sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(error.render().contains("bad.opaal"), "{}", error.render());
    assert_eq!(submit(&mut session, &sources, "existing"), Value::Int(4));
    sources.put("/work/bad.opaal", "let value = 1\nexport { value }\n");
    let error = session
        .submit_with_source_loader(
            "cell",
            "import './bad.opaal' as existing",
            &sources,
            &sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(error.render().contains("MOD011"));
    assert_eq!(submit(&mut session, &sources, "existing"), Value::Int(4));
    submit(&mut session, &sources, "import './bad.opaal' as good");
    assert_eq!(submit(&mut session, &sources, "good::value"), Value::Int(1));
}

#[test]
fn retained_source_count_accepts_the_boundary_and_refuses_first_excess() {
    let sources = Sources::default();
    let mut session = session();
    for _ in 0..256 {
        assert_eq!(submit(&mut session, &sources, "1"), Value::Int(1));
    }
    let failed = session
        .submit_with_source_loader(
            "cell",
            "1",
            &sources,
            &sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(failed.render().contains("retained source limit"));
    assert!(sources.reads.borrow().is_empty());
}

#[test]
fn a_declaration_cannot_publish_types_from_a_later_local_import() {
    let sources = Sources::default();
    sources.put(
        "/work/bad.opaal",
        "type Item = { value: Int }\nlet broken = 1 / 0\nexport { Item }\n",
    );
    let mut session = session();
    let error = session
        .submit_with_source_loader(
            "cell",
            "let before = 9\ntype Wrapper = { item: bad::Item }\nimport './bad.opaal' as bad",
            &sources,
            &sources,
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(error.render().contains("initialize"), "{}", error.render());
    assert_eq!(submit(&mut session, &sources, "before"), Value::Int(9));
    sources.put(
        "/work/bad.opaal",
        "type Item = { value: Int }\nexport { Item }\n",
    );
    submit(
        &mut session,
        &sources,
        "import './bad.opaal' as bad\ntype Wrapper = { item: bad::Item }",
    );
    submit(
        &mut session,
        &sources,
        "let wrapped = Wrapper {item: bad::Item {value: 7}}",
    );
    assert_eq!(
        submit(&mut session, &sources, "wrapped.item.value"),
        Value::Int(7)
    );
}
