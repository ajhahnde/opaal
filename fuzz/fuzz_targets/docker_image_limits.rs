#![no_main]

use libfuzzer_sys::fuzz_target;
use opaal_platform::FakePlatform;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::session::{Session, SubmitError, SubmitOutcome};
use opaal_runtime::{BindingMutability, Environment, ScopeStack, Value};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const SOURCE: &str = "import std::docker as docker\nimport std::data as data\ndocker::decode_image(data::json_decode(input))";
struct Source;
impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        assert_eq!(path, Path::new("/fuzz/image.opaal"));
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        assert_eq!(module.path(), Path::new("/fuzz/image.opaal"));
        Ok(SOURCE.as_bytes().to_vec())
    }
}
struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("saved Docker data must not resolve executables")
    }
}
fn evaluate(input: &[u8], limits: EvalLimits) -> Option<Value> {
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
        "image.opaal",
        SOURCE,
        &Source,
        &Source,
        &limits,
        &NoExecutables,
        &FakePlatform::full(),
        &FakeClock::new(),
        &mut Vec::new(),
    ) {
        Ok((SubmitOutcome::Continued, value)) => Some(value),
        Ok((SubmitOutcome::Cancelled(_), _)) | Err(SubmitError::Runtime { .. }) => None,
        other => panic!("fixed pure decoder source must analyze: {other:?}"),
    }
}

fuzz_target!(|data: &[u8]| {
    let control = data.get(..4).unwrap_or(&[0, 0, 0, 0]);
    let input = data.get(4..).unwrap_or_default();
    let generous = evaluate(
        input,
        EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
    );
    if let Some(Value::NominalRecord(image)) = &generous {
        let document: serde_json::Value = serde_json::from_slice(input).unwrap();
        assert_eq!(image.id().name(), "Image");
        assert_eq!(image.fields().len(), 6);
        assert_eq!(
            image.get("id"),
            Some(&Value::string(document["Id"].as_str().unwrap()))
        );
        assert_eq!(document["Id"], document["Descriptor"]["digest"]);
        assert!(matches!(
            document["Descriptor"]["mediaType"].as_str(),
            Some(
                "application/vnd.oci.image.manifest.v1+json"
                    | "application/vnd.docker.distribution.manifest.v2+json"
            )
        ));
    } else {
        assert!(generous.is_none());
    }
    let limited = evaluate(
        input,
        EvalLimits::pure_opaal(
            CancellationToken::never(),
            ResourceBudget::steps(u64::from(control[0]))
                .with_collection_items(u64::from(control[1]))
                .with_collection_bytes(u64::from(control[2]) * 64),
        ),
    );
    if let Some(value) = limited {
        assert_eq!(Some(value), generous);
    }
    if control[3] & 1 != 0 {
        assert!(
            evaluate(
                input,
                EvalLimits::pure_opaal(
                    CancellationToken::from_fn(|| true),
                    ResourceBudget::default()
                )
            )
            .is_none()
        );
    }
});
