#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::project::{
    MAX_PROJECT_ENTRIES, MAX_PROJECT_INPUTS, check_project, load_project_program,
    parse_authority_document, parse_project_manifest, parse_tool_lock,
};

struct MemorySources(BTreeMap<PathBuf, Vec<u8>>);

impl ModuleCanonicalizer for MemorySources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for MemorySources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.0
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("missing source"))
    }
}

const MANIFEST: &str = r#"
schema_version = 1

[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"

[paths]
root = "."
evidence = "target/evidence.json"

[tools.git]
adapter = "git"
version = ">=2.39.0,<3.0.0"

[endpoints.ready]
url = "http://127.0.0.1:43119/ready"
methods = ["GET"]
secret_headers = []
tls = false

[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#;

const AUTHORITY: &str = r#"
schema_version = 1
project = "demo"
environment = "ci"

[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"

[[rules]]
decision = "grant"
effect = "process.run"
scope = "tool.git"
required_enforcement = "acknowledge-unenforced"
"#;

const LOCK: &str = r#"
schema_version = 1
project = "demo"
environment = "ci"
platform = "aarch64-apple-darwin"

[child_environment]
inherit = []

[[child_environment.variables]]
name = "PATH"
value = { encoding = "base64url-nopad", platform = "unix", value = "L2Jpbg" }

[[tools]]
id = "git"
adapter = "git"
path = { encoding = "base64url-nopad", platform = "unix", value = "L3Vzci9iaW4vZ2l0" }
version = "2.50.0"
digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
"#;

#[test]
fn explicit_project_task_and_documents_bind_without_execution() {
    let manifest_path = Path::new("/project/opaal.toml");
    let manifest = parse_project_manifest(manifest_path, MANIFEST.as_bytes()).unwrap();
    let source = br#"import project::context as project
import project::tools as tools

action ready(candidate: String) -> String
effects {
    filesystem.read(project::root);
    process.run(tools::git);
}
{
    return candidate
}

task release = ready
"#;
    let sources = MemorySources(BTreeMap::from([(
        PathBuf::from("/project/tasks.opaal"),
        source.to_vec(),
    )]));
    let project = load_project_program(manifest.clone(), &sources, &sources).unwrap();
    let authority = parse_authority_document(&manifest, "ci", AUTHORITY.as_bytes()).unwrap();
    let tools = parse_tool_lock(&manifest, "ci", LOCK.as_bytes()).unwrap();
    let checked = check_project(
        &project,
        "release",
        "ci",
        &authority,
        &tools,
        [("candidate".to_owned(), "artifact.tar".to_owned())],
    )
    .unwrap();

    assert_eq!(checked.task().id().qualified_name(), "demo::release");
    assert_eq!(checked.task().action().id().name(), "ready");
    assert_eq!(checked.task().effects().len(), 2);
    assert_eq!(manifest.id().name(), "demo");
    assert_eq!(
        manifest.tools()["git"].id().qualified_name(),
        "demo::tool::git"
    );
    assert_eq!(
        manifest.environments()["ci"].id().qualified_name(),
        "demo::environment::ci"
    );
    assert_eq!(checked.downstream().project(), Some(manifest.id()));
    assert_eq!(checked.downstream().task(), Some(checked.task().id()));
    assert_eq!(
        checked.downstream().environment(),
        Some(manifest.environments()["ci"].id())
    );
    assert_eq!(
        checked.downstream().action(),
        Some(checked.task().action().id())
    );
    assert_eq!(manifest.id().manifest_path(), manifest_path);
    assert_eq!(manifest.id().root(), Path::new("/project"));
    assert!(manifest.id().manifest_digest().starts_with("sha256:"));
    assert_eq!(manifest.id().manifest_digest().len(), 71);
}

#[test]
fn project_identity_includes_the_exact_manifest_context() {
    let first =
        parse_project_manifest(Path::new("/first/opaal.toml"), MANIFEST.as_bytes()).unwrap();
    let second =
        parse_project_manifest(Path::new("/second/opaal.toml"), MANIFEST.as_bytes()).unwrap();
    let changed_manifest = MANIFEST.replace("name = \"demo\"", "name = \"other\"");
    let changed =
        parse_project_manifest(Path::new("/first/opaal.toml"), changed_manifest.as_bytes())
            .unwrap();

    assert_ne!(first.id(), second.id());
    assert_ne!(first.id(), changed.id());
    assert_eq!(first.id().name(), second.id().name());
}

#[test]
fn corrupt_future_and_ambient_documents_refuse() {
    let manifest_path = Path::new("/project/opaal.toml");
    let future = MANIFEST.replacen("schema_version = 1", "schema_version = 2", 1);
    assert_eq!(
        parse_project_manifest(manifest_path, future.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT001"
    );

    let manifest = parse_project_manifest(manifest_path, MANIFEST.as_bytes()).unwrap();
    let ambient = LOCK.replace("inherit = []", "inherit = [\"PATH\"]");
    assert_eq!(
        parse_tool_lock(&manifest, "ci", ambient.as_bytes())
            .unwrap_err()
            .code(),
        "LOCK003"
    );

    let duplicate = format!(
        "{AUTHORITY}\n[[rules]]\ndecision = \"deny\"\neffect = \"filesystem.read\"\nscope = \"project.root\"\n"
    );
    assert_eq!(
        parse_authority_document(&manifest, "ci", duplicate.as_bytes())
            .unwrap_err()
            .code(),
        "AUTH005"
    );
}

#[test]
fn hostile_schema_boundaries_and_native_encodings_refuse() {
    let manifest_path = Path::new("/project/opaal.toml");

    let false_loopback = MANIFEST.replace("http://127.0.0.1:43119", "http://127.example:43119");
    assert_eq!(
        parse_project_manifest(manifest_path, false_loopback.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT017"
    );

    let invalid_percent = MANIFEST.replace("/ready", "/%ZZ");
    assert_eq!(
        parse_project_manifest(manifest_path, invalid_percent.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT017"
    );

    let encoded_control = MANIFEST.replace("/ready", "/%0A");
    assert_eq!(
        parse_project_manifest(manifest_path, encoded_control.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT017"
    );

    let invalid_method = MANIFEST.replace("methods = [\"GET\"]", "methods = [\"-\"]");
    assert_eq!(
        parse_project_manifest(manifest_path, invalid_method.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT014"
    );

    let invalid_tls = MANIFEST
        .replace("http://127.0.0.1:43119/ready", "https://EXAMPLE.com/ready")
        .replace(
            "tls = false",
            "tls = true\ntls_server_name = \"EXAMPLE.com\"\nca = \"ca.pem\"",
        );
    assert_eq!(
        parse_project_manifest(manifest_path, invalid_tls.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT016"
    );

    let mut too_deep = String::from("schema_version = 1\n[");
    too_deep.push_str(&vec!["nested"; 16].join("."));
    too_deep.push_str("]\nvalue = 1\n");
    assert_eq!(
        parse_project_manifest(manifest_path, too_deep.as_bytes())
            .unwrap_err()
            .code(),
        "PROJECT026"
    );

    let manifest = parse_project_manifest(manifest_path, MANIFEST.as_bytes()).unwrap();
    let noncanonical_native_bytes = LOCK.replacen("L2Jpbg", "L2Jpbh", 1);
    assert_eq!(
        parse_tool_lock(&manifest, "ci", noncanonical_native_bytes.as_bytes())
            .unwrap_err()
            .code(),
        "LOCK018"
    );

    let nul_environment = LOCK.replacen("L2Jpbg", "AA", 1);
    assert_eq!(
        parse_tool_lock(&manifest, "ci", nul_environment.as_bytes())
            .unwrap_err()
            .code(),
        "LOCK019"
    );

    let relative_tool = LOCK.replace(
        "platform = \"unix\", value = \"L3Vzci9iaW4vZ2l0\"",
        "platform = \"unix\", value = \"Z2l0\"",
    );
    assert_eq!(
        parse_tool_lock(&manifest, "ci", relative_tool.as_bytes())
            .unwrap_err()
            .code(),
        "LOCK020"
    );

    let mut oversized_lock = LOCK.to_owned();
    for index in 0..MAX_PROJECT_ENTRIES {
        oversized_lock.push_str(&format!(
            "\n[[tools]]\nid = \"extra{index}\"\nadapter = \"git\"\npath = {{ encoding = \"base64url-nopad\", platform = \"unix\", value = \"L2Jpbg\" }}\nversion = \"2.50.0\"\ndigest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n"
        ));
    }
    assert_eq!(
        parse_tool_lock(&manifest, "ci", oversized_lock.as_bytes())
            .unwrap_err()
            .code(),
        "LOCK005"
    );
}

#[test]
fn task_inputs_are_bounded_and_checked_against_the_declared_type() {
    let manifest_path = Path::new("/project/opaal.toml");
    let manifest = parse_project_manifest(manifest_path, MANIFEST.as_bytes()).unwrap();
    let source = br#"action ready(candidate: Int) -> Int effects {} { return candidate }
task release = ready
"#;
    let sources = MemorySources(BTreeMap::from([(
        PathBuf::from("/project/tasks.opaal"),
        source.to_vec(),
    )]));
    let project = load_project_program(manifest.clone(), &sources, &sources).unwrap();
    let authority = parse_authority_document(&manifest, "ci", AUTHORITY.as_bytes()).unwrap();
    let tools = parse_tool_lock(&manifest, "ci", LOCK.as_bytes()).unwrap();

    assert_eq!(
        check_project(
            &project,
            "release",
            "ci",
            &authority,
            &tools,
            [("candidate".to_owned(), "not-an-int".to_owned())],
        )
        .unwrap_err()
        .code(),
        "CHECK008"
    );
    check_project(
        &project,
        "release",
        "ci",
        &authority,
        &tools,
        [("candidate".to_owned(), "42".to_owned())],
    )
    .unwrap();

    let too_many = (0..=MAX_PROJECT_INPUTS).map(|index| (format!("input{index}"), String::new()));
    assert_eq!(
        check_project(&project, "release", "ci", &authority, &tools, too_many,)
            .unwrap_err()
            .code(),
        "CHECK007"
    );
}

#[test]
fn tasks_refuse_parameter_types_without_an_exact_input_spelling() {
    let manifest =
        parse_project_manifest(Path::new("/project/opaal.toml"), MANIFEST.as_bytes()).unwrap();
    let source = br#"action ready(candidate: List[String]) -> String effects {} { return "ready" }
task release = ready
"#;
    let sources = MemorySources(BTreeMap::from([(
        PathBuf::from("/project/tasks.opaal"),
        source.to_vec(),
    )]));

    let error = load_project_program(manifest, &sources, &sources).unwrap_err();
    let opaal_runtime::project::ProjectProgramError::Contract(error) = error else {
        panic!("expected a task contract error");
    };
    assert_eq!(error.code(), "TASK005");
}

#[test]
fn effect_closure_uses_project_identity_instead_of_local_alias_spelling() {
    let manifest_path = Path::new("/project/opaal.toml");
    let manifest = parse_project_manifest(manifest_path, MANIFEST.as_bytes()).unwrap();
    let root = br#"import './child.opaal' as child
import project::context as project
action ready(candidate: String) -> String
effects {
    filesystem.read(project::root);
}
{
    return child::read(candidate)
}
task release = ready
"#;
    let child = br#"import project::context as context
export { read }
action read(candidate: String) -> String
effects {
    filesystem.read(context::root);
}
{
    return candidate
}
"#;
    let sources = MemorySources(BTreeMap::from([
        (PathBuf::from("/project/tasks.opaal"), root.to_vec()),
        (PathBuf::from("/project/child.opaal"), child.to_vec()),
    ]));
    let project = load_project_program(manifest, &sources, &sources).unwrap();
    assert_eq!(project.task("release").unwrap().effects().len(), 1);
}

#[test]
fn project_identities_require_explicit_imports_and_never_become_values() {
    let manifest =
        parse_project_manifest(Path::new("/project/opaal.toml"), MANIFEST.as_bytes()).unwrap();

    let raw = br#"action ready() -> String
effects { filesystem.read(project::context::root); }
{ return "ready" }
task release = ready
"#;
    let sources = MemorySources(BTreeMap::from([(
        PathBuf::from("/project/tasks.opaal"),
        raw.to_vec(),
    )]));
    let error = load_project_program(manifest.clone(), &sources, &sources).unwrap_err();
    let opaal_runtime::project::ProjectProgramError::Contract(error) = error else {
        panic!("expected an explicit-import contract error");
    };
    assert_eq!(error.code(), "ACT012");

    let value = br#"import project::context as project
let saved = project::root
action ready() -> String effects {} { return "ready" }
task release = ready
"#;
    let sources = MemorySources(BTreeMap::from([(
        PathBuf::from("/project/tasks.opaal"),
        value.to_vec(),
    )]));
    let error = load_project_program(manifest, &sources, &sources).unwrap_err();
    let opaal_runtime::project::ProjectProgramError::Contract(error) = error else {
        panic!("expected a project-value contract error");
    };
    assert_eq!(error.code(), "PROJECT027");
}
