#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;

use opaal_runtime::ScopeStack;
use opaal_runtime::Value;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, ResourceBudget, evaluate};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::query::SemanticHover;
use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::{completed, load, run, try_load, value};

#[test]
fn construction_and_all_existing_pattern_contexts_share_expanded_semantics() {
    let expected = Value::list(vec![Value::string("api"), Value::Int(8080)]);
    for text in [
        "let name = 'api'\nlet port = 8080\nlet fields = {name, port}\n[fields.name, fields.port]",
        "type Server = {name: String, port: Int}\nlet name = 'api'\nlet port = 8080\nlet server = Server {name, port}\nlet Server {name: label, port: bound_port} = server\n[label, bound_port]",
        "type Server[T] = {name: T, port: Int}\nlet name = 'api'\nlet port = 8080\nlet server: Server[String] = Server {name, port}\nlet pick = {|Server {name, port}| [name, port]}\npick(server)",
        "type Server = {name: String, port: Int}\ndef pick(Server {name, port}: Server) { return [name, port] }\nlet server = Server {name: 'api', port: 8080}\npick(server)",
        "type Server = {name: String, port: Int}\nlet server = Server {name: 'api', port: 8080}\nmut Server {name, port} = server\nport += 0\n[name, port]",
        "type Server = {name: String, port: Int}\ndef pick(server: Server) { match server { Server {name, port} if port > 0 => { return [name, port] }; Server {name, port} => { return [] } }; return [] }\nlet server = Server {name: 'api', port: 8080}\npick(server)",
        "type Server = {name: String, port: Int}\nenum Box { Some(Server) }\nlet server = Server {name: 'api', port: 8080}\nlet [Box::Some(Server {name, port})] = [Box::Some(server)]\n[name, port]",
    ] {
        assert_eq!(value(text), expected, "{text}");
    }
    let text =
        "let name = 'api'\nlet port = 8080\nlet fields = {name, port}\n[fields.name, fields.port]";
    let file = SourceFile::new(SourceId::new(82), "record.opaal", text);
    let ParseOutcome::Complete(script) = parse_opaal(&file) else {
        panic!()
    };
    assert_eq!(
        evaluate(&script, &file, &mut ScopeStack::new()).unwrap(),
        expected
    );
    assert_eq!(
        value(
            "def saved(x: Int) { let callback = {|| {x}}; return callback }\nlet make = saved(1)\nlet x = 2\nlet result = make()\nresult.x"
        ),
        Value::Int(1)
    );
}

#[test]
fn field_and_binding_validation_and_failure_publication_are_unchanged() {
    for text in [
        "let x = {missing}",
        "type Server = {name: String}\nlet name = 1\nlet x = Server {name}",
        "type Server = {name: String}\nlet other = 'x'\nlet x = Server {other}",
        "type Server = {name: String, port: Int}\nlet name = 'x'\nlet x = Server {name}",
        "type Server = {name: String}\nlet name = 'x'\nlet x = Server {name, name}",
        "type Server = {name: String, port: Int}\nlet x = Server {name: 'x', port: 1}\nlet Server {name, port: name} = x",
        "type Server = {name: String}\ndef pick(Server {}: Server) { return 1 }",
    ] {
        assert!(try_load(text).is_err(), "invalid source accepted: {text}");
    }
    assert_eq!(
        value(
            "type Server = {name: String}\ndef wrong() -> Any { return 7 }\nlet name = wrong()\nmut result = 9\ntry { let server = Server {name}; result = 0 } catch failure { null }\nresult"
        ),
        Value::Int(9)
    );
    assert_eq!(
        value(
            "type Server = {name: Int}\nlet name = 1\nlet server = Server {name}\ndef pick(server: Server) { let Server {name} = server; return name }\npick(server)"
        ),
        Value::Int(1)
    );
    let duplicate = run(
        &load("let name = 1\nlet fields = {name, name: 2}"),
        ResourceBudget::default(),
        CancellationToken::never(),
    );
    assert!(
        matches!(duplicate.primary(), PrimaryOutcome::Error(error) if error.render().contains("duplicate record key"))
    );
    assert_eq!(
        value(
            "type Item = {_: Int, value: Int}\nlet item = Item {_: 7, value: 1}\nlet Item {_, value} = item\nvalue"
        ),
        Value::Int(1)
    );
    for parameter in ["Item {value}", "Item {value: value}"] {
        assert!(try_load(&format!("type Item = {{value: Int}}\naction pick({parameter}: Item) -> Int effects {{}} {{ return value }}")).is_ok());
    }
}

#[test]
fn queries_prioritize_the_lexical_reference_and_fresh_pattern_binding() {
    let program = load(
        "# 界🙂\ntype Server = {name: Int}\nlet name = 7\nlet server = Server {name}\ndef pick(server: Server) { let Server {name} = server; return name }",
    );
    let module = program.graph().root();
    let file = program.sources().source(module).unwrap();
    let text = file.text();
    let registry = standard_registry();
    let queries = program.semantic_queries(&registry);
    let read = text.find("Server {name}").unwrap() + "Server {".len();
    let binding = text.rfind("Server {name}").unwrap() + "Server {".len();
    let original = queries.definition_at(module, read).unwrap();
    assert_eq!(original.span().start(), text.find("name = 7").unwrap());
    let fresh = queries.definition_at(module, binding).unwrap();
    assert_eq!(fresh.span().start(), binding);
    assert_eq!(
        queries
            .definition_at(module, text.rfind("name").unwrap())
            .unwrap(),
        fresh
    );
    assert_eq!(queries.references_to(&original, false).len(), 1);
    assert_eq!(queries.references_to(&fresh, false).len(), 1);
    for offset in [read, binding] {
        assert!(matches!(
            queries.hover_at(module, offset),
            Some(SemanticHover::Binding(_))
        ));
    }
}

#[test]
fn shorthand_and_expanded_fields_charge_identical_runtime_budgets() {
    let short = load(
        "type Item = {name: Int}\nlet name = 7\nlet item = Item {name}\nlet pick = {|Item {name}| {name}}\npick(item)",
    );
    let explicit = load(
        "type Item = {name: Int}\nlet name = 7\nlet item = Item {name: name}\nlet pick = {|Item {name: name}| {name: name}}\npick(item)",
    );
    for budget in (0..50)
        .map(ResourceBudget::steps)
        .chain((0..40).map(|n| ResourceBudget::default().with_collection_items(n)))
        .chain((0..150).map(|n| ResourceBudget::default().with_collection_bytes(n)))
    {
        let a = run(&short, budget, CancellationToken::never());
        let b = run(&explicit, budget, CancellationToken::never());
        assert_eq!(
            matches!(a.primary(), PrimaryOutcome::Completed(_)),
            matches!(b.primary(), PrimaryOutcome::Completed(_)),
            "{a:?}\n{b:?}"
        );
        if matches!(a.primary(), PrimaryOutcome::Completed(_)) {
            assert_eq!(completed(&a), completed(&b));
        }
    }
    for limit in 0..60 {
        let token = || {
            let polls = Arc::new(AtomicUsize::new(0));
            CancellationToken::from_fn(move || polls.fetch_add(1, Ordering::SeqCst) >= limit)
        };
        let a = run(&short, ResourceBudget::default(), token());
        let b = run(&explicit, ResourceBudget::default(), token());
        assert_eq!(
            matches!(a.primary(), PrimaryOutcome::Cancelled(_)),
            matches!(b.primary(), PrimaryOutcome::Cancelled(_)),
            "poll limit {limit}: {a:?}\n{b:?}"
        );
    }
}
