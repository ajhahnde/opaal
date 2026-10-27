#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opaal_runtime::builtin::standard_registry;
use opaal_runtime::help::{ModuleHelpCatalog, ModuleHelpKind};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModuleOrigin, ModulePathError, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::query::SemanticHover;

#[derive(Default)]
struct FakeModules {
    canonical: BTreeMap<PathBuf, PathBuf>,
    sources: BTreeMap<PathBuf, Vec<u8>>,
    loads: RefCell<Vec<PathBuf>>,
}

impl FakeModules {
    fn maps(mut self, requested: &str, canonical: &str) -> Self {
        self.canonical
            .insert(PathBuf::from(requested), PathBuf::from(canonical));
        self
    }

    fn contains(mut self, path: &str, source: &str) -> Self {
        self.sources
            .insert(PathBuf::from(path), source.as_bytes().to_vec());
        self
    }
}

impl ModuleCanonicalizer for FakeModules {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.canonical
            .get(candidate)
            .cloned()
            .ok_or_else(|| ModulePathError::new("unmapped canonical path"))
    }
}

impl ModuleSourceLoader for FakeModules {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.loads.borrow_mut().push(module.path().to_path_buf());
        self.sources
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("unmapped source"))
    }
}

#[test]
fn aliases_reexports_and_nominal_types_keep_one_identity_and_provenance() {
    for grouped in [false, true] {
        let modules = FakeModules::default()
        .maps("/project/main.opaal", "/project/main.opaal")
        .maps("/project/facade.opaal", "/project/facade.opaal")
        .maps("/project/model.opaal", "/canonical/model.opaal")
        .maps("/project/model-alias.opaal", "/canonical/model.opaal")
        .contains(
            "/project/main.opaal",
            if grouped { "import {'./facade.opaal' as api, std::value as values}\nexport {api, values}\n" } else { concat!(
                "\n",
                "import './facade.opaal' as api\n",
                "import std::value as values\n",
                "export { api, values }\n",
            ) },
        )
        .contains(
            "/project/facade.opaal",
            if grouped { "import {'./model.opaal', './model-alias.opaal' as alternate}\nexport {model, alternate}\n" } else { concat!(
                "\n",
                "import './model.opaal' as model\n",
                "import './model-alias.opaal' as alternate\n",
                "export { model, alternate }\n",
            ) },
        )
        .contains(
            "/canonical/model.opaal",
            concat!(
                "\n",
                "type Item = {\n",
                "    value: Int,\n",
                "}\n",
                "\n",
                "export { Item }\n",
            ),
        );

        let program = ModuleProgramLoader::new(&modules, &modules)
            .load(Path::new("/project/main.opaal"))
            .expect("the canonical opaal module program loads without execution");
        let root = program.graph().root();
        let api = program.aliases().alias(root, "api").unwrap();
        assert_eq!(api.requested(), Some(Path::new("./facade.opaal")));
        assert_eq!(api.target().path(), Path::new("/project/facade.opaal"));
        assert_eq!(api.target().origin(), &ModuleOrigin::Local);
        assert_eq!(
            program.aliases().export(root, "api").unwrap().target(),
            api.target()
        );

        let values = program.aliases().alias(root, "values").unwrap();
        assert_eq!(
            values.target().origin(),
            &ModuleOrigin::Standard {
                namespace: "std".into(),
                module: "value".into(),
            }
        );
        assert_eq!(
            modules.loads.borrow().as_slice(),
            [
                PathBuf::from("/project/main.opaal"),
                PathBuf::from("/project/facade.opaal"),
                PathBuf::from("/canonical/model.opaal"),
            ],
            "compiled standard descriptors are never source-loaded"
        );

        let model = program
            .aliases()
            .resolve(root, &["api", "model"])
            .expect("the explicit re-export traverses to the canonical model module");
        assert_eq!(model.path(), Path::new("/canonical/model.opaal"));
        assert_eq!(
            program
                .aliases()
                .resolve(root, &["api", "alternate"])
                .unwrap(),
            model,
            "two local spellings collapse to one canonical module identity"
        );
        let direct = program.types().nominal(model, "Item").unwrap();
        let qualified = program
            .resolve_nominal_type(root, &["api", "model", "Item"])
            .unwrap();
        assert_eq!(qualified.id(), direct.id());
        assert_eq!(qualified.id().module(), model);
        assert_eq!(qualified.id().name(), "Item");
        assert_eq!(qualified.fields()[0].name(), "value");

        let root_source = program.sources().source(root).unwrap();
        let api_cursor = root_source.text().find("api").unwrap() + 1;
        let SemanticHover::Module(alias_hover) = program
            .semantic_queries(&standard_registry())
            .hover_at(root, api_cursor)
            .unwrap()
        else {
            panic!("module aliases use shared semantic hover metadata");
        };
        assert_eq!(alias_hover.target(), api.target());
        assert_eq!(alias_hover.requested(), Some(Path::new("./facade.opaal")));

        let model_source = program.sources().source(model).unwrap();
        let item_cursor = model_source.text().find("Item").unwrap() + 1;
        let SemanticHover::NominalType(type_hover) = program
            .semantic_queries(&standard_registry())
            .hover_at(model, item_cursor)
            .unwrap()
        else {
            panic!("nominal declarations use shared semantic hover metadata");
        };
        assert_eq!(type_hover.nominal().id(), direct.id());

        let help = ModuleHelpCatalog::snapshot(&program);
        let item_help = help.query(root, "api::model::Item").unwrap();
        assert_eq!(item_help.kind(), ModuleHelpKind::NominalType);
        assert_eq!(item_help.nominal_type().unwrap().id(), direct.id());
        let standard_help = help.query(root, "values").unwrap();
        assert_eq!(standard_help.kind(), ModuleHelpKind::Module);
        assert_eq!(standard_help.module().unwrap(), values.target());
    }
}

#[test]
fn grouped_conflicts_keep_both_locations_and_distinct_aliases_share_the_target() {
    for source in [
        "import {std::math, std::math}",
        "import std::math\nimport std::math as math",
    ] {
        let modules = FakeModules::default()
            .maps("/main.opaal", "/main.opaal")
            .contains("/main.opaal", source);
        let report = ModuleProgramLoader::new(&modules, &modules).analyze(Path::new("/main.opaal"));
        let opaal_runtime::module::ModuleProgramError::Aliases(error) = report.issues()[0].error()
        else {
            panic!("expected alias conflict")
        };
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.code(), "MOD011");
        assert_eq!(diagnostic.labels().len(), 2);
        let source_file = report
            .sources()
            .iter()
            .find_map(|source| {
                (source.module().path() == Path::new("/main.opaal")).then_some(source.source())
            })
            .unwrap();
        for label in diagnostic.labels() {
            assert_eq!(source_file.slice(label.span()).unwrap(), "math");
        }
        assert!(report.program().is_none());
    }
    let modules = FakeModules::default()
        .maps("/main.opaal", "/main.opaal")
        .contains("/main.opaal", "import {std::math, std::math as other_math}");
    let program = ModuleProgramLoader::new(&modules, &modules)
        .load(Path::new("/main.opaal"))
        .unwrap();
    let root = program.graph().root();
    assert_eq!(
        program.aliases().alias(root, "math").unwrap().target(),
        program
            .aliases()
            .alias(root, "other_math")
            .unwrap()
            .target()
    );
}

#[test]
fn selected_exports_keep_original_value_function_and_nominal_identity() {
    let modules = FakeModules::default()
        .maps("/work/main.opaal", "/work/main.opaal")
        .maps("/work/model.opaal", "/work/model.opaal")
        .maps("/work/facade.opaal", "/work/facade.opaal")
        .contains("/work/model.opaal", "type Payload = {value: Int}\ndef make(value: Int) -> Payload { return Payload {value: value} }\nlet code = 7\nexport {Payload, make, code}\n")
        .contains("/work/facade.opaal", "import './model.opaal'::{Payload, make, code}\nexport {Payload, make, code}\n")
        .contains("/work/main.opaal", "import './facade.opaal'::{Payload, make as create, code}\nlet item: Payload = create(code)\nitem.value\n");
    let program = ModuleProgramLoader::new(&modules, &modules)
        .load(Path::new("/work/main.opaal"))
        .unwrap_or_else(|error| panic!("{error:?}"));
    let root = program.graph().root();
    assert!(program.aliases().alias(root, "facade").is_none());
    assert_eq!(
        program
            .resolve_nominal_type(root, &["Payload"])
            .unwrap()
            .id()
            .module()
            .path(),
        Path::new("/work/model.opaal")
    );
    let source = program.sources().source(root).unwrap();
    let reference = program
        .names()
        .references(root)
        .iter()
        .find(|reference| source.slice(reference.reference_span()).unwrap() == "create")
        .unwrap();
    let opaal_runtime::module::ModuleReferenceTarget::Imported { target_module, .. } =
        reference.target()
    else {
        panic!("selected call must retain provenance");
    };
    assert_eq!(target_module.path(), Path::new("/work/model.opaal"));
}

#[test]
fn member_imports_reject_missing_private_duplicate_and_reserved_names() {
    for text in [
        "import './model.opaal'::{private}\n",
        "import './model.opaal'::{missing}\n",
        "import './model.opaal'::{value, value}\n",
        "import './model.opaal'::{value as status}\n",
        "import './model.opaal'::{value}\nlet value = 2\n",
        "import './model.opaal'::{value as model}\nimport './model.opaal' as model\n",
    ] {
        let modules = FakeModules::default()
            .maps("/work/main.opaal", "/work/main.opaal")
            .maps("/work/model.opaal", "/work/model.opaal")
            .contains("/work/main.opaal", text)
            .contains(
                "/work/model.opaal",
                "let value = 1\nlet private = 2\nexport {value}\n",
            );
        assert!(
            ModuleProgramLoader::new(&modules, &modules)
                .load(Path::new("/work/main.opaal"))
                .is_err(),
            "{text}"
        );
    }
}

#[test]
fn selected_module_aliases_generics_and_variant_patterns_share_defining_identities() {
    let modules = FakeModules::default()
        .maps("/work/main.opaal", "/work/main.opaal")
        .maps("/work/model.opaal", "/work/model.opaal")
        .maps("/work/facade.opaal", "/work/facade.opaal")
        .contains("/work/model.opaal", "type Box[T: Equal] = {value: T}\nenum Maybe[T: Equal] {Some(T), None}\ndef unwrap[T: Equal](item: Box[T]) -> T {return item.value}\nexport {Box, Maybe, unwrap}\n")
        .contains("/work/facade.opaal", "import './model.opaal' as original\nimport './model.opaal'::{Box, Maybe, unwrap}\nexport {original, Box, Maybe, unwrap}\n")
        .contains("/work/main.opaal", "import './facade.opaal'::{original as data, Box as Payload, Maybe as Optional, unwrap as extract}\nexport {test_value}\ndef test_value() -> Bool {\nlet item: Payload[Int] = data::Box {value: 7}\nlet selected: Optional[Int] = Optional::Some(extract(item))\nmatch selected { Optional::Some(value) => {return value == 7}\nOptional::None => {return false} }\n}\n");
    let program = ModuleProgramLoader::new(&modules, &modules)
        .load(Path::new("/work/main.opaal"))
        .unwrap_or_else(|error| panic!("{error:?}"));
    let (owner, signature) = program.exported_function("test_value").unwrap();
    let result = opaal_runtime::script::invoke_pure_function(
        &program,
        &owner,
        signature,
        opaal_runtime::eval::CancellationToken::never(),
        &mut opaal_runtime::eval::ResourceBudget::opaal(),
    )
    .unwrap();
    assert!(matches!(
        result,
        opaal_runtime::eval::Completion::Value(opaal_runtime::Value::Bool(true))
    ));
    assert_eq!(
        program
            .aliases()
            .alias(program.graph().root(), "data")
            .unwrap()
            .target()
            .path(),
        Path::new("/work/model.opaal")
    );
}

#[test]
fn selected_operations_share_pipeline_dispatch_and_editor_metadata() {
    let modules = FakeModules::default().maps("/work/main.opaal", "/work/main.opaal")
        .contains("/work/main.opaal", "import std::value::{length as count_items}\nexport {test_value}\ndef count() -> Int { [1, 2, 3] | count_items }\ndef test_value() -> Bool { return count() == 3 }\n");
    let program = ModuleProgramLoader::new(&modules, &modules)
        .load_for_frontend_with_commands(Path::new("/work/main.opaal"), &standard_registry())
        .unwrap();
    let (owner, signature) = program.exported_function("test_value").unwrap();
    let result = opaal_runtime::script::invoke_pure_function(
        &program,
        &owner,
        signature,
        opaal_runtime::eval::CancellationToken::never(),
        &mut opaal_runtime::eval::ResourceBudget::opaal(),
    )
    .unwrap();
    assert!(matches!(
        result,
        opaal_runtime::eval::Completion::Value(opaal_runtime::Value::Bool(true))
    ));
    let registry = standard_registry();
    let queries = program.semantic_queries(&registry);
    let source = program.sources().source(&owner).unwrap();
    let cursor = source.text().rfind("count_items").unwrap();
    assert!(matches!(
        queries.hover_at(&owner, cursor),
        Some(SemanticHover::Operation(_))
    ));
    let names = queries.visible_names(&owner, source.len()).unwrap();
    assert!(names.iter().any(|name| name.name() == "count_items"
        && name.kind() == opaal_runtime::query::NameKind::ImportedFunction));
}

#[test]
fn qualified_and_selected_facade_calls_keep_original_effects_and_navigation() {
    for (import, call) in [
        ("import './facade.opaal' as api", "api::selected()"),
        ("import './facade.opaal'::{selected}", "selected()"),
    ] {
        let text = format!(
            "{import}\nexport {{test_value}}\ndef test_value() -> Bool {{return {call} == 5}}\n"
        );
        let modules = FakeModules::default()
            .maps("/work/main.opaal", "/work/main.opaal")
            .maps("/work/facade.opaal", "/work/facade.opaal")
            .maps("/work/model.opaal", "/work/model.opaal")
            .contains("/work/main.opaal", &text)
            .contains(
                "/work/facade.opaal",
                "import './model.opaal'::{original as selected}\nexport {selected}\n",
            )
            .contains(
                "/work/model.opaal",
                "def original() -> Int {return 5}\nexport {original}\n",
            );
        let program = ModuleProgramLoader::new(&modules, &modules)
            .load(Path::new("/work/main.opaal"))
            .unwrap();
        let (owner, signature) = program.exported_function("test_value").unwrap();
        assert!(
            program
                .function_effects(
                    &owner,
                    signature,
                    &opaal_runtime::module::AnalysisControl::never()
                )
                .occurrences()
                .is_empty(),
            "{import}"
        );
        let registry = standard_registry();
        let definition = program
            .semantic_queries(&registry)
            .definition_at(&owner, text.rfind(call).unwrap())
            .unwrap();
        assert_eq!(definition.module().path(), Path::new("/work/model.opaal"));
        assert_eq!(
            program
                .sources()
                .source(definition.module())
                .unwrap()
                .slice(definition.span())
                .unwrap(),
            "original"
        );
        assert!(matches!(
            opaal_runtime::script::invoke_pure_function(
                &program,
                &owner,
                signature,
                opaal_runtime::eval::CancellationToken::never(),
                &mut opaal_runtime::eval::ResourceBudget::opaal(),
            )
            .unwrap(),
            opaal_runtime::eval::Completion::Value(opaal_runtime::Value::Bool(true))
        ));
    }
}

#[test]
fn prefix_exports_are_source_local_whole_file_and_keep_concrete_navigation() {
    let model = concat!(
        "export { check*, Item, Choice }\n",
        "import std::math as check_math\nimport std::math::{abs as check_abs}\n",
        "type Item = { value: Int }\nenum Choice { Yes(Int), No }\n",
        "let [check_left, [check_right]] = [2, [3]]\nlet check = 0\n",
        "mut check_mut = 1\n",
        "def check_sum() -> Int { return check_left + check_right }\n",
        "action check_action() -> Int effects {} { return 5 }\n",
        "def hidden() -> Int { let check_nested = 9; return check_nested }\n",
    );
    let modules = FakeModules::default()
        .maps("/main.opaal", "/main.opaal").maps("/model.opaal", "/model.opaal")
        .maps("/facade.opaal", "/facade.opaal")
        .contains("/model.opaal", model)
        .contains("/facade.opaal", "import './model.opaal'::{check_sum, Item, Choice}\nexport {check_sum, Item, Choice}\n")
        .contains("/main.opaal", "import './model.opaal' as model\nimport './facade.opaal'::{check_sum as sum, Item, Choice}\nlet item: Item = model::Item {value: sum()}\nlet choice: Choice = Choice::Yes(item.value)\n");
    let program = ModuleProgramLoader::new(&modules, &modules)
        .load_for_frontend(Path::new("/main.opaal"))
        .unwrap_or_else(|error| panic!("{}", error.render()));
    let owner = program
        .aliases()
        .alias(program.graph().root(), "model")
        .unwrap()
        .target()
        .clone();
    let exports = program
        .names()
        .exports(&owner)
        .map(|export| export.name())
        .collect::<Vec<_>>();
    assert_eq!(
        exports,
        [
            "Choice",
            "Item",
            "check",
            "check_action",
            "check_left",
            "check_mut",
            "check_right",
            "check_sum"
        ]
    );
    let commands = standard_registry();
    let queries = program.semantic_queries(&commands);
    for offset in model.find("check*").unwrap()..model.find("check*").unwrap() + 6 {
        assert!(queries.hover_at(&owner, offset).is_none());
        assert!(queries.definition_at(&owner, offset).is_none());
    }
    let definition = queries
        .definition_at(&owner, model.rfind("check_left").unwrap())
        .unwrap();
    let references = queries.references_to(&definition, false);
    assert_eq!(references.len(), 1);
    assert_eq!(
        references[0].span().start(),
        model.rfind("check_left").unwrap()
    );
    assert!(
        program
            .names()
            .export(&owner, "check_sum")
            .unwrap()
            .is_prefix()
    );
    assert!(!program.names().export(&owner, "Item").unwrap().is_prefix());
}

#[test]
fn prefix_no_match_and_overlap_diagnostics_cover_both_original_selectors() {
    for (source, code, primary, secondary) in [
        (
            "export { absent_* }\nlet present = 1\n",
            "MOD005",
            "absent_*",
            None,
        ),
        (
            "import std::math as check_math\nimport std::math::{abs as check_abs}\nexport {check_*}\n",
            "MOD005",
            "check_*",
            None,
        ),
        (
            "export {test_*, test_one}\ndef test_one() -> Bool { return true }\n",
            "MOD006",
            "test_one",
            Some("test_*"),
        ),
        (
            "export {test_one}\nexport {test_*}\ndef test_one() -> Bool { return true }\n",
            "MOD006",
            "test_*",
            Some("test_one"),
        ),
        (
            "export {test*, test_*}\ndef test_one() -> Bool { return true }\n",
            "MOD006",
            "test_*",
            Some("test*"),
        ),
        (
            "import std::math as math\nexport { math, math }\n",
            "MOD006",
            "math",
            Some("math"),
        ),
        (
            "export {Test*}\nlet test_one = 1\n",
            "MOD005",
            "Test*",
            None,
        ),
    ] {
        let modules = FakeModules::default()
            .maps("/main.opaal", "/main.opaal")
            .contains("/main.opaal", source);
        let error = ModuleProgramLoader::new(&modules, &modules)
            .load(Path::new("/main.opaal"))
            .unwrap_err();
        let diagnostic = error.diagnostics().remove(0);
        let rendered = format!("{diagnostic:?}");
        assert_eq!(
            &source[diagnostic.labels()[0].span().start()..diagnostic.labels()[0].span().end()],
            primary
        );
        if let Some(first) = secondary {
            assert_eq!(
                &source[diagnostic.labels()[1].span().start()..diagnostic.labels()[1].span().end()],
                first
            );
        }
        assert_eq!(diagnostic.code(), code, "{rendered}");
        if code == "MOD005" {
            assert!(rendered.contains("matches no local top-level declaration"));
        }
    }
    let modules = FakeModules::default().maps("/main.opaal", "/main.opaal")
        .contains("/main.opaal", "import std::math as check_math\nimport std::math::{abs as check_abs}\nexport { check_*, check_math, check_abs }\nlet check_value = 5\n");
    assert!(
        ModuleProgramLoader::new(&modules, &modules)
            .load(Path::new("/main.opaal"))
            .is_ok()
    );
}
