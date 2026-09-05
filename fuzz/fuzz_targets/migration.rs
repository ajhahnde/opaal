#![no_main]

use std::path::{Path, PathBuf};

use flash_migrate::{
    MigrationFormat, MigrationLimits, SourceReader, analyze_roots, analyze_roots_with_limits,
};
use libfuzzer_sys::fuzz_target;

struct FuzzReader<'input> {
    bytes: &'input [u8],
}

impl SourceReader for FuzzReader<'_> {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
        Ok(path.to_owned())
    }

    fn read(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
        if path == Path::new("fuzz.fsh") {
            Ok(self.bytes[..self.bytes.len().min(max_bytes.saturating_add(1))].to_vec())
        } else {
            Err("fuzz import is unavailable".to_owned())
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let (limits, source) = fuzz_limits(data);
    let reader = FuzzReader { bytes: source };
    let roots = [PathBuf::from("fuzz.fsh")];
    let analyzed = limits.map_or_else(
        || analyze_roots(&reader, &roots),
        |limits| analyze_roots_with_limits(&reader, &roots, &limits),
    );
    let json = match analyzed {
        Ok(report) => {
            assert!(report.exit_status() <= 1);
            match limits {
                Some(limits) => report
                    .render_with_limits(MigrationFormat::Json, &limits)
                    .unwrap_or_else(|error| error.render(MigrationFormat::Json)),
                None => report
                    .render(MigrationFormat::Json)
                    .expect("default migration limits bound fuzz report rendering"),
            }
        }
        Err(error) => error.render(MigrationFormat::Json),
    };

    let decoded: serde_json::Value =
        serde_json::from_str(&json).expect("migration JSON must always be valid");
    assert_eq!(decoded["schema"], flash_migrate::SCHEMA_VERSION);
});

fn fuzz_limits(data: &[u8]) -> (Option<MigrationLimits>, &[u8]) {
    let Some((&selector, source)) = data.split_first() else {
        return (None, data);
    };
    if selector != 0 {
        return (None, data);
    }
    let mut knobs = source.iter().copied();
    let limits = MigrationLimits {
        max_files: usize::from(knobs.next().unwrap_or_default() % 4),
        max_source_bytes: usize::from(knobs.next().unwrap_or_default()),
        max_findings: usize::from(knobs.next().unwrap_or_default() % 16),
        max_edit_bytes: usize::from(knobs.next().unwrap_or_default()),
        max_output_bytes: usize::from(knobs.next().unwrap_or_default()) * 8,
        max_nesting: usize::from(knobs.next().unwrap_or_default() % 32),
        max_work_units: usize::from(knobs.next().unwrap_or_default()) * 16,
    };
    let consumed = source.len().min(7);
    (Some(limits), &source[consumed..])
}
