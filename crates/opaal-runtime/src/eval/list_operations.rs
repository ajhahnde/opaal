//! List callbacks use the ordinary evaluator with a pure host and live budget.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use opaal_syntax::Span;

use super::{
    CallableValue, Eval, EvaluationPolicy, Evaluator, PureEvaluationHost, RuntimeArgument,
    RuntimeErrorKind,
};
use crate::module::{CallableKind, NominalTypeId, ValueType, substitute_type};
use crate::operation::{CallbackShape, OperationDescriptor, OperationError, StandardOperation};
use crate::{Callable, Value};

struct PreparedCallback {
    callable: Arc<dyn Callable>,
    function: CallableValue,
    type_arguments: Vec<ValueType>,
    result: ValueType,
}

impl Evaluator<'_, '_> {
    /// Interactive submissions have no whole-module signature analysis. Use
    /// complete homogeneous evidence and callback annotations before iteration.
    /// Any and empty collections never introduce a type argument.
    pub(super) fn infer_operation_arguments(
        &mut self,
        descriptor: &OperationDescriptor,
        values: &[Value],
        result_types: &[Option<ValueType>],
        expected_result: Option<&ValueType>,
        span: Span,
    ) -> Eval<Vec<ValueType>> {
        use crate::module::unify_type;
        let overload = descriptor.inference_overload().expect("value overload");
        let mut types = BTreeMap::new();
        if let Some(expected) = expected_result {
            unify_type(overload.result(), expected, &mut types);
        }
        for ((value, result_type), parameter) in
            values.iter().zip(result_types).zip(overload.parameters())
        {
            if let Some(relation) = parameter.callback() {
                let (_, shape) = self.callback_shape(value, descriptor, span)?;
                shape
                    .infer_operation(relation, &mut types)
                    .map_err(|message| self.invalid_callback(descriptor, message, span))?;
            } else {
                let crate::operation::OperationInputType::Value(expected) = parameter.input()
                else {
                    unreachable!("value input")
                };
                let declared = result_type.clone();
                let actual = if let Some(declared) = declared {
                    Some(declared)
                } else {
                    self.complete_value_type(value, span)?
                };
                if let Some(actual) = actual
                    && !unify_type(expected, &actual, &mut types)
                {
                    return Err(self.invalid_callback(
                        descriptor,
                        format!("type evidence `{actual}` conflicts with `{expected}`"),
                        span,
                    ));
                }
            }
        }
        descriptor
            .type_parameters()
            .iter()
            .map(|name| {
                types.get(name).cloned().ok_or_else(|| {
                    self.error(
                        RuntimeErrorKind::GenericInstantiation {
                            message: format!(
                                "cannot infer type argument `{name}`; provide it explicitly"
                            ),
                        },
                        span,
                    )
                })
            })
            .collect()
    }

    pub(super) fn complete_value_type(
        &mut self,
        value: &Value,
        span: Span,
    ) -> Eval<Option<ValueType>> {
        self.check_cancel(span)?;
        self.charge(span)?;
        if let Value::List(values) = value {
            if !self.budget.enter_call() {
                return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
            }
            let result = (|| {
                let mut element = None;
                for value in values.iter() {
                    let Some(actual) = self.complete_value_type(value, span)? else {
                        return Ok(None);
                    };
                    if element.as_ref().is_some_and(|previous| previous != &actual) {
                        return Ok(None);
                    }
                    element = Some(actual);
                }
                Ok(element.map(|element| ValueType::List(Box::new(element))))
            })();
            self.budget.leave_call();
            result
        } else {
            Ok(super::runtime_value_type(value))
        }
    }

    #[cfg(test)]
    pub(super) fn test_callback_shape(
        &self,
        value: &Value,
        descriptor: &OperationDescriptor,
        span: Span,
    ) -> Eval<(CallableValue, CallbackShape)> {
        self.callback_shape(value, descriptor, span)
    }

    fn callback_shape(
        &self,
        value: &Value,
        descriptor: &OperationDescriptor,
        span: Span,
    ) -> Eval<(CallableValue, CallbackShape)> {
        let Value::Callable(callable) = value else {
            return Err(self.invalid_callback(
                descriptor,
                format!(
                    "expected function or closure, found {}",
                    value.family_name()
                ),
                span,
            ));
        };
        let Some(function) = callable.as_any().downcast_ref::<CallableValue>() else {
            return Err(self.invalid_callback(
                descriptor,
                "callback has no language binding metadata".to_owned(),
                span,
            ));
        };
        let mut function = function.clone();
        let mut captured = function.captured_type_arguments.clone();
        for parameter in &function.family.type_parameters {
            captured.remove(parameter.name());
        }
        for parameter in &mut function.parameters {
            parameter.value_type = substitute_type(&parameter.value_type, &captured);
        }
        function.result_type = function
            .result_type
            .as_ref()
            .map(|result| substitute_type(result, &captured));
        let shape = CallbackShape {
            action: function
                .binding_types
                .function_signature(function.family.source.id(), function.family.origin_span)
                .is_some_and(|signature| signature.kind() == CallableKind::Action),
            generics: function.family.type_parameters.clone(),
            parameters: function
                .parameters
                .iter()
                .map(|parameter| parameter.value_type.clone())
                .collect(),
            result: function.result_type.clone().unwrap_or(ValueType::Any),
        };
        Ok((function, shape))
    }

    fn invalid_callback(
        &self,
        descriptor: &OperationDescriptor,
        message: String,
        span: Span,
    ) -> super::Abort {
        self.operation(
            OperationError::InvalidArgument {
                operation: descriptor.id().qualified_name(),
                message,
            },
            span,
        )
    }

    fn prepare_callback(
        &self,
        descriptor: &OperationDescriptor,
        value: &Value,
        substitutions: &BTreeMap<String, ValueType>,
        span: Span,
    ) -> Eval<PreparedCallback> {
        let Value::Callable(callable) = value else {
            return Err(self.invalid_callback(
                descriptor,
                format!(
                    "expected function or closure, found {}",
                    value.family_name()
                ),
                span,
            ));
        };
        let (function, shape) = self.callback_shape(value, descriptor, span)?;
        let relation = descriptor
            .inference_overload()
            .expect("value overload")
            .parameters()
            .last()
            .and_then(|parameter| parameter.callback())
            .expect("callback relation");
        let type_arguments = shape
            .instantiate(relation, substitutions, |actual, constraint| {
                function
                    .binding_types
                    .type_satisfies_constraint(actual, constraint)
            })
            .map_err(|message| self.invalid_callback(descriptor, message, span))?;
        Ok(PreparedCallback {
            callable: Arc::clone(callable),
            function,
            type_arguments,
            result: substitute_type(relation.result(), substitutions),
        })
    }

    fn invoke_callback(
        &mut self,
        callback: &PreparedCallback,
        arguments: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        self.check_cancel(span)?;
        let mut host = PureEvaluationHost {
            environment: self.host.environment(),
            policy: EvaluationPolicy::PureOpaal,
        };
        let mut evaluator = Evaluator {
            source: Arc::clone(&self.source),
            binding_types: Arc::clone(&self.binding_types),
            current_result_type: None,
            current_type_arguments: self.current_type_arguments.clone(),
            budgeted_callback: true,
            cancel: self.cancel.clone(),
            budget: self.budget,
            host: &mut host,
        };
        let arguments = arguments
            .into_iter()
            .map(|value| RuntimeArgument { value, span })
            .collect();
        evaluator.run_call(
            &callback.callable,
            &callback.function,
            arguments,
            span,
            Some(callback.type_arguments.clone()),
            Some(&callback.result),
        )
    }

    /// Validate nested List types with bounded work and recursion before invocation/retention.
    pub(super) fn validate_operation_value(
        &mut self,
        expected: &ValueType,
        value: &Value,
        span: Span,
    ) -> Eval<bool> {
        if let (ValueType::List(element), Value::List(values)) = (expected, value) {
            if !self.budget.enter_call() {
                return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
            }
            let result = (|| {
                for value in values.iter() {
                    self.check_cancel(span)?;
                    self.charge(span)?;
                    if !self.validate_operation_value(element, value, span)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            })();
            self.budget.leave_call();
            result
        } else {
            Ok(expected.accepts(value))
        }
    }

    pub(super) fn list_operation(
        &mut self,
        descriptor: &OperationDescriptor,
        arguments: &[Value],
        substitutions: &BTreeMap<String, ValueType>,
        span: Span,
    ) -> Eval<Value> {
        self.check_cancel(span)?;
        if descriptor
            .type_parameters()
            .iter()
            .any(|name| !substitutions.contains_key(name))
        {
            return Err(self.error(
                RuntimeErrorKind::GenericInstantiation {
                    message: "list operations require complete type evidence before iteration"
                        .to_owned(),
                },
                span,
            ));
        }
        for name in descriptor.type_parameters() {
            for constraint in descriptor.type_parameter_constraints(name) {
                if !self
                    .binding_types
                    .type_satisfies_constraint(&substitutions[name], *constraint)
                {
                    return Err(self.invalid_callback(
                        descriptor,
                        format!(
                            "type `{}` fails Ordered constraint for `{name}`",
                            substitutions[name]
                        ),
                        span,
                    ));
                }
            }
        }
        let callback = if descriptor
            .inference_overload()
            .expect("value overload")
            .parameters()
            .last()
            .is_some_and(|parameter| parameter.callback().is_some())
        {
            Some(self.prepare_callback(
                descriptor,
                arguments.last().expect("callback"),
                substitutions,
                span,
            )?)
        } else {
            None
        };
        let Value::List(items) = &arguments[0] else {
            unreachable!("validated list carrier")
        };
        let element = &substitutions["T"];
        let operation = descriptor.implementation();
        if matches!(
            operation,
            StandardOperation::Take
                | StandardOperation::Drop
                | StandardOperation::Reverse
                | StandardOperation::Sort
        ) {
            let count = if matches!(operation, StandardOperation::Take | StandardOperation::Drop) {
                let Value::Int(count) = arguments[1] else {
                    unreachable!("validated count")
                };
                if count < 0 {
                    return Err(self.invalid_callback(
                        descriptor,
                        "count must be nonnegative".to_owned(),
                        span,
                    ));
                }
                usize::try_from(count)
                    .unwrap_or(usize::MAX)
                    .min(items.len())
            } else {
                items.len()
            };
            // Validate the entire carrier, including omitted items and singletons.
            for item in items.iter() {
                self.check_list_item(descriptor, element, item, span)?;
            }
            if operation == StandardOperation::Sort {
                return self.sorted_list(items, items, span);
            }
            let selected = match operation {
                StandardOperation::Take => &items[..count],
                StandardOperation::Drop => &items[count..],
                _ => items.as_ref(),
            };
            let mut output = Vec::new();
            for index in 0..selected.len() {
                self.check_cancel(span)?;
                self.charge(span)?;
                let item = if operation == StandardOperation::Reverse {
                    &selected[selected.len() - 1 - index]
                } else {
                    &selected[index]
                };
                self.reserve_list_item(&mut output, selected.len(), span)?;
                output.push(item.clone());
            }
            return Ok(Value::list(output));
        }
        let callback = callback.expect("callback operation");
        let folding = operation == StandardOperation::Fold;
        let mut accumulator = if folding {
            arguments[1].clone()
        } else {
            Value::Null
        };
        if folding && !self.validate_operation_value(&substitutions["A"], &accumulator, span)? {
            return Err(self.invalid_callback(
                descriptor,
                "initial value does not fit accumulator type".to_owned(),
                span,
            ));
        }
        let mut output = Vec::new();
        let mut count = 0_i64;
        for item in items.iter() {
            self.check_list_item(descriptor, element, item, span)?;
            let values = if folding {
                vec![accumulator.clone(), item.clone()]
            } else {
                vec![item.clone()]
            };
            let result = self.invoke_callback(&callback, values, span)?;
            match operation {
                StandardOperation::Any
                | StandardOperation::All
                | StandardOperation::Count
                | StandardOperation::Find => {
                    let Value::Bool(matched) = result else {
                        unreachable!("validated predicate result")
                    };
                    if operation == StandardOperation::Any && matched {
                        return Ok(Value::Bool(true));
                    }
                    if operation == StandardOperation::All && !matched {
                        return Ok(Value::Bool(false));
                    }
                    if operation == StandardOperation::Find && matched {
                        return self.list_option(element, Some(item), span);
                    }
                    if operation == StandardOperation::Count && matched {
                        count = count.checked_add(1).ok_or_else(|| {
                            self.invalid_callback(
                                descriptor,
                                "match count exceeds Int".to_owned(),
                                span,
                            )
                        })?;
                    }
                }
                StandardOperation::Map | StandardOperation::SortBy => {
                    self.reserve_list_item(&mut output, items.len(), span)?;
                    output.push(result);
                }
                StandardOperation::Filter => {
                    let Value::Bool(matched) = result else {
                        unreachable!("validated predicate result")
                    };
                    if matched {
                        self.reserve_list_item(&mut output, items.len(), span)?;
                        output.push(item.clone());
                    }
                }
                StandardOperation::Fold => accumulator = result,
                _ => unreachable!("callback list operation"),
            }
        }
        match operation {
            StandardOperation::Fold => Ok(accumulator),
            StandardOperation::Any => Ok(Value::Bool(false)),
            StandardOperation::All => Ok(Value::Bool(true)),
            StandardOperation::Count => Ok(Value::Int(count)),
            StandardOperation::Find => self.list_option(element, None, span),
            StandardOperation::SortBy => self.sorted_list(items, &output, span),
            _ => Ok(Value::list(output)),
        }
    }

    fn check_list_item(
        &mut self,
        descriptor: &OperationDescriptor,
        element: &ValueType,
        item: &Value,
        span: Span,
    ) -> Eval<()> {
        self.check_cancel(span)?;
        self.charge(span)?;
        if !self.validate_operation_value(element, item, span)? {
            return Err(self.invalid_callback(
                descriptor,
                "list item does not fit its type argument".to_owned(),
                span,
            ));
        }
        Ok(())
    }

    fn reserve_list_item(&mut self, output: &mut Vec<Value>, limit: usize, span: Span) -> Eval<()> {
        self.check_cancel(span)?;
        if !self.budget.charge_collection_items(1) {
            return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
        }
        if output.len() == output.capacity() {
            let capacity = output.capacity().saturating_mul(2).max(1).min(limit);
            let additional = capacity - output.capacity();
            let bytes = additional
                .checked_mul(size_of::<Value>())
                .ok_or_else(|| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
            self.retain_collection_bytes(bytes, span)?;
            output
                .try_reserve_exact(additional)
                .map_err(|_| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
        }
        Ok(())
    }

    fn list_option(
        &mut self,
        element: &ValueType,
        item: Option<&Value>,
        span: Span,
    ) -> Eval<Value> {
        self.check_cancel(span)?;
        let payload = if let Some(item) = item {
            if !self.budget.charge_collection_items(1) {
                return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
            }
            self.retain_collection_bytes(size_of::<Value>(), span)?;
            vec![item.clone()]
        } else {
            vec![]
        };
        Ok(Value::Variant(Box::new(crate::value::VariantValue::new(
            NominalTypeId::standard("outcome", "Option"),
            vec![element.clone()],
            if item.is_some() { "Some" } else { "None" },
            payload,
        ))))
    }

    /// Stable bottom-up merge sort of indices. All keys are validated before
    /// sorting; callback evaluation never occurs inside a comparison.
    fn sorted_list(&mut self, items: &[Value], keys: &[Value], span: Span) -> Eval<Value> {
        let length = items.len();
        let mut indices = Vec::new();
        let mut workspace = Vec::new();
        if length > 1 {
            let bytes = length
                .checked_mul(size_of::<usize>())
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or_else(|| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
            self.retain_collection_bytes(bytes, span)?;
            indices
                .try_reserve_exact(length)
                .map_err(|_| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
            workspace
                .try_reserve_exact(length)
                .map_err(|_| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
            for index in 0..length {
                self.check_cancel(span)?;
                self.charge(span)?;
                indices.push(index);
                workspace.push(0);
            }
            let mut width = 1;
            while width < length {
                let mut start = 0;
                while start < length {
                    let middle = start.saturating_add(width).min(length);
                    let end = middle.saturating_add(width).min(length);
                    let (mut left, mut right) = (start, middle);
                    for slot in &mut workspace[start..end] {
                        self.check_cancel(span)?;
                        self.charge(span)?;
                        if left < middle
                            && (right == end
                                || self.compare_list_keys(
                                    &keys[indices[left]],
                                    &keys[indices[right]],
                                    span,
                                )? != Ordering::Greater)
                        {
                            *slot = indices[left];
                            left += 1;
                        } else {
                            *slot = indices[right];
                            right += 1;
                        }
                    }
                    start = end;
                }
                std::mem::swap(&mut indices, &mut workspace);
                width = width.saturating_mul(2);
            }
        }
        let mut output = Vec::new();
        for index in 0..length {
            self.check_cancel(span)?;
            self.charge(span)?;
            self.reserve_list_item(&mut output, length, span)?;
            output.push(items[if length > 1 { indices[index] } else { index }].clone());
        }
        Ok(Value::list(output))
    }

    fn compare_list_keys(&mut self, left: &Value, right: &Value, span: Span) -> Eval<Ordering> {
        self.check_cancel(span)?;
        self.charge(span)?;
        match (left, right) {
            (Value::String(left), Value::String(right)) => {
                self.compare_key_bytes(left.as_bytes(), right.as_bytes(), span)
            }
            (Value::Bytes(left), Value::Bytes(right)) => self.compare_key_bytes(left, right, span),
            (Value::Path(left), Value::Path(right)) => self.compare_key_bytes(
                left.as_os_str().as_encoded_bytes(),
                right.as_os_str().as_encoded_bytes(),
                span,
            ),
            (Value::List(left), Value::List(right)) => {
                if !self.budget.enter_call() {
                    return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
                }
                let result = (|| {
                    for (left, right) in left.iter().zip(right.iter()) {
                        let order = self.compare_list_keys(left, right, span)?;
                        if order != Ordering::Equal {
                            return Ok(order);
                        }
                    }
                    Ok(left.len().cmp(&right.len()))
                })();
                self.budget.leave_call();
                result
            }
            _ => crate::operation::order(left, right).map_err(|error| self.operation(error, span)),
        }
    }

    pub(super) fn compare_key_bytes(
        &mut self,
        left: &[u8],
        right: &[u8],
        span: Span,
    ) -> Eval<Ordering> {
        for (left, right) in left.chunks(4096).zip(right.chunks(4096)) {
            self.check_cancel(span)?;
            self.charge(span)?;
            let order = left.cmp(right);
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(left.len().cmp(&right.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;
    use crate::eval::{Abort, CancellationToken, ResourceBudget};
    use crate::module::{ModuleId, RuntimeBindingTypes};
    use crate::operation::standard_operation;
    use opaal_syntax::{SourceFile, SourceId};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn run(
        name: &str,
        arguments: Vec<Value>,
        element: ValueType,
        budget: &mut ResourceBudget,
        cancel: CancellationToken,
    ) -> Eval<Value> {
        let source = Arc::new(SourceFile::new(SourceId::new(1), "lists.opaal", "call"));
        let mut environment = Environment::new();
        let mut host = PureEvaluationHost {
            environment: &mut environment,
            policy: EvaluationPolicy::PureOpaal,
        };
        let mut evaluator = Evaluator {
            source,
            binding_types: Arc::new(RuntimeBindingTypes::default()),
            current_result_type: None,
            current_type_arguments: BTreeMap::new(),
            budgeted_callback: false,
            cancel,
            budget,
            host: &mut host,
        };
        let operation = standard_operation(&ModuleId::standard("std", "list"), name).unwrap();
        evaluator.execute_operation(
            &operation,
            arguments,
            &[element],
            evaluator.source.span(0..4).unwrap(),
        )
    }

    fn ints(values: &[i64]) -> Value {
        Value::list(values.iter().copied().map(Value::Int).collect())
    }

    #[test]
    fn selection_charges_slots_before_retention_and_shares_cumulative_limits() {
        for (name, arguments, slots) in [
            ("take", vec![ints(&[1, 2, 3]), Value::Int(2)], 2),
            ("drop", vec![ints(&[1, 2, 3]), Value::Int(1)], 2),
            ("reverse", vec![ints(&[1, 2])], 2),
        ] {
            let bytes = (slots * size_of::<Value>()) as u64;
            let mut budget = ResourceBudget::unlimited()
                .with_collection_items(slots as u64)
                .with_collection_bytes(bytes);
            assert!(
                run(
                    name,
                    arguments.clone(),
                    ValueType::Int,
                    &mut budget,
                    CancellationToken::never()
                )
                .is_ok()
            );
            assert_eq!(budget.collection_items(), slots as u64);
            assert_eq!(budget.collection_bytes(), bytes);
            assert!(
                matches!(run(name, arguments.clone(), ValueType::Int, &mut budget, CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
            );
            let mut short = ResourceBudget::unlimited().with_collection_bytes(bytes - 1);
            assert!(
                matches!(run(name, arguments, ValueType::Int, &mut short, CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
            );
            assert!(short.collection_bytes() < bytes);
        }
    }

    #[test]
    fn sorting_charges_index_workspace_output_and_exact_steps() {
        let arguments = vec![ints(&[3, 1, 2, 1])];
        let slots = 4;
        let bytes = (slots * (2 * size_of::<usize>() + size_of::<Value>())) as u64;
        let mut budget = ResourceBudget::steps(u64::MAX)
            .with_collection_bytes(bytes)
            .with_collection_items(4);
        assert_eq!(
            run(
                "sort",
                arguments.clone(),
                ValueType::Int,
                &mut budget,
                CancellationToken::never()
            )
            .unwrap_or_else(|_| panic!("exact limit")),
            ints(&[1, 1, 2, 3])
        );
        assert_eq!(budget.collection_bytes(), bytes);
        let steps = budget.used();
        for short in [
            ResourceBudget::steps(steps - 1),
            ResourceBudget::unlimited().with_collection_bytes(bytes - 1),
            ResourceBudget::unlimited().with_collection_items(3),
        ] {
            assert!(
                matches!(run("sort", arguments.clone(), ValueType::Int, &mut short.clone(), CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
            );
        }
        assert!(
            run(
                "sort",
                arguments,
                ValueType::Int,
                &mut ResourceBudget::steps(steps),
                CancellationToken::never()
            )
            .is_ok()
        );
    }

    #[test]
    fn ordering_matches_existing_scalar_nested_and_native_path_rules() {
        let cases = [
            (
                ValueType::Float,
                vec![
                    Value::from(crate::FiniteFloat::new(0.0).unwrap()),
                    Value::from(crate::FiniteFloat::new(-0.0).unwrap()),
                    Value::from(crate::FiniteFloat::new(-1.0).unwrap()),
                ],
            ),
            (
                ValueType::String,
                vec![Value::string("é"), Value::string("a"), Value::string("a\0")],
            ),
            (
                ValueType::Bytes,
                vec![
                    Value::bytes(vec![255]),
                    Value::bytes(vec![]),
                    Value::bytes(vec![0]),
                ],
            ),
            (
                ValueType::Path,
                vec![
                    Value::Path(std::ffi::OsString::from("a//b").into()),
                    Value::Path(std::ffi::OsString::from("a/b").into()),
                    Value::Path(std::ffi::OsString::from("a").into()),
                ],
            ),
            (
                ValueType::Duration,
                vec![
                    Value::Duration(crate::Duration::from_nanos(2)),
                    Value::Duration(crate::Duration::from_nanos(-1)),
                    Value::Duration(crate::Duration::from_nanos(0)),
                ],
            ),
            (
                ValueType::ByteSize,
                vec![
                    Value::ByteSize(crate::ByteSize::new(u64::MAX)),
                    Value::ByteSize(crate::ByteSize::new(0)),
                    Value::ByteSize(crate::ByteSize::new(1)),
                ],
            ),
            (
                ValueType::List(Box::new(ValueType::Int)),
                vec![ints(&[1, 2]), ints(&[]), ints(&[1])],
            ),
        ];
        for (element, items) in cases {
            let mut expected = items.clone();
            expected.sort_by(|left, right| crate::operation::order(left, right).unwrap());
            assert_eq!(
                run(
                    "sort",
                    vec![Value::list(items)],
                    element,
                    &mut ResourceBudget::unlimited(),
                    CancellationToken::never()
                )
                .unwrap_or_else(|_| panic!("sort must succeed")),
                Value::list(expected)
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let items = [vec![b'a', 0xff], vec![b'a', 0x80], vec![b'a']]
                .into_iter()
                .map(|bytes| Value::Path(std::ffi::OsString::from_vec(bytes).into()))
                .collect::<Vec<_>>();
            let mut expected = items.clone();
            expected.sort_by(|left, right| crate::operation::order(left, right).unwrap());
            assert_eq!(
                run(
                    "sort",
                    vec![Value::list(items)],
                    ValueType::Path,
                    &mut ResourceBudget::unlimited(),
                    CancellationToken::never()
                )
                .unwrap_or_else(|_| panic!("native paths must sort")),
                Value::list(expected)
            );
        }
        // Exercise every merge-pass boundary, many equal keys and ragged runs.
        for length in 0..100 {
            let items = (0..length)
                .map(|index| (index * 17 % 13) as i64)
                .collect::<Vec<_>>();
            let mut expected = items.clone();
            expected.sort();
            assert_eq!(
                run(
                    "sort",
                    vec![ints(&items)],
                    ValueType::Int,
                    &mut ResourceBudget::unlimited(),
                    CancellationToken::never()
                )
                .unwrap_or_else(|_| panic!("sort must succeed")),
                ints(&expected)
            );
        }
    }

    #[test]
    fn long_key_scans_cancel_inside_comparison_and_recursive_depth_is_live() {
        let input = Value::list(vec![
            Value::string("a".repeat(4096 * 40)),
            Value::string("a".repeat(4096 * 40)),
        ]);
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&polls);
        let cancel =
            CancellationToken::from_fn(move || seen.fetch_add(1, AtomicOrdering::SeqCst) >= 15);
        assert!(matches!(
            run(
                "sort",
                vec![input.clone()],
                ValueType::String,
                &mut ResourceBudget::unlimited(),
                cancel
            ),
            Err(Abort::Cancelled(_))
        ));
        assert_eq!(polls.load(AtomicOrdering::SeqCst), 16);
        // Equal long keys must scan every started chunk, rather than an
        // uncharged standard-library comparison after one cancellation poll.
        let mut baseline = ResourceBudget::steps(u64::MAX);
        assert!(
            run(
                "sort",
                vec![input],
                ValueType::String,
                &mut baseline,
                CancellationToken::never()
            )
            .is_ok()
        );
        let mut short = ResourceBudget::steps(u64::MAX);
        assert!(
            run(
                "sort",
                vec![Value::list(vec![Value::string("a"), Value::string("a")])],
                ValueType::String,
                &mut short,
                CancellationToken::never()
            )
            .is_ok()
        );
        assert_eq!(baseline.used() - short.used(), 39);
        let nested = vec![Value::list(vec![ints(&[1])]), Value::list(vec![ints(&[1])])];
        let element = ValueType::List(Box::new(ValueType::List(Box::new(ValueType::Int))));
        let mut budget = ResourceBudget::unlimited().with_call_depth(2);
        assert!(
            run(
                "sort",
                vec![Value::list(nested.clone())],
                element.clone(),
                &mut budget,
                CancellationToken::never()
            )
            .is_ok()
        );
        assert_eq!(budget.peak_call_depth(), 2);
        assert!(
            matches!(run("sort", vec![Value::list(nested)], element, &mut ResourceBudget::unlimited().with_call_depth(1), CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
        );
    }
}
