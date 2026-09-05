#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use flash_runtime::builtin::standard_registry;
use flash_runtime::help::{ModuleHelpCatalog, ModuleHelpKind};
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModuleOrigin, ModulePathError, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use flash_runtime::query::SemanticHover;
use flash_syntax::LanguageMajor;

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
    let modules = FakeModules::default()
        .maps("/project/main.fsh", "/project/main.fsh")
        .maps("/project/facade.fsh", "/project/facade.fsh")
        .maps("/project/model.fsh", "/canonical/model.fsh")
        .maps("/project/model-alias.fsh", "/canonical/model.fsh")
        .contains(
            "/project/main.fsh",
            concat!(
                "language 2\n\n",
                "import './facade.fsh' as api\n",
                "import std::value as values\n",
                "export { api, values }\n",
            ),
        )
        .contains(
            "/project/facade.fsh",
            concat!(
                "language 2\n\n",
                "import './model.fsh' as model\n",
                "import './model-alias.fsh' as alternate\n",
                "export { model, alternate }\n",
            ),
        )
        .contains(
            "/canonical/model.fsh",
            concat!(
                "language 2\n\n",
                "type Item = {\n",
                "    value: Int,\n",
                "}\n",
                "\n",
                "export { Item }\n",
            ),
        );

    let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(Path::new("/project/main.fsh"))
        .expect("the canonical v2 module program loads without execution");
    let root = program.graph().root();
    let api = program.aliases().alias(root, "api").unwrap();
    assert_eq!(api.requested(), Some(Path::new("./facade.fsh")));
    assert_eq!(api.target().path(), Path::new("/project/facade.fsh"));
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
    assert_eq!(values.target().language(), LanguageMajor::V2);
    assert_eq!(
        modules.loads.borrow().as_slice(),
        [
            PathBuf::from("/project/main.fsh"),
            PathBuf::from("/project/facade.fsh"),
            PathBuf::from("/canonical/model.fsh"),
        ],
        "compiled standard descriptors are never source-loaded"
    );

    let model = program
        .aliases()
        .resolve(root, &["api", "model"])
        .expect("the explicit re-export traverses to the canonical model module");
    assert_eq!(model.path(), Path::new("/canonical/model.fsh"));
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
    assert_eq!(alias_hover.requested(), Some(Path::new("./facade.fsh")));

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
