#![forbid(unsafe_code)]

//! Host-only probes for measurements that need a stable in-process boundary.

use std::env;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use flash_cli::completion::{
    CompletionCandidateProvider, CompletionEngine, CompletionSnapshotLimits,
};
use flash_runtime::builtin::standard_registry;
use flash_runtime::stream::{StreamPull, ValueStream};
use flash_runtime::{Environment, ScopeStack, Value};

fn usage() -> ExitCode {
    eprintln!(
        "usage: flash-benchmark-fixture completion WARMUPS SAMPLES | \
         structured-stream ITEMS"
    );
    ExitCode::from(2)
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
