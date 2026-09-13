#![forbid(unsafe_code)]

//! Host-only probes for measurements that need a stable in-process boundary.

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use opaal_cli::completion::{
    CompletionCandidateProvider, CompletionEngine, CompletionSnapshotLimits,
};
use opaal_cli::project::inspect_plan_artifact;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, EvalLimits, ResourceBudget, evaluate_with_limits};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use opaal_runtime::stream::{StreamPull, ValueStream};
use opaal_runtime::workflow::{
    JournalChain, MAX_JOURNAL_BYTES, MAX_JOURNAL_LINES, PlanArtifact, audit_journal, native_path,
};
use opaal_runtime::{Environment, ScopeStack, Value as OpaalValue};
use serde_json::{Value, json};

const OPERATIONAL_PLAN_BYTES: usize = 1024 * 1024;
const OPERATIONAL_PLAN_ACTIONS: usize = 1024;
const OPERATIONAL_JOURNAL_MESSAGE_BYTES: usize = 4 * 1024;
const OPERATIONAL_JOURNAL_CLEANUPS: usize = 16;

fn usage() -> ExitCode {
    eprintln!(
        "usage: opaal-benchmark-fixture completion WARMUPS SAMPLES | \
         structured-stream ITEMS | opaal-resources WARMUPS SAMPLES STATEMENTS | \
         plan-build-render WARMUPS SAMPLES OUTPUT | maximum-journal OUTPUT"
    );
    ExitCode::from(2)
}

fn benchmark_digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn benchmark_outcome(code: &str, message: impl Into<String>) -> Value {
    json!({
        "class":"success",
        "code":code,
        "message":message.into(),
        "status":null,
        "value_digest":null,
        "partial":false
    })
}

fn operational_plan_document() -> Result<Value, String> {
    let contract = benchmark_digest('5');
    let node_ids = (0..OPERATIONAL_PLAN_ACTIONS)
        .map(|ordinal| format!("{contract}#{ordinal:06}"))
        .collect::<Vec<_>>();
    let actions = node_ids
        .iter()
        .enumerate()
        .map(|(ordinal, node_id)| {
            json!({
                "id":node_id,
                "ordinal":ordinal,
                "action_id":format!("/benchmark/tasks.opaal::node_{ordinal:04}"),
                "contract_digest":contract,
                "requests":[],
                "dependencies":if ordinal == 0 {
                    node_ids.iter().skip(1).cloned().map(Value::String).collect::<Vec<_>>()
                } else {
                    Vec::new()
                },
                "outcome":benchmark_outcome("PLAN000", "action is executable")
            })
        })
        .collect::<Vec<_>>();
    let root = native_path(Path::new("/benchmark"));
    let manifest = native_path(Path::new("/benchmark/opaal.toml"));
    let authority_path = native_path(Path::new("/benchmark/authority.toml"));
    let tool_lock_path = native_path(Path::new("/benchmark/tools.toml"));
    let mut document = json!({
        "schema":"opaal.plan.v2",
        "schema_version":2,
        "created_at":"2026-09-13T08:00:00.000000000Z",
        "expires_at":"2026-09-13T08:15:00.000000000Z",
        "toolchain":{"version":"1.0.0-alpha.1"},
        "platform":{"triple":"aarch64-apple-darwin"},
        "project":{
            "name":"benchmark",
            "root":root,
            "manifest_path":manifest,
            "manifest_digest":benchmark_digest('1'),
            "environment":"ci",
            "environment_digest":benchmark_digest('2'),
            "tool_lock_digest":benchmark_digest('3'),
            "child_environment_digest":benchmark_digest('4'),
            "tls":[]
        },
        "task":{
            "id":"benchmark::node_0000",
            "action_id":"/benchmark/tasks.opaal::node_0000",
            "contract_digest":contract
        },
        "inputs":[],
        "secrets":[],
        "sources":[],
        "authority":{
            "path":authority_path,
            "digest":benchmark_digest('6'),
            "rules":[],
            "requests":[]
        },
        "tools":[],
        "observations":[
            {"kind":"manifest","id":"benchmark","path":manifest,"digest":benchmark_digest('1'),"size":0,"observed_at":null},
            {"kind":"authority","id":"ci","path":authority_path,"digest":benchmark_digest('6'),"size":0,"observed_at":null},
            {"kind":"tool-lock","id":"ci","path":tool_lock_path,"digest":benchmark_digest('3'),"size":0,"observed_at":null},
            {"kind":"child-environment","id":"ci","path":null,"digest":benchmark_digest('4'),"size":null,"observed_at":null},
            {"kind":"wall-clock","id":"created-at","path":null,"digest":null,"size":null,"observed_at":"2026-09-13T08:00:00.000000000Z"}
        ],
        "actions":actions,
        "outcome":benchmark_outcome("PLAN000", "")
    });
    let minimal = PlanArtifact::seal(document.clone()).map_err(|error| error.to_string())?;
    let padding = OPERATIONAL_PLAN_BYTES
        .checked_sub(minimal.bytes().len())
        .ok_or_else(|| "1 MiB cannot contain the 1,024-node benchmark plan".to_owned())?;
    document["outcome"]["message"] = Value::String("x".repeat(padding));
    let exact = PlanArtifact::seal(document.clone()).map_err(|error| error.to_string())?;
    if exact.bytes().len() != OPERATIONAL_PLAN_BYTES {
        return Err("operational plan generator did not reach exactly 1 MiB".to_owned());
    }
    Ok(document)
}

fn plan_build_render(warmups: usize, samples: usize, output: &Path) -> Result<(), String> {
    let document = operational_plan_document()?;
    for index in 0..warmups + samples {
        let started = Instant::now();
        let plan = PlanArtifact::seal(document.clone()).map_err(|error| error.to_string())?;
        fs::write(output, plan.bytes()).map_err(|error| error.to_string())?;
        let rendered = inspect_plan_artifact(output).map_err(|error| error.to_string())?;
        let elapsed = started.elapsed().as_nanos();
        if plan.bytes().len() != OPERATIONAL_PLAN_BYTES
            || !rendered
                .windows(b"Actions (1024)".len())
                .any(|window| window == b"Actions (1024)")
        {
            return Err("operational plan benchmark produced the wrong artifact".to_owned());
        }
        let class = if index < warmups { "warmup" } else { "sample" };
        println!("{class}_ns={elapsed}");
    }
    Ok(())
}

fn journal_header() -> Value {
    json!({
        "plan_digest":benchmark_digest('5'),
        "accepted_plan_digest":benchmark_digest('5'),
        "authority_digest":benchmark_digest('6'),
        "project_digest":benchmark_digest('7'),
        "environment_digest":benchmark_digest('2'),
        "tool_lock_digest":benchmark_digest('3'),
        "child_environment_digest":benchmark_digest('4'),
        "started_at":"2026-09-13T08:01:00.000000000Z"
    })
}

fn journal_action_start(action_node: &str) -> Value {
    json!({
        "action_node_id":action_node,
        "action_id":"/benchmark/tasks.opaal::ready",
        "contract_digest":benchmark_digest('5')
    })
}

fn journal_action_end(action_node: &str) -> Value {
    json!({
        "action_node_id":action_node,
        "outcome":benchmark_outcome(
            "ACTION000",
            "x".repeat(OPERATIONAL_JOURNAL_MESSAGE_BYTES),
        )
    })
}

fn journal_cleanup(ordinal: usize) -> (Value, Value) {
    let outcome = json!({
        "class":"cleanup-failed",
        "code":"CLEANUP001",
        "message":"x".repeat(OPERATIONAL_JOURNAL_MESSAGE_BYTES),
        "status":null,
        "value_digest":null,
        "partial":false
    });
    let payload = json!({
        "action_node_id":null,
        "resource_id":format!("resource-000000000000000b-{ordinal:06}"),
        "ordinal":ordinal,
        "outcome":outcome.clone()
    });
    (payload, outcome)
}

fn journal_terminal(message: String, cleanup: Vec<Value>) -> Value {
    json!({
        "finished_at":"2026-09-13T08:02:00.000000000Z",
        "primary":benchmark_outcome("RUN000", message),
        "cleanup":cleanup,
        "complete":true
    })
}

fn maximum_legal_journal() -> Result<(Vec<u8>, usize), String> {
    let (mut chain, first) =
        JournalChain::begin("0000000000000000000000000000000b", journal_header())
            .map_err(|error| error.to_string())?;
    let mut journal = first;
    let action_node = format!("{}#000000", benchmark_digest('5'));
    let mut action_snapshots = Vec::new();
    loop {
        let mut candidate = chain.clone();
        let start = match candidate.append("action-start", journal_action_start(&action_node)) {
            Ok(line) => line,
            Err(error) if error.code() == "JOURNAL001" => break,
            Err(error) => return Err(error.to_string()),
        };
        let end = match candidate.append("action-end", journal_action_end(&action_node)) {
            Ok(line) => line,
            Err(error) if error.code() == "JOURNAL001" => break,
            Err(error) => return Err(error.to_string()),
        };
        chain = candidate;
        journal.extend(start);
        journal.extend(end);
        action_snapshots.push((chain.clone(), journal.len()));
    }

    let mut exact = None;
    for (mut candidate, journal_len) in action_snapshots.iter().rev().take(64).cloned() {
        let mut cleanup_lines = Vec::new();
        let mut cleanup = Vec::new();
        for ordinal in (0..OPERATIONAL_JOURNAL_CLEANUPS).rev() {
            let (payload, outcome) = journal_cleanup(ordinal);
            match candidate.append("cleanup", payload) {
                Ok(line) => cleanup_lines.extend(line),
                Err(error) if error.code() == "JOURNAL001" => {
                    cleanup_lines.clear();
                    break;
                }
                Err(error) => return Err(error.to_string()),
            }
            cleanup.push(outcome);
        }
        if cleanup_lines.is_empty() {
            continue;
        }
        let mut minimum_probe = candidate.clone();
        let Ok(minimum_terminal) =
            minimum_probe.append("terminal", journal_terminal(String::new(), cleanup.clone()))
        else {
            continue;
        };
        let padding = MAX_JOURNAL_BYTES
            .checked_sub(candidate.bytes() + minimum_terminal.len())
            .ok_or_else(|| "journal terminal exceeds the byte ceiling".to_owned())?;
        if padding > OPERATIONAL_JOURNAL_MESSAGE_BYTES {
            continue;
        }
        exact = Some((candidate, journal_len, cleanup_lines, cleanup, padding));
        break;
    }
    let Some((mut chain, journal_len, cleanup_lines, cleanup, padding)) = exact else {
        return Err("producer-shaped journal cannot reach the exact byte ceiling".to_owned());
    };
    journal.truncate(journal_len);
    journal.extend(cleanup_lines);

    let mut excess_probe = chain.clone();
    let excess = excess_probe
        .append(
            "terminal",
            journal_terminal("x".repeat(padding + 1), cleanup.clone()),
        )
        .expect_err("the first byte beyond the journal ceiling must refuse");
    if excess.code() != "JOURNAL001" {
        return Err(format!("journal first excess returned {}", excess.code()));
    }
    let terminal = chain
        .append("terminal", journal_terminal("x".repeat(padding), cleanup))
        .map_err(|error| error.to_string())?;
    journal.extend(terminal);
    if journal.len() != MAX_JOURNAL_BYTES
        || chain.bytes() != MAX_JOURNAL_BYTES
        || chain.lines() >= MAX_JOURNAL_LINES
        || !chain.is_terminal()
    {
        return Err("maximum journal did not stop exactly at the byte boundary".to_owned());
    }
    let audit = audit_journal(&journal).map_err(|error| error.to_string())?;
    if !audit.is_complete() {
        return Err("maximum legal journal did not produce a complete audit".to_owned());
    }
    Ok((journal, chain.lines()))
}

fn write_maximum_journal(output: &Path) -> Result<(), String> {
    let (journal, lines) = maximum_legal_journal()?;
    fs::write(output, &journal).map_err(|error| error.to_string())?;
    println!("journal_bytes={}", journal.len());
    println!("journal_lines={lines}");
    println!("journal_byte_first_excess=JOURNAL001");
    Ok(())
}

struct BenchmarkSource {
    bytes: Vec<u8>,
}

impl ModuleCanonicalizer for BenchmarkSource {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        (candidate == Path::new("/benchmark.opaal"))
            .then(|| candidate.to_path_buf())
            .ok_or_else(|| ModulePathError::new("benchmark imports are unavailable"))
    }
}

impl ModuleSourceLoader for BenchmarkSource {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        (module.path() == Path::new("/benchmark.opaal"))
            .then(|| self.bytes[..self.bytes.len().min(maximum)].to_vec())
            .ok_or_else(|| ModuleSourceError::new("benchmark imports are unavailable"))
    }
}

fn opaal_resources(warmups: usize, samples: usize, statements: usize) -> Result<(), String> {
    let mut text = String::from("");
    for index in 0..statements {
        text.push_str(&format!("let value_{index} = [{index}, {index}]\n"));
    }
    text.push_str("[1, 2, 3, 4]\n");
    let source = BenchmarkSource {
        bytes: text.into_bytes(),
    };

    for index in 0..=warmups + samples {
        let started = Instant::now();
        let program = ModuleProgramLoader::new(&source, &source)
            .load(Path::new("/benchmark.opaal"))
            .map_err(|error| error.to_string())?;
        let root = program.graph().root();
        let script = program
            .sources()
            .script(root)
            .ok_or_else(|| "benchmark analysis retained no root syntax".to_owned())?;
        let source_file = program
            .sources()
            .source(root)
            .ok_or_else(|| "benchmark analysis retained no root source".to_owned())?;
        evaluate_with_limits(
            script,
            source_file,
            &mut ScopeStack::new(),
            &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::opaal()),
        )
        .map_err(|error| error.to_string())?;
        let elapsed = started.elapsed().as_nanos();
        let class = if index == 0 {
            "cold"
        } else if index <= warmups {
            "warmup"
        } else {
            "sample"
        };
        println!("{class}_ns={elapsed}");
    }
    Ok(())
}

fn positive_usize(value: Option<String>, name: &str) -> Result<usize, String> {
    let value = value.ok_or_else(|| format!("missing {name}"))?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{name} must be a positive integer"));
    }
    Ok(parsed)
}

fn nonnegative_usize(value: Option<String>, name: &str) -> Result<usize, String> {
    let value = value.ok_or_else(|| format!("missing {name}"))?;
    value
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a nonnegative integer"))
}

fn completion(warmups: usize, samples: usize) -> Result<(), String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    let environment = Environment::from_snapshot(
        env::vars_os()
            .filter_map(|(name, value)| name.into_string().ok().map(|name| (name, value))),
    );
    let registry = standard_registry();
    let scope = ScopeStack::new();
    let limits = CompletionSnapshotLimits::default();
    let mut provider = CompletionCandidateProvider::new(limits);

    for index in 0..=warmups + samples {
        let started = Instant::now();
        let catalog = provider
            .snapshot(&registry, &scope, &cwd, &environment, &|| false)
            .ok_or_else(|| "completion snapshot was cancelled or overflowed".to_owned())?;
        let engine = CompletionEngine::new(catalog);
        let completions = engine.complete("^ben", 4);
        let elapsed = started.elapsed().as_nanos();
        if completions.is_empty() {
            return Err("completion fixture produced no candidate".to_owned());
        }
        let class = if index == 0 {
            "cold"
        } else if index <= warmups {
            "warmup"
        } else {
            "sample"
        };
        println!("{class}_ns={elapsed}");
    }
    Ok(())
}

fn structured_stream(items: usize) -> Result<(), String> {
    println!("ready");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;

    let mut next = 0_i64;
    let mut stream = ValueStream::from_fn(move || {
        if next >= items as i64 {
            None
        } else {
            let value = next;
            next += 1;
            Some(Ok(OpaalValue::Int(value)))
        }
    });
    let mut count = 0_usize;
    let mut checksum = 0_i64;
    loop {
        match stream.pull() {
            StreamPull::Item(OpaalValue::Int(value)) => {
                count += 1;
                checksum = checksum.wrapping_add(value);
            }
            StreamPull::Item(_) => return Err("unexpected stream value".to_owned()),
            StreamPull::End => break,
            StreamPull::Failed(error) => return Err(error.to_string()),
            StreamPull::Cancelled(reason) => {
                return Err(format!("stream cancelled: {reason:?}"));
            }
        }
    }
    if count != items {
        return Err(format!("expected {items} items, observed {count}"));
    }
    println!("count={count}");
    println!("checksum={checksum}");
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("completion") => {
            let warmups = nonnegative_usize(args.next(), "warmup count")?;
            let samples = positive_usize(args.next(), "sample count")?;
            if args.next().is_some() {
                return Err("completion accepts exactly two arguments".to_owned());
            }
            completion(warmups, samples)
        }
        Some("structured-stream") => {
            let items = positive_usize(args.next(), "item count")?;
            if args.next().is_some() {
                return Err("structured-stream accepts exactly one argument".to_owned());
            }
            structured_stream(items)
        }
        Some("opaal-resources") => {
            let warmups = nonnegative_usize(args.next(), "warmup count")?;
            let samples = positive_usize(args.next(), "sample count")?;
            let statements = positive_usize(args.next(), "statement count")?;
            if args.next().is_some() {
                return Err("opaal-resources accepts exactly three arguments".to_owned());
            }
            opaal_resources(warmups, samples, statements)
        }
        Some("plan-build-render") => {
            let warmups = nonnegative_usize(args.next(), "warmup count")?;
            let samples = positive_usize(args.next(), "sample count")?;
            let output = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "missing plan output".to_owned())?;
            if args.next().is_some() {
                return Err("plan-build-render accepts exactly three arguments".to_owned());
            }
            plan_build_render(warmups, samples, &output)
        }
        Some("maximum-journal") => {
            let output = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "missing journal output".to_owned())?;
            if args.next().is_some() {
                return Err("maximum-journal accepts exactly one argument".to_owned());
            }
            write_maximum_journal(&output)
        }
        Some(_) | None => Err(String::new()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is_empty() => usage(),
        Err(error) => {
            eprintln!("opaal-benchmark-fixture: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use opaal_runtime::workflow::{MAX_JOURNAL_BYTES, MAX_JOURNAL_LINES, PlanArtifact};
    use serde_json::Value;

    #[test]
    fn opaal_resource_probe_analyzes_and_executes_its_corpus() {
        super::opaal_resources(0, 1, 1).expect("the benchmark corpus must remain executable");
    }

    #[test]
    fn operational_plan_generator_hits_the_exact_node_and_byte_boundaries() {
        let document = super::operational_plan_document().unwrap();
        let artifact = PlanArtifact::seal(document).unwrap();
        assert_eq!(artifact.bytes().len(), super::OPERATIONAL_PLAN_BYTES);
        assert_eq!(
            artifact.value()["actions"].as_array().unwrap().len(),
            super::OPERATIONAL_PLAN_ACTIONS
        );
    }

    #[test]
    fn maximum_journal_is_byte_bound_before_the_independent_line_ceiling() {
        let (journal, lines) = super::maximum_legal_journal().unwrap();
        assert_eq!(journal.len(), MAX_JOURNAL_BYTES);
        assert!(lines < MAX_JOURNAL_LINES);
        let terminal: Value = serde_json::from_slice(
            journal
                .split(|byte| *byte == b'\n')
                .rev()
                .nth(1)
                .expect("the journal ends with one terminal line"),
        )
        .unwrap();
        let payload = &terminal["payload"];
        assert!(
            payload["primary"]["message"].as_str().unwrap().len()
                <= super::OPERATIONAL_JOURNAL_MESSAGE_BYTES
        );
        let cleanup = payload["cleanup"].as_array().unwrap();
        assert_eq!(cleanup.len(), super::OPERATIONAL_JOURNAL_CLEANUPS);
        assert!(cleanup.iter().all(|outcome| {
            outcome["message"].as_str().unwrap().len() <= super::OPERATIONAL_JOURNAL_MESSAGE_BYTES
        }));
    }

    #[test]
    fn journal_line_ceiling_property_accepts_exactly_one_hundred_thousand() {
        let terminal_admitted = |existing: usize| existing < MAX_JOURNAL_LINES;
        let nonterminal_admitted = |existing: usize| existing + 1 < MAX_JOURNAL_LINES;
        assert!(nonterminal_admitted(MAX_JOURNAL_LINES - 2));
        assert!(!nonterminal_admitted(MAX_JOURNAL_LINES - 1));
        assert!(terminal_admitted(MAX_JOURNAL_LINES - 1));
        assert!(!terminal_admitted(MAX_JOURNAL_LINES));
    }
}
