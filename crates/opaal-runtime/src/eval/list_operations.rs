//! List callbacks use the ordinary evaluator with a pure host and live budget.

use std::collections::BTreeMap;
use std::sync::Arc;

use opaal_syntax::Span;

use super::{
    CallableValue, Eval, EvaluationPolicy, Evaluator, PureEvaluationHost, RuntimeArgument,
    RuntimeErrorKind,
};
use crate::module::{CallableKind, ValueType, substitute_type};
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
        call: &opaal_syntax::CallExpression,
        values: &[Value],
        scope: &crate::ScopeStack,
        expected_result: Option<&ValueType>,
        span: Span,
    ) -> Eval<Vec<ValueType>> {
        use crate::module::unify_type;
        let overload = descriptor.value_overload().expect("value overload");
        let mut types = BTreeMap::new();
        if let Some(expected) = expected_result {
            unify_type(overload.result(), expected, &mut types);
        }
        for ((expression, value), parameter) in
            call.arguments.iter().zip(values).zip(overload.parameters())
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
                let declared = match expression.kind() {
                    opaal_syntax::ExpressionKind::Name(name) => {
                        scope.declared_type(self.text(name.name.span())).cloned()
                    }
                    opaal_syntax::ExpressionKind::Call(call) => {
                        let value = match call.callee.kind() {
                            opaal_syntax::ExpressionKind::Name(name) => {
                                scope.get(self.text(name.name.span()))
                            }
                            _ => None,
                        };
                        Some(
                            value
                                .and_then(|value| {
                                    let Value::Callable(callable) = value else {
                                        return None;
                                    };
                                    callable
                                        .as_any()
                                        .downcast_ref::<CallableValue>()
                                        .and_then(|function| function.result_type.clone())
                                })
                                .unwrap_or(ValueType::Any),
                        )
                    }
                    _ => None,
                };
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

    fn complete_value_type(&mut self, value: &Value, span: Span) -> Eval<Option<ValueType>> {
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
        for parameter in &function.type_parameters {
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
                .function_signature(function.source.id(), function.origin_span)
                .is_some_and(|signature| signature.kind() == CallableKind::Action),
            generics: function.type_parameters.clone(),
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
            .value_overload()
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
        let callback = self.prepare_callback(
            descriptor,
            arguments.last().expect("callback"),
            substitutions,
            span,
        )?;
        let Value::List(items) = &arguments[0] else {
            unreachable!("validated list carrier")
        };
        let element = &substitutions["T"];
        let folding = descriptor.implementation() == StandardOperation::Fold;
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
        for item in items.iter() {
            self.check_cancel(span)?;
            self.charge(span)?;
            if !self.validate_operation_value(element, item, span)? {
                return Err(self.invalid_callback(
                    descriptor,
                    "list item does not fit its type argument".to_owned(),
                    span,
                ));
            }
            let values = if folding {
                vec![accumulator.clone(), item.clone()]
            } else {
                vec![item.clone()]
            };
            let result = self.invoke_callback(&callback, values, span)?;
            let retained = match descriptor.implementation() {
                StandardOperation::Map => Some(result),
                StandardOperation::Filter => match result {
                    Value::Bool(true) => Some(item.clone()),
                    Value::Bool(false) => None,
                    _ => unreachable!("validated predicate result"),
                },
                StandardOperation::Fold => {
                    accumulator = result;
                    None
                }
                _ => unreachable!("callback list operation"),
            };
            if let Some(value) = retained {
                if !self.budget.charge_collection_items(1) {
                    return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
                }
                if output.len() == output.capacity() {
                    let capacity = output.capacity().saturating_mul(2).max(1).min(items.len());
                    let additional = capacity - output.capacity();
                    let bytes = additional.checked_mul(size_of::<Value>()).ok_or_else(|| {
                        self.error(RuntimeErrorKind::ResourceBudgetExceeded, span)
                    })?;
                    self.retain_collection_bytes(bytes, span)?;
                    output
                        .try_reserve_exact(additional)
                        .map_err(|_| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
                }
                output.push(value);
            }
        }
        if folding {
            Ok(accumulator)
        } else {
            Ok(Value::list(output))
        }
    }
}
