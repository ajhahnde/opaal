#![no_main]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use libfuzzer_sys::fuzz_target;
use opaal_platform::FakePlatform;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::session::{Session, SubmitError, SubmitOutcome};
use opaal_runtime::{BindingMutability, Environment, ScopeStack, Value};

struct Source(String);

impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        (path == Path::new("/fuzz/main.opaal"))
            .then(|| path.to_path_buf())
            .ok_or_else(|| ModulePathError::new("local imports are unavailable"))
    }
}

impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        (module.path() == Path::new("/fuzz/main.opaal"))
            .then(|| self.0.as_bytes().to_vec())
            .ok_or_else(|| ModuleSourceError::new("local imports are unavailable"))
    }
}

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("pure data operations must not resolve executables")
    }
}

fn execute(source: &Source, input: &[u8], limits: EvalLimits) -> PrimaryOutcome<Value, String> {
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "input",
            BindingMutability::Immutable,
            Value::bytes(input.to_vec()),
        )
        .unwrap();
    let mut session = Session::with_scope(
        scope,
        "/fuzz",
        Environment::new(),
        SessionOptions::default(),
    );
    match session.submit_with_source_loader_and_limits(
        "main.opaal",
        &source.0,
        source,
        source,
        &limits,
        &NoExecutables,
        &FakePlatform::full(),
        &FakeClock::new(),
        &mut Vec::new(),
    ) {
        Ok((SubmitOutcome::Continued, value)) => PrimaryOutcome::Completed(value),
        Ok((SubmitOutcome::Cancelled(reason), _)) => PrimaryOutcome::Cancelled(reason),
        Err(SubmitError::Runtime { rendered, .. }) => PrimaryOutcome::Error(rendered),
        other => panic!(
            "generated pure source must analyze without host effects: {other:?}\n{}",
            source.0
        ),
    }
}

fn generated_source(knobs: &[u8], payload: &[u8]) -> String {
    let knob = |index: usize| knobs.get(index).copied().unwrap_or(32);
    let characters = [
        'a', 'b', '/', ' ', 'é', 'α', '中', '😀', '\n', '\t', '\\', '"',
    ];
    let text: String = payload
        .iter()
        .take(128)
        .map(|byte| characters[usize::from(*byte) % characters.len()])
        .collect();
    let text = text.repeat(1 + usize::from(knob(6) % 8));
    let quote = |text: &str| serde_json::to_string(text).expect("strings serialize");
    let text = quote(&text);
    let integers: Vec<_> = payload
        .iter()
        .take(64)
        .map(|byte| i64::from(*byte as i8))
        .collect();
    let list = serde_json::to_string(&integers).expect("integers serialize");
    let count = i64::from(knob(7) as i8);
    let callback = if knob(6) & 0x80 == 0 {
        "{|x: Int| -> Int x + 1}"
    } else {
        "fail"
    };
    let body = match knob(0) % 16 {
        0 => format!(
            "string::replace({text}, 'a', {})",
            quote(&"α😀".repeat(usize::from(knob(6))))
        ),
        1 => format!("string::join(string::split({text}, '/'), 'é')"),
        2 => format!("string::trim({text})"),
        3 => format!("list::map({list}, {callback})"),
        4 => format!("list::filter({list}, {{|x: Int| -> Bool x >= 0}})"),
        5 => format!("list::fold({list}, 0, {{|a: Int, x: Int| -> Int a + x}})"),
        6 => format!("list::sort_by[Int, List[Int]]({list}, {{|x| [x, -x]}})"),
        7 => format!("list::sort(list::reverse[Int]({list}))"),
        8 => format!("list::drop(list::take[Int]({list}, {count}), {count})"),
        9 => format!("list::find({list}, {{|x: Int| -> Bool x >= {count}}})"),
        10 => format!(
            "[list::any({list}, {{|x: Int| -> Bool x > {count}}}), list::all({list}, {{|x: Int| -> Bool x < {count}}}), list::count({list}, {{|x: Int| -> Bool x == {count}}})]"
        ),
        11 => {
            let prefix = "é".repeat(usize::from(knob(6)));
            let left = integers
                .iter()
                .enumerate()
                .map(|(index, value)| format!("{}: {value}", quote(&format!("{prefix}{index:03}"))))
                .collect::<Vec<_>>()
                .join(", ");
            let right = integers
                .iter()
                .enumerate()
                .rev()
                .map(|(index, value)| {
                    let index = if knob(7) & 1 == 0 {
                        index
                    } else {
                        index + integers.len() / 2
                    };
                    format!("{}: {}", quote(&format!("{prefix}{index:03}")), -value)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("record::merge({{ {left} }}, {{ {right} }})")
        }
        12 => format!(
            "let row = {{ a: {list}, b: {text}, c: null }}\nrecord::set(record::select(row, ['b', 'a']), 'c', record::get_or(row, 'c', 7))"
        ),
        13 => "data::json_decode(input)".to_owned(),
        14 => "data::json_decode(input)".to_owned(),
        _ => format!(
            "list::map[Int, List[Int]]({list}, {{|x| list::map[Int, Int]([x, x], {callback})}})"
        ),
    };
    format!(
        "import std::string as string\nimport std::list as list\nimport std::record as record\nimport std::data as data\ndef fail(x: Int) -> Int {{ throw 'callback failure' }}\n{body}"
    )
}

fuzz_target!(|data: &[u8]| {
    let (knobs, payload) = data.split_at(data.len().min(8));
    let knob = |index: usize| u64::from(knobs.get(index).copied().unwrap_or(32));
    let source = Source(generated_source(knobs, payload));
    let nested;
    let payload = if knob(0) % 16 == 14 {
        let depth = (knob(6) % 72) as usize;
        nested = format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes();
        nested.as_slice()
    } else {
        payload
    };
    let baseline = execute(
        &source,
        payload,
        EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
    );
    assert!(matches!(
        &baseline,
        PrimaryOutcome::Completed(_) | PrimaryOutcome::Error(_)
    ));
    let budget = ResourceBudget::steps(knob(1) * 128)
        .with_call_depth(knob(2) % 32)
        .with_collection_items(knob(3) * 8)
        .with_collection_bytes(knob(4) * 128);
    let constrained = execute(
        &source,
        payload,
        EvalLimits::pure_opaal(CancellationToken::never(), budget),
    );
    match &constrained {
        PrimaryOutcome::Completed(_) => assert_eq!(&constrained, &baseline),
        PrimaryOutcome::Error(message) => {
            assert!(
                message.contains("resource budget") || constrained == baseline,
                "a smaller budget must preserve the original error or report exhaustion: {constrained:?}; baseline: {baseline:?}"
            );
        }
        other => panic!("unexpected pure outcome: {other:?}"),
    }
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&polls);
    let stop = knob(5) as usize * 4;
    let cancel =
        CancellationToken::from_fn(move || observed.fetch_add(1, Ordering::Relaxed) >= stop);
    let cancelled = execute(
        &source,
        payload,
        EvalLimits::pure_opaal(cancel, ResourceBudget::default()),
    );
    if matches!(&cancelled, PrimaryOutcome::Cancelled(_)) {
        assert!(polls.load(Ordering::Relaxed) > stop);
    } else {
        assert!(polls.load(Ordering::Relaxed) <= stop);
        assert_eq!(&cancelled, &baseline);
    }
});
