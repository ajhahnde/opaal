#![forbid(unsafe_code)]

#[path = "support/docker.rs"]
mod docker_cases;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use opaal_platform::FakePlatform;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::session::{Session, SubmitError, SubmitOutcome};
use opaal_runtime::{BindingMutability, Environment, ScopeStack, Value};
use serde_json::{Value as Json, json};

struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("Docker decoding must not resolve executables")
    }
}

fn session(input: Value) -> Session {
    let mut scope = ScopeStack::new();
    scope
        .declare("input", BindingMutability::Immutable, input)
        .unwrap();
    Session::with_scope(
        scope,
        "/project",
        Environment::new(),
        SessionOptions::default(),
    )
}

fn evaluate(input: &[u8], body: &str) -> Value {
    let mut session = session(Value::bytes(input.to_vec()));
    session
        .submit_with_value(
            "image.opaal",
            format!("import std::docker as docker\nimport std::data as data\n{body}"),
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_or_else(|error| panic!("{body}: {error:?}"))
        .1
}

struct Source(String);
impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        assert_eq!(path, Path::new("/project/main.opaal"));
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        assert_eq!(module.path(), Path::new("/project/main.opaal"));
        Ok(self.0.as_bytes().to_vec())
    }
}

#[test]
fn checker_rejects_wrong_decoder_calls_and_target_relations() {
    for body in [
        "docker::decode_image()",
        "docker::decode_image(1)",
        "docker::decode_image[Int]({})",
        "def identity[T: Equal](x: T) -> T { x }\nidentity(docker::TargetIdentity {})",
        "import std::list as list\nlist::sort([docker::TargetIdentity {}])",
    ] {
        let source = Source(format!("import std::docker as docker\n{body}"));
        assert!(
            ModuleProgramLoader::new(&source, &source)
                .load(Path::new("/project/main.opaal"))
                .is_err(),
            "{body}"
        );
    }
}

#[test]
fn decoder_shares_work_retention_and_cancellation_limits() {
    let input = evaluate(
        &serde_json::to_vec(&document()).unwrap(),
        "data::json_decode(input)",
    );
    let source = Source(
        "import std::docker as docker\ndocker::decode_image(input)\ndocker::decode_image(input)"
            .into(),
    );
    for budget in [
        ResourceBudget::steps(1),
        ResourceBudget::unlimited().with_collection_bytes(1),
        ResourceBudget::unlimited().with_collection_items(1),
    ] {
        let result = session(input.clone()).submit_with_source_loader_and_limits(
            "limits.opaal",
            &source.0,
            &source,
            &source,
            &EvalLimits::pure_opaal(CancellationToken::never(), budget),
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        );
        assert!(
            matches!(result, Err(SubmitError::Runtime { rendered, .. }) if rendered.contains("resource budget"))
        );
    }
    let cancel = CancellationToken::from_fn(|| true);
    let result = session(input)
        .submit_with_source_loader_and_limits(
            "cancel.opaal",
            &source.0,
            &source,
            &source,
            &EvalLimits::pure_opaal(cancel, ResourceBudget::unlimited()),
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap();
    assert!(matches!(result.0, SubmitOutcome::Cancelled(_)));
}

fn document() -> Json {
    let id = format!("sha256:{}", "a".repeat(64));
    json!({
        "Id": id,
        "Descriptor": {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": id},
        "Os": "linux", "Architecture": "amd64",
        "Config": {"Labels": {"org.opencontainers.image.revision": "café\n\u{1b}[31m"}},
        "RepoDigests": [format!("example.test/team/image@sha256:{}", "b".repeat(64))],
        "Unknown": {"credentials": "unreturned-canary"},
    })
}

fn decoded(input: &Json) -> Value {
    evaluate(
        &serde_json::to_vec(input).unwrap(),
        "docker::decode_image(data::json_decode(input))",
    )
}

fn rejected(input: &[u8]) {
    let Value::String(message) = evaluate(
        input,
        "mut message = ''\ntry { docker::decode_image(data::json_decode(input)) } catch error { message = error.message }\nmessage",
    ) else {
        panic!("expected error message")
    };
    assert!(!message.is_empty(), "input must refuse");
    assert!(
        !message.contains("unreturned-canary"),
        "metadata leaked: {message}"
    );
    assert!(
        !message.contains("credential-canary"),
        "metadata leaked: {message}"
    );
}

#[test]
fn named_decoder_corpus_preserves_every_selected_field_or_returns_only_an_error() {
    let cases = docker_cases::cases();
    let Value::NominalRecord(baseline) = decoded(&document()) else {
        panic!("baseline requires Image")
    };
    let mut names = std::collections::BTreeSet::new();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert!(names.insert(name.to_owned()), "duplicate case: {name}");
        let input = docker_cases::payload(&case);
        let result = evaluate(
            &input,
            "try { docker::decode_image(data::json_decode(input)) } catch error { error.message }",
        );
        if case["expect"] == "error" {
            let Value::String(message) = result else {
                panic!("{name}: invalid document admitted an Image")
            };
            assert!(!message.is_empty(), "{name}: missing failure");
            // The general JSON parser reports duplicate keys; the saved reference sanitizes them.
            if case["hex"].is_null() {
                for canary in [
                    "credential-canary",
                    "unknown-label-canary",
                    "raw-metadata-canary",
                ] {
                    assert!(
                        !message.contains(canary),
                        "{name}: diagnostic leaked metadata"
                    );
                }
            }
            continue;
        }
        assert_eq!(case["expect"], "image", "{name}");
        let Value::NominalRecord(image) = result else {
            panic!("{name}: valid document did not return an Image: {result:?}")
        };
        let document: Json = serde_json::from_slice(&input).unwrap();
        assert_eq!(image.id(), baseline.id(), "{name}");
        assert_eq!(image.fields().len(), 6, "{name}: extra metadata returned");
        for (field, key) in [("id", "Id"), ("os", "Os"), ("architecture", "Architecture")] {
            assert_eq!(
                image.get(field),
                Some(&Value::string(document[key].as_str().unwrap())),
                "{name}: {field}"
            );
        }
        assert_eq!(
            image.get("variant"),
            Some(&Value::string(document["Variant"].as_str().unwrap_or(""))),
            "{name}"
        );
        let Some(Value::Variant(revision)) = image.get("revision") else {
            panic!("{name}: revision is not an Option")
        };
        let selected = document["Config"]["Labels"]["org.opencontainers.image.revision"].as_str();
        assert_eq!(
            revision.constructor(),
            if selected.is_some() { "Some" } else { "None" },
            "{name}"
        );
        assert_eq!(
            revision.payload(),
            selected
                .map(|text| vec![Value::string(text)])
                .unwrap_or_default(),
            "{name}"
        );
        let references = document["RepoDigests"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|text| Value::string(text.as_str().unwrap()))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            image.get("repo_digests"),
            Some(&Value::list(references)),
            "{name}"
        );
    }
}

#[test]
fn single_manifest_fields_preserve_observed_identity_and_selected_data() {
    for media_type in [
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ] {
        let mut input = document();
        input["Descriptor"]["mediaType"] = json!(media_type);
        let Value::NominalRecord(image) = decoded(&input) else {
            panic!("expected Image")
        };
        assert_eq!(image.id().name(), "Image");
        assert_eq!(
            image.get("id"),
            Some(&Value::string(format!("sha256:{}", "a".repeat(64))))
        );
        assert_eq!(image.get("os"), Some(&Value::string("linux")));
        assert_eq!(image.get("architecture"), Some(&Value::string("amd64")));
        assert_eq!(image.get("variant"), Some(&Value::string("")));
        let Some(Value::Variant(revision)) = image.get("revision") else {
            panic!("expected Option")
        };
        assert_eq!(revision.constructor(), "Some");
        assert_eq!(revision.payload(), &[Value::string("café\n\u{1b}[31m")]);
        assert_eq!(image.fields().len(), 6);
        let Some(Value::List(references)) = image.get("repo_digests") else {
            panic!("expected references")
        };
        assert_eq!(
            references.as_ref(),
            &[Value::string(format!(
                "example.test/team/image@sha256:{}",
                "b".repeat(64)
            ))]
        );
    }
}

#[test]
fn missing_null_and_empty_optional_fields_have_distinct_meanings() {
    for config in [
        None,
        Some(Json::Null),
        Some(json!({})),
        Some(json!({"Labels": null})),
        Some(json!({"Labels": {}})),
        Some(json!({"Labels": {"org.opencontainers.image.revision": null}})),
    ] {
        let mut input = document();
        input.as_object_mut().unwrap().remove("Config");
        if let Some(config) = config {
            input["Config"] = config;
        }
        input["RepoDigests"] = Json::Null;
        let Value::NominalRecord(image) = decoded(&input) else {
            panic!("expected Image")
        };
        let Some(Value::Variant(revision)) = image.get("revision") else {
            panic!("expected Option")
        };
        assert_eq!(revision.constructor(), "None");
        assert!(revision.payload().is_empty());
        assert_eq!(image.get("repo_digests"), Some(&Value::list(vec![])));
    }
    let mut input = document();
    input["Config"]["Labels"]["org.opencontainers.image.revision"] = json!("");
    input["Variant"] = json!("v8");
    let Value::NominalRecord(image) = decoded(&input) else {
        panic!("expected Image")
    };
    let Some(Value::Variant(revision)) = image.get("revision") else {
        panic!("expected Option")
    };
    assert_eq!(revision.constructor(), "Some");
    assert_eq!(revision.payload(), &[Value::string("")]);
    assert_eq!(image.get("variant"), Some(&Value::string("v8")));
}

#[test]
fn hostile_selected_shapes_and_indexes_refuse_without_partial_image() {
    for key in ["Id", "Os", "Architecture", "Descriptor"] {
        let mut input = document();
        input.as_object_mut().unwrap().remove(key);
        rejected(&serde_json::to_vec(&input).unwrap());
        for value in [
            Json::Null,
            json!(false),
            json!(42),
            json!([]),
            json!("!credential-canary"),
        ] {
            let mut input = document();
            input[key] = value;
            rejected(&serde_json::to_vec(&input).unwrap());
        }
    }
    for key in ["mediaType", "digest"] {
        let mut input = document();
        input["Descriptor"].as_object_mut().unwrap().remove(key);
        rejected(&serde_json::to_vec(&input).unwrap());
        for value in [Json::Null, json!(false), json!(42), json!([])] {
            let mut input = document();
            input["Descriptor"][key] = value;
            rejected(&serde_json::to_vec(&input).unwrap());
        }
    }
    for media_type in [
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "",
        "unknown",
    ] {
        let mut input = document();
        input["Descriptor"]["mediaType"] = json!(media_type);
        rejected(&serde_json::to_vec(&input).unwrap());
    }
    let mut input = document();
    input["Descriptor"]["digest"] = json!(format!("sha256:{}", "b".repeat(64)));
    rejected(&serde_json::to_vec(&input).unwrap());
    for platform in ["", "Linux", "arm/64", "linux\n", "credential-canary:", "é"] {
        let mut input = document();
        input["Os"] = json!(platform);
        rejected(&serde_json::to_vec(&input).unwrap());
    }
    for (key, value) in [
        ("Variant", Json::Null),
        ("Config", json!([])),
        ("RepoDigests", json!([null])),
        ("RepoDigests", json!(["sha256:short"])),
    ] {
        let mut input = document();
        input[key] = value;
        rejected(&serde_json::to_vec(&input).unwrap());
    }
    for labels in [json!([]), json!({"org.opencontainers.image.revision": 42})] {
        let mut input = document();
        input["Config"]["Labels"] = labels;
        rejected(&serde_json::to_vec(&input).unwrap());
    }
    rejected(b"{\"Id\":1,\"Id\":2}");
    rejected(b"{\"credential-canary\":\"\xff\"}");
    rejected(b"{}{}");
    rejected(b"[]");
}

#[test]
fn exact_selected_limits_admit_and_first_excess_refuses() {
    let mut input = document();
    input["Config"]["Labels"]["org.opencontainers.image.revision"] = json!("é".repeat(2048));
    input["RepoDigests"] = json!(vec![format!("repo@sha256:{}", "a".repeat(64)); 256]);
    decoded(&input);
    input["RepoDigests"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("repo@sha256:{}", "a".repeat(64))));
    rejected(&serde_json::to_vec(&input).unwrap());
    input["RepoDigests"] = json!([]);
    input["Config"]["Labels"]["org.opencontainers.image.revision"] = json!("é".repeat(2048) + "a");
    rejected(&serde_json::to_vec(&input).unwrap());
    input["Config"] = Json::Null;
    input["Os"] = json!("a".repeat(506));
    decoded(&input);
    input["Os"] = json!("a".repeat(507));
    rejected(&serde_json::to_vec(&input).unwrap());
    input["Os"] = json!("linux");
    input["padding"] = json!("");
    let baseline = serde_json::to_vec(&input).unwrap().len();
    input["padding"] = json!("a".repeat(1_048_576 - baseline));
    decoded(&input);
    input["padding"] = json!("a".repeat(1_048_577 - baseline));
    rejected(&serde_json::to_vec(&input).unwrap());
}

#[test]
fn forged_target_is_opaque_and_cannot_be_serialized_inside_data() {
    for expression in [
        "target",
        "[target]",
        "{ nested: [target] }",
        "Wrapper { target: target }",
        "{ nested: [Wrapper { target: target }] }",
    ] {
        let Value::String(message) = evaluate(
            b"null",
            &format!(
                "type Wrapper = {{ target: docker::TargetIdentity }}\nlet target = docker::TargetIdentity {{}}\nmut message = ''\ntry {{ data::json_encode({expression}) }} catch error {{ message = error.message }}\nmessage"
            ),
        ) else {
            panic!("expected error")
        };
        assert!(message.contains("canonical JSON"));
    }
}

#[test]
fn target_equality_and_membership_refuse_through_nested_values() {
    for expression in [
        "target == target",
        "[target] != [target]",
        "target in [target]",
        "{ nested: target } == { nested: target }",
        "Wrapper { target: target } == Wrapper { target: target }",
        "[Wrapper { target: target }] == [Wrapper { target: target }]",
    ] {
        let Value::String(message) = evaluate(
            b"null",
            &format!(
                "type Wrapper = {{ target: docker::TargetIdentity }}\nlet target = docker::TargetIdentity {{}}\nmut message = ''\ntry {{ let result = {expression} }} catch error {{ message = error.message }}\nmessage"
            ),
        ) else {
            panic!("expected error")
        };
        assert!(
            message.contains("no equality operation"),
            "{expression}: {message}"
        );
    }
}
