#![forbid(unsafe_code)]

//! Host-only probes for measurements that need a stable in-process boundary.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use flash_cli::completion::{
    CompletionCandidateProvider, CompletionEngine, CompletionSnapshotLimits,
};
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::{CancellationToken, EvalLimits, ResourceBudget, evaluate_with_limits};
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use flash_runtime::stream::{StreamPull, ValueStream};
use flash_runtime::{Environment, ScopeStack, Value};
use flash_syntax::LanguageMajor;

fn usage() -> ExitCode {
    eprintln!(
        "usage: flash-benchmark-fixture completion WARMUPS SAMPLES | \
         structured-stream ITEMS | v2-resources WARMUPS SAMPLES STATEMENTS"
    );
    ExitCode::from(2)
}

struct BenchmarkSource {
    bytes: Vec<u8>,
}

impl ModuleCanonicalizer for BenchmarkSource {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        (candidate == Path::new("/benchmark.fsh"))
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
        (module.path() == Path::new("/benchmark.fsh"))
            .then(|| self.bytes[..self.bytes.len().min(maximum)].to_vec())
            .ok_or_else(|| ModuleSourceError::new("benchmark imports are unavailable"))
    }
}

fn v2_resources(warmups: usize, samples: usize, statements: usize) -> Result<(), String> {
    let mut text = String::from("language 2\n");
    for index in 0..statements {
        text.push_str(&format!("let value_{index} = [{index}, {index}]\n"));
    }
    text.push_str("[1, 2, 3, 4]\n");
    let source = BenchmarkSource {
        bytes: text.into_bytes(),
    };

    for index in 0..=warmups + samples {
        let started = Instant::now();
        let program = ModuleProgramLoader::for_language(&source, &source, LanguageMajor::V2)
            .load(Path::new("/benchmark.fsh"))
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
            &EvalLimits::pure_v2(CancellationToken::never(), ResourceBudget::v2()),
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

#[cfg(test)]
mod tests {
    #[test]
    fn v2_resource_probe_analyzes_and_executes_its_corpus() {
        super::v2_resources(0, 1, 1).expect("the benchmark corpus must remain executable");
    }
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
        let completions = engine.complete("ben", 3);
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
            Some(Ok(Value::Int(value)))
        }
    });
    let mut count = 0_usize;
    let mut checksum = 0_i64;
    loop {
        match stream.pull() {
            StreamPull::Item(Value::Int(value)) => {
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
        Some("v2-resources") => {
            let warmups = nonnegative_usize(args.next(), "warmup count")?;
            let samples = positive_usize(args.next(), "sample count")?;
            let statements = positive_usize(args.next(), "statement count")?;
            if args.next().is_some() {
                return Err("v2-resources accepts exactly three arguments".to_owned());
            }
            v2_resources(warmups, samples, statements)
        }
        Some(_) | None => Err(String::new()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is_empty() => usage(),
        Err(error) => {
            eprintln!("flash-benchmark-fixture: {error}");
            ExitCode::FAILURE
        }
    }
}
