#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opaal_runtime::module::{
    CallableKind, ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
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

#[test]
fn action_identity_carries_signature_effects_and_a_stable_digest() {
    let path = PathBuf::from("/project/main.opaal");
    let text = b"action ready(candidate: String) -> String\neffects {\n    clock.wall;\n    clock.wall;\n}\n{\n    return candidate\n}\n".to_vec();
    let sources = MemorySources(BTreeMap::from([(path.clone(), text)]));
    let program = ModuleProgramLoader::new(&sources, &sources)
        .load(&path)
        .unwrap();
    let action = &program.actions().actions(program.graph().root())[0];

    assert_eq!(action.id().name(), "ready");
    assert_eq!(action.callable().kind(), CallableKind::Action);
    assert_eq!(action.callable().parameters()[0].name(), "candidate");
    assert_eq!(action.callable().result().to_string(), "String");
    assert_eq!(action.effects().len(), 1, "duplicate requests canonicalize");
    assert_eq!(action.effects()[0].capability(), "clock.wall");
    assert_eq!(action.id().contract_digest().len(), 64);
    assert_eq!(action.callable().downstream().action(), Some(action.id()));
    assert_eq!(action.callable().declared_effects(), action.effects());
    let shared = program
        .types()
        .function(program.graph().root(), action.callable().declaration_span())
        .unwrap();
    assert_eq!(shared, action.callable());
    assert_eq!(shared.downstream().action(), Some(action.id()));
    assert!(
        action
            .id()
            .contract_digest()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
}

fn action_diagnostic_codes(text: &str) -> Vec<String> {
    let path = PathBuf::from("/project/main.opaal");
    let sources = MemorySources(BTreeMap::from([(path.clone(), text.as_bytes().to_vec())]));
    ModuleProgramLoader::new(&sources, &sources)
        .analyze(&path)
        .issues()
        .iter()
        .flat_map(|issue| issue.error().diagnostics())
        .map(|diagnostic| diagnostic.code().to_owned())
        .collect()
}

#[test]
fn action_call_graph_is_static_acyclic_and_effect_closed() {
    let missing = action_diagnostic_codes(
        "action leaf() -> String effects { clock.wall; } { return 'ok' }\n\
         action root() -> String effects {} { return leaf() }\n",
    );
    assert!(missing.contains(&"ACT007".to_owned()), "{missing:?}");

    let function = action_diagnostic_codes(
        "action leaf() -> String effects {} { return 'ok' }\n\
         def invalid() -> String { return leaf() }\n",
    );
    assert!(function.contains(&"ACT005".to_owned()), "{function:?}");

    let cycle = action_diagnostic_codes("action first() -> String effects {} { return first() }\n");
    assert!(cycle.contains(&"ACT008".to_owned()), "{cycle:?}");

    let valid = action_diagnostic_codes(
        "action leaf() -> String effects { clock.wall; } { return 'ok' }\n\
         action root() -> String effects { clock.wall; } { return leaf() }\n",
    );
    assert!(valid.is_empty(), "{valid:?}");
}

#[test]
fn invalid_action_contracts_are_source_diagnostics() {
    let path = PathBuf::from("/project/main.opaal");
    let text = b"action broken(candidate) -> String effects { unknown.work; } {}\n".to_vec();
    let sources = MemorySources(BTreeMap::from([(path.clone(), text)]));
    let report = ModuleProgramLoader::new(&sources, &sources).analyze(&path);
    let codes = report
        .issues()
        .iter()
        .flat_map(|issue| issue.error().diagnostics())
        .map(|diagnostic| diagnostic.code().to_owned())
        .collect::<Vec<_>>();
    assert!(codes.contains(&"ACT001".to_owned()), "{codes:?}");
    assert!(codes.contains(&"ACT002".to_owned()), "{codes:?}");

    let dynamic = action_diagnostic_codes(
        "let suffix = 'wall'\naction broken() -> String effects { filesystem.read(\"$suffix\"); } { return 'done' }\n",
    );
    assert!(dynamic.contains(&"ACT011".to_owned()), "{dynamic:?}");

    let value = action_diagnostic_codes(
        "action leaf() -> String effects {} { return 'ok' }\nlet saved = $leaf\n",
    );
    assert!(value.contains(&"ACT006".to_owned()), "{value:?}");
}

#[test]
fn nested_action_reads_and_first_excess_call_depth_are_diagnosed() {
    let nested = action_diagnostic_codes(
        "action leaf() -> String effects {} { return 'ok' }\n\
         def invalid(flag: Bool) -> String {\n\
             if flag { return leaf() }\n\
             return 'done'\n\
         }\n",
    );
    assert!(nested.contains(&"ACT005".to_owned()), "{nested:?}");

    let root = PathBuf::from("/project/main.opaal");
    let library = PathBuf::from("/project/library.opaal");
    let sources = MemorySources(BTreeMap::from([
        (
            root.clone(),
            b"import './library.opaal' as lib\naction stores() -> String effects {} { let saved = lib::leaf\nreturn 'done' }\n"
                .to_vec(),
        ),
        (
            library,
            b"export { leaf }\naction leaf() -> String effects {} { return 'ok' }\n".to_vec(),
        ),
    ]));
    let imported_value = ModuleProgramLoader::new(&sources, &sources)
        .analyze(&root)
        .issues()
        .iter()
        .flat_map(|issue| issue.error().diagnostics())
        .map(|diagnostic| diagnostic.code().to_owned())
        .collect::<Vec<_>>();
    assert!(
        imported_value.contains(&"ACT006".to_owned()),
        "{imported_value:?}"
    );

    let mut deep = String::new();
    for index in (0..65).rev() {
        let body = if index == 64 {
            "return 'done'".to_owned()
        } else {
            format!("return action{}()", index + 1)
        };
        deep.push_str(&format!(
            "action action{index}() -> String effects {{}} {{ {body} }}\n"
        ));
    }
    let deep = action_diagnostic_codes(&deep);
    assert!(deep.contains(&"ACT010".to_owned()), "{deep:?}");
}
