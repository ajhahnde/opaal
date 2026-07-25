#![forbid(unsafe_code)]

use flashshell_runtime::{BindingMutability, ScopeError, ScopeStack, Value};

#[test]
fn declarations_reject_same_scope_duplicates_and_enforce_mutability() {
    let mut scopes = ScopeStack::new();
    scopes
        .declare("name", BindingMutability::Immutable, Value::string("first"))
        .unwrap();
    assert_eq!(scopes.get("name"), Some(&Value::string("first")));

    assert_eq!(
        scopes
            .declare("name", BindingMutability::Mutable, Value::string("second"))
            .unwrap_err(),
        ScopeError::DuplicateBinding("name".to_owned())
    );
    assert_eq!(
        scopes.assign("name", Value::string("changed")).unwrap_err(),
        ScopeError::ImmutableBinding("name".to_owned())
    );
    assert_eq!(scopes.get("name"), Some(&Value::string("first")));

    scopes
        .declare("count", BindingMutability::Mutable, Value::Int(1))
        .unwrap();
    scopes.assign("count", Value::Int(2)).unwrap();
    assert_eq!(scopes.get("count"), Some(&Value::Int(2)));
}

#[test]
fn child_scopes_shadow_and_assignment_selects_the_nearest_binding() {
    let mut scopes = ScopeStack::new();
    scopes
        .declare("count", BindingMutability::Mutable, Value::Int(1))
        .unwrap();
    scopes.push();
    assert_eq!(scopes.depth(), 2);
    assert_eq!(scopes.get("count"), Some(&Value::Int(1)));

    scopes
        .declare("count", BindingMutability::Immutable, Value::Int(10))
        .unwrap();
    assert_eq!(scopes.get("count"), Some(&Value::Int(10)));
    assert_eq!(
        scopes.assign("count", Value::Int(11)).unwrap_err(),
        ScopeError::ImmutableBinding("count".to_owned())
    );

    scopes.pop().unwrap();
    assert_eq!(scopes.depth(), 1);
    assert_eq!(scopes.get("count"), Some(&Value::Int(1)));
    scopes.assign("count", Value::Int(2)).unwrap();
    assert_eq!(scopes.get("count"), Some(&Value::Int(2)));
}

#[test]
fn unknown_assignment_and_root_scope_pop_are_errors_without_side_effects() {
    let mut scopes = ScopeStack::new();
    assert_eq!(
        scopes.assign("missing", Value::Null).unwrap_err(),
        ScopeError::UnknownBinding("missing".to_owned())
    );
    assert_eq!(scopes.get("missing"), None);
    assert_eq!(scopes.pop().unwrap_err(), ScopeError::CannotPopRoot);
    assert_eq!(scopes.depth(), 1);
}

#[test]
fn cloned_scope_stacks_are_independent_value_snapshots() {
    let mut scopes = ScopeStack::new();
    scopes
        .declare("value", BindingMutability::Mutable, Value::string("before"))
        .unwrap();
    scopes.push();
    scopes
        .declare(
            "local",
            BindingMutability::Mutable,
            Value::list(vec![Value::Int(1)]),
        )
        .unwrap();

    let snapshot = scopes.clone();
    scopes.assign("value", Value::string("after")).unwrap();
    scopes
        .assign("local", Value::list(vec![Value::Int(2)]))
        .unwrap();

    assert_eq!(snapshot.get("value"), Some(&Value::string("before")));
    assert_eq!(
        snapshot.get("local"),
        Some(&Value::list(vec![Value::Int(1)]))
    );
    assert_eq!(scopes.get("value"), Some(&Value::string("after")));
    assert_eq!(scopes.get("local"), Some(&Value::list(vec![Value::Int(2)])));
}
