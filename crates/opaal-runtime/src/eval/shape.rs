use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use opaal_syntax::MAX_TUPLE_DEPTH;

use super::{
    CallableValue, DEFAULT_OPAAL_EVALUATION_STEPS, RuntimeError, RuntimeErrorKind, ScopeError,
    ScopeStack, Status, Value, ValueType,
};

const MAX_RUST_VALUE_DEPTH: usize = 64;

pub(crate) enum Node<'a> {
    Value(&'a Value),
    Type(&'a ValueType),
    Error(&'a RuntimeError),
    Status(&'a Status),
    Frame(Edges<'a>),
}

type Edges<'a> = Box<dyn Iterator<Item = Node<'a>> + 'a>;

pub(crate) enum ShapeError<E> {
    Limit,
    Interrupted(E),
}

fn values(values: &[Value]) -> impl Iterator<Item = Node<'_>> {
    values.iter().map(Node::Value)
}

fn types(types: &[ValueType]) -> impl Iterator<Item = Node<'_>> {
    types.iter().map(Node::Type)
}

pub(crate) fn scope(scope: &ScopeStack) -> impl Iterator<Item = Node<'_>> {
    scope.shape_frames().flatten()
}

#[derive(Clone, Copy, Default)]
struct Path {
    depth: usize,
    tuple: bool,
}

#[derive(Clone, Copy)]
enum Walk<'a> {
    FindTuple(Option<&'a HashMap<*const (), bool>>),
    Source(Path),
    Rust,
}

/// Check source values, metering each edge only when the graph contains a tuple.
pub(crate) fn check<'a, E, I: Iterator<Item = Node<'a>> + 'a>(
    roots: impl Fn() -> I,
    step: impl FnMut(bool) -> Result<(), E>,
) -> Result<bool, ShapeError<E>> {
    check_with_callable_shapes(roots, step, None)
}

pub(crate) fn check_with_callable_shapes<'a, E, I: Iterator<Item = Node<'a>> + 'a>(
    roots: impl Fn() -> I,
    mut step: impl FnMut(bool) -> Result<(), E>,
    callable_shapes: Option<&HashMap<*const (), bool>>,
) -> Result<bool, ShapeError<E>> {
    let mut remaining = DEFAULT_OPAAL_EVALUATION_STEPS;
    if !walk(
        roots(),
        &mut step,
        Walk::FindTuple(callable_shapes),
        &mut remaining,
    )? {
        return Ok(false);
    }
    walk(roots(), step, Walk::Source(Path::default()), &mut remaining)
}

pub(crate) fn check_rust<'a, E>(
    roots: impl Iterator<Item = Node<'a>> + 'a,
    step: impl FnMut(bool) -> Result<(), E>,
) -> Result<bool, ShapeError<E>> {
    let mut remaining = DEFAULT_OPAAL_EVALUATION_STEPS;
    walk(roots, step, Walk::Rust, &mut remaining)
}

pub(super) fn check_tuple_slot<E>(
    slot: Node<'_>,
    remaining: &mut u64,
    mut step: impl FnMut() -> Result<(), E>,
) -> Result<bool, ShapeError<E>> {
    walk(
        std::iter::once(slot),
        |_| step(),
        Walk::Source(Path {
            depth: 1,
            tuple: true,
        }),
        remaining,
    )
}

/// Walk references one edge at a time so a wide child array does not grow the stack.
fn walk<'a, E>(
    roots: impl Iterator<Item = Node<'a>> + 'a,
    mut step: impl FnMut(bool) -> Result<(), E>,
    mode: Walk<'_>,
    remaining: &mut u64,
) -> Result<bool, ShapeError<E>> {
    let find_tuple = matches!(mode, Walk::FindTuple(_));
    let rust_ingress = matches!(mode, Walk::Rust);
    let initial_path = match mode {
        Walk::Source(path) => path,
        _ => Path::default(),
    };
    let mut seen_tuple = matches!(mode, Walk::Source(_));
    let mut probes = 0_u64;
    let mut visited_storage = HashSet::new();
    let mut visited_callables = HashSet::new();
    let mut pending: Vec<(Edges<'a>, Path)> = vec![(Box::new(roots), initial_path)];
    while let Some((children, path)) = pending.last_mut() {
        let Some(node) = children.next() else {
            pending.pop();
            continue;
        };
        let mut path = *path;
        let is_tuple = matches!(
            node,
            Node::Value(Value::Tuple(_)) | Node::Type(ValueType::Tuple(_))
        );
        if let Walk::FindTuple(callable_shapes) = mode {
            if is_tuple {
                return Ok(true);
            }
            if let Node::Value(Value::Callable(callable)) = &node
                && let Some(&has_tuple) = callable_shapes
                    .and_then(|shapes| shapes.get(&Arc::as_ptr(callable).cast::<()>()))
            {
                if has_tuple {
                    return Ok(true);
                }
                continue;
            }
            probes += 1;
            if probes.is_multiple_of(256) {
                step(false).map_err(ShapeError::Interrupted)?;
            }
            if shared_children(&node).is_some_and(|key| !visited_storage.insert(key)) {
                continue;
            }
            if matches!(node, Node::Value(Value::Status(_)) | Node::Status(_)) {
                continue;
            }
        } else {
            *remaining = remaining.checked_sub(1).ok_or(ShapeError::Limit)?;
            seen_tuple |= is_tuple;
            step(seen_tuple).map_err(ShapeError::Interrupted)?;
        }
        if rust_ingress && pending.len() - 1 > MAX_RUST_VALUE_DEPTH {
            return Err(ShapeError::Limit);
        }
        let construct = !matches!(node, Node::Value(Value::Callable(_)));
        let edges: Edges<'a> = match node {
            Node::Value(Value::Tuple(items)) => {
                if items.len() < 2 {
                    return Err(ShapeError::Limit);
                }
                path.tuple = true;
                Box::new(values(items))
            }
            Node::Value(Value::List(items)) => Box::new(values(items)),
            Node::Value(Value::Record(record)) => {
                Box::new(record.entries().iter().map(|(_, item)| Node::Value(item)))
            }
            Node::Value(Value::NominalRecord(record)) => Box::new(
                types(record.type_arguments())
                    .chain(record.fields().iter().map(|(_, item)| Node::Value(item))),
            ),
            Node::Value(Value::Variant(variant)) => {
                Box::new(types(variant.type_arguments()).chain(values(variant.payload())))
            }
            Node::Value(Value::Table(table)) => {
                Box::new(table.rows().iter().flat_map(|row| values(row)))
            }
            Node::Value(Value::Error(error)) => Box::new(error_edges(error)),
            Node::Error(error) => Box::new(error_edges(error)),
            Node::Value(Value::Status(status)) | Node::Status(status) => {
                Box::new(status.stages().iter().map(Node::Status))
            }
            Node::Value(Value::Callable(callable)) => {
                let cleanup_depth = if rust_ingress { pending.len() } else { 0 };
                let key = (
                    Arc::as_ptr(callable).cast::<()>(),
                    if find_tuple { 0 } else { path.depth },
                    path.tuple,
                    cleanup_depth,
                );
                if !visited_callables.insert(key) {
                    continue;
                }
                let Some(function) =
                    (callable.as_ref() as &dyn std::any::Any).downcast_ref::<CallableValue>()
                else {
                    continue;
                };
                Box::new(
                    function
                        .captured
                        .shape_frames()
                        .map(|frame| Node::Frame(Box::new(frame)))
                        .chain(function.result_type.iter().map(Node::Type))
                        .chain(
                            function
                                .parameters
                                .iter()
                                .map(|p| Node::Type(&p.value_type)),
                        )
                        .chain(function.captured_type_arguments.values().map(Node::Type)),
                )
            }
            Node::Type(ValueType::Tuple(items)) => {
                if items.len() < 2 {
                    return Err(ShapeError::Limit);
                }
                path.tuple = true;
                Box::new(types(items))
            }
            Node::Type(ValueType::List(item)) => Box::new(std::iter::once(Node::Type(item))),
            Node::Type(ValueType::Nominal { arguments, .. }) if !arguments.is_empty() => {
                Box::new(types(arguments))
            }
            Node::Frame(edges) => edges,
            Node::Value(_) | Node::Type(_) => continue,
        };
        path.depth = (path.depth + usize::from(construct)).min(MAX_TUPLE_DEPTH + 1);
        if path.tuple && path.depth > MAX_TUPLE_DEPTH {
            return Err(ShapeError::Limit);
        }
        pending.try_reserve(1).map_err(|_| ShapeError::Limit)?;
        pending.push((edges, path));
    }
    Ok(seen_tuple)
}

// Clones keep the same immutable child storage. Use it only when finding tuples,
// since admission must count each edge and check every enclosing path.
fn shared_children(node: &Node<'_>) -> Option<*const ()> {
    match node {
        Node::Value(Value::List(items)) if !items.is_empty() => Some(items.as_ptr().cast()),
        Node::Value(Value::Record(record)) if !record.entries().is_empty() => {
            Some(record.entries().as_ptr().cast())
        }
        Node::Value(Value::NominalRecord(record)) if !record.fields().is_empty() => {
            Some(record.fields().as_ptr().cast())
        }
        Node::Value(Value::NominalRecord(record)) if !record.type_arguments().is_empty() => {
            Some(record.type_arguments().as_ptr().cast())
        }
        Node::Value(Value::Variant(variant)) if !variant.payload().is_empty() => {
            Some(variant.payload().as_ptr().cast())
        }
        Node::Value(Value::Variant(variant)) if !variant.type_arguments().is_empty() => {
            Some(variant.type_arguments().as_ptr().cast())
        }
        Node::Value(Value::Table(table)) if !table.rows().is_empty() => {
            Some(table.rows().as_ptr().cast())
        }
        Node::Value(Value::Error(error)) => Some(Arc::as_ptr(error).cast()),
        Node::Error(error) => Some(std::ptr::from_ref(*error).cast()),
        _ => None,
    }
}

fn error_edges(error: &RuntimeError) -> impl Iterator<Item = Node<'_>> {
    let expected = match error.kind() {
        RuntimeErrorKind::BindingTypeMismatch { expected, .. }
        | RuntimeErrorKind::ParameterTypeMismatch { expected, .. }
        | RuntimeErrorKind::FunctionResultTypeMismatch { expected, .. }
        | RuntimeErrorKind::Scope(ScopeError::TypeMismatch { expected, .. }) => Some(expected),
        _ => None,
    };
    [
        expected.map(Node::Type),
        error.status().map(Node::Status),
        error.cause().map(Node::Error),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_tuple_subtrees_charge_each_traversed_edge_until_interrupted() {
        let mut value = Value::Tuple(Arc::from([Value::Int(0), Value::Int(1)]));
        for _ in 0..25 {
            value = Value::list(vec![value.clone(), value]);
        }
        let mut visits = 0;
        let result = check(
            || std::iter::once(Node::Value(&value)),
            |_| {
                visits += 1;
                (visits <= 100).then_some(()).ok_or("cancelled")
            },
        );
        assert!(matches!(result, Err(ShapeError::Interrupted("cancelled"))));
        assert_eq!(visits, 101);
    }
}
