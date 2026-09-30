//! Immutable structural record operations with bounded key work.

use std::cmp::Ordering;
use std::sync::Arc;

use opaal_syntax::Span;

use super::{Eval, Evaluator, RuntimeErrorKind};
use crate::operation::{OperationDescriptor, OperationError, StandardOperation};
use crate::{Record, Value};

type Entry = (Arc<str>, Value);

impl Evaluator<'_, '_> {
    pub(super) fn record_operation(
        &mut self,
        descriptor: &OperationDescriptor,
        arguments: &[Value],
        span: Span,
    ) -> Eval<Value> {
        use StandardOperation::{
            RecordGetOr, RecordHas, RecordKeys, RecordMerge, RecordSelect, RecordSet,
        };
        self.check_cancel(span)?;
        let Value::Record(input) = &arguments[0] else {
            unreachable!("structural Record overload checked")
        };
        match descriptor.implementation() {
            RecordKeys => {
                let mut output = self.record_workspace(input.entries().len(), span)?;
                for (key, _) in input.entries() {
                    self.record_visit(span)?;
                    self.retain_record_key(key, span)?;
                    self.retain_record_item(span)?;
                    output.push(Value::String(Arc::clone(key)));
                }
                Ok(Value::list(output))
            }
            RecordHas | RecordGetOr | RecordSet => {
                let Value::String(key) = &arguments[1] else {
                    unreachable!("String overload checked")
                };
                let position = self.record_position(input, key, span)?;
                if descriptor.implementation() == RecordHas {
                    return Ok(Value::Bool(position.is_some()));
                }
                if descriptor.implementation() == RecordGetOr {
                    self.retain_record_item(span)?;
                    self.retain_collection_bytes(size_of::<Value>(), span)?;
                    return Ok(position
                        .map_or(&arguments[2], |index| &input.entries()[index].1)
                        .clone());
                }
                let length = input
                    .entries()
                    .len()
                    .checked_add(usize::from(position.is_none()))
                    .ok_or_else(|| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
                let mut output = self.record_workspace(length, span)?;
                for (index, (existing, value)) in input.entries().iter().enumerate() {
                    let value = if position == Some(index) {
                        &arguments[2]
                    } else {
                        value
                    };
                    self.copy_record_entry(&mut output, existing, value, span)?;
                }
                if position.is_none() {
                    self.copy_record_entry(&mut output, key, &arguments[2], span)?;
                }
                Ok(Record::from_unique_entries(output).into())
            }
            RecordSelect => {
                let Value::List(keys) = &arguments[1] else {
                    unreachable!("List overload checked")
                };
                // Validate every requested item before building any result.
                for key in keys.iter() {
                    self.record_visit(span)?;
                    if !matches!(key, Value::String(_)) {
                        return Err(self.record_error(
                            descriptor,
                            "requested keys must be Strings".to_owned(),
                            span,
                        ));
                    }
                }
                if keys.is_empty() {
                    return Ok(Record::from_unique_entries(Vec::new()).into());
                }
                let key_at = |index: usize| match &keys[index] {
                    Value::String(key) => key.as_ref(),
                    _ => unreachable!("requested keys validated"),
                };
                let sorted_requests = self.sorted_record_indices(keys.len(), key_at, span)?;
                for pair in sorted_requests.windows(2) {
                    if self.record_compare(key_at(pair[0]), key_at(pair[1]), span)?
                        == Ordering::Equal
                    {
                        return Err(self.record_error(
                            descriptor,
                            format!("repeated requested key {}", key_excerpt(key_at(pair[1]))),
                            span,
                        ));
                    }
                }
                let entries = input.entries();
                let sorted_input = self.sorted_record_indices(
                    entries.len(),
                    |index| entries[index].0.as_ref(),
                    span,
                )?;
                let mut output = self.record_workspace(keys.len(), span)?;
                for index in 0..keys.len() {
                    self.record_visit(span)?;
                    let key = key_at(index);
                    let (mut low, mut high) = (0, sorted_input.len());
                    let mut found = None;
                    while low < high {
                        let middle = low + (high - low) / 2;
                        let entry = sorted_input[middle];
                        match self.record_compare(&entries[entry].0, key, span)? {
                            Ordering::Less => low = middle + 1,
                            Ordering::Greater => high = middle,
                            Ordering::Equal => {
                                found = Some(entry);
                                break;
                            }
                        }
                    }
                    let Some(found) = found else {
                        return Err(self.record_error(
                            descriptor,
                            format!("missing requested key {}", key_excerpt(key)),
                            span,
                        ));
                    };
                    self.copy_record_entry(
                        &mut output,
                        &entries[found].0,
                        &entries[found].1,
                        span,
                    )?;
                }
                Ok(Record::from_unique_entries(output).into())
            }
            RecordMerge => {
                let Value::Record(right) = &arguments[1] else {
                    unreachable!("structural Record overload checked")
                };
                self.merge_records(input, right, span)
            }
            _ => unreachable!("record operation dispatch"),
        }
    }

    fn record_error(
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

    fn record_visit(&mut self, span: Span) -> Eval<()> {
        self.check_cancel(span)?;
        self.charge(span)
    }

    fn record_compare(&mut self, left: &str, right: &str, span: Span) -> Eval<Ordering> {
        self.record_visit(span)?;
        self.compare_key_bytes(left.as_bytes(), right.as_bytes(), span)
    }

    fn record_position(&mut self, input: &Record, key: &str, span: Span) -> Eval<Option<usize>> {
        for (index, (candidate, _)) in input.entries().iter().enumerate() {
            if self.record_compare(candidate, key, span)? == Ordering::Equal {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    fn retain_record_item(&mut self, span: Span) -> Eval<()> {
        self.check_cancel(span)?;
        if self.budget.charge_collection_items(1) {
            Ok(())
        } else {
            Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))
        }
    }

    fn retain_record_key(&mut self, key: &str, span: Span) -> Eval<()> {
        for chunk in key.as_bytes().chunks(4096) {
            self.record_visit(span)?;
            self.retain_collection_bytes(chunk.len(), span)?;
        }
        Ok(())
    }

    fn copy_record_entry(
        &mut self,
        output: &mut Vec<Entry>,
        key: &Arc<str>,
        value: &Value,
        span: Span,
    ) -> Eval<()> {
        self.record_visit(span)?;
        self.retain_record_key(key, span)?;
        self.retain_record_item(span)?;
        output.push((Arc::clone(key), value.clone()));
        Ok(())
    }

    fn record_workspace<T>(&mut self, length: usize, span: Span) -> Eval<Vec<T>> {
        self.check_cancel(span)?;
        let bytes = length
            .checked_mul(size_of::<T>())
            .ok_or_else(|| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
        self.retain_collection_bytes(bytes, span)?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(length)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
        Ok(result)
    }

    /// Stable bottom-up sorting over borrowed keys. Two index buffers bound
    /// extra storage to O(n); every comparison and 4096 key bytes are charged.
    fn sorted_record_indices<'a>(
        &mut self,
        length: usize,
        key: impl Fn(usize) -> &'a str,
        span: Span,
    ) -> Eval<Vec<usize>> {
        let mut indices = self.record_workspace(length, span)?;
        for index in 0..length {
            self.record_visit(span)?;
            indices.push(index);
        }
        if length < 2 {
            return Ok(indices);
        }
        let mut workspace = self.record_workspace(length, span)?;
        for _ in 0..length {
            self.record_visit(span)?;
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
                    self.record_visit(span)?;
                    if left < middle
                        && (right == end
                            || self.record_compare(
                                key(indices[left]),
                                key(indices[right]),
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
        Ok(indices)
    }

    fn merge_records(&mut self, left: &Record, right: &Record, span: Span) -> Eval<Value> {
        let left = left.entries();
        let right = right.entries();
        let length = left
            .len()
            .checked_add(right.len())
            .ok_or_else(|| self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
        let entry = |index: usize| {
            if index < left.len() {
                &left[index]
            } else {
                &right[index - left.len()]
            }
        };
        let sorted = self.sorted_record_indices(length, |index| entry(index).0.as_ref(), span)?;
        let mut winners = self.record_workspace(length, span)?;
        for _ in 0..length {
            self.record_visit(span)?;
            winners.push(usize::MAX);
        }
        let mut cursor = 0;
        let mut retained = 0;
        while cursor < length {
            self.record_visit(span)?;
            let first = sorted[cursor];
            // Each input is unique. Stable sorting places a left duplicate
            // before its right replacement, so groups contain at most two.
            if cursor + 1 < length
                && self.record_compare(&entry(first).0, &entry(sorted[cursor + 1]).0, span)?
                    == Ordering::Equal
            {
                let second = sorted[cursor + 1];
                debug_assert!(first < left.len() && second >= left.len());
                winners[first] = second;
                cursor += 2;
            } else {
                winners[first] = first;
                cursor += 1;
            }
            retained += 1;
        }
        let mut output = self.record_workspace(retained, span)?;
        // Original index order is left order followed by right order. Winners
        // replace left slots and suppress their corresponding right slots.
        for winner in winners {
            self.record_visit(span)?;
            if winner != usize::MAX {
                let (key, value) = entry(winner);
                self.copy_record_entry(&mut output, key, value, span)?;
            }
        }
        Ok(Record::from_unique_entries(output).into())
    }
}

/// Keys are ordinary data and may be sensitive. Errors show only a bounded,
/// escaped excerpt; control characters never enter diagnostics literally.
fn key_excerpt(key: &str) -> String {
    let mut output = String::from("\"");
    let mut chars = key.chars();
    for character in chars.by_ref().take(24) {
        output.extend(character.escape_default());
    }
    if chars.next().is_some() {
        output.push('…');
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;
    use crate::eval::{
        Abort, CancellationToken, EvaluationPolicy, PureEvaluationHost, ResourceBudget,
    };
    use crate::module::{ModuleId, RuntimeBindingTypes};
    use crate::operation::standard_operation;
    use opaal_syntax::{SourceFile, SourceId};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn run(
        name: &str,
        arguments: Vec<Value>,
        budget: &mut ResourceBudget,
        cancel: CancellationToken,
    ) -> Eval<Value> {
        let source = Arc::new(SourceFile::new(SourceId::new(1), "records.opaal", "call"));
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
        let operation = standard_operation(&ModuleId::standard("std", "record"), name)
            .unwrap_or_else(|| panic!("unexpected failure"));
        evaluator.execute_operation(
            &operation,
            arguments,
            &[],
            evaluator
                .source
                .span(0..4)
                .unwrap_or_else(|_| panic!("unexpected failure")),
        )
    }

    fn record(keys: &[&str]) -> Value {
        Record::new(
            keys.iter()
                .enumerate()
                .map(|(index, key)| ((*key).to_owned(), Value::Int(index as i64)))
                .collect(),
        )
        .unwrap_or_else(|_| panic!("unexpected failure"))
        .into()
    }

    fn budget_error(result: Eval<Value>) -> bool {
        matches!(result, Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
    }

    #[test]
    fn record_retention_charges_exact_items_keys_slots_and_index_workspace() {
        let slot = size_of::<Entry>();
        let index_slot = size_of::<usize>();
        for (name, arguments, items, bytes) in [
            (
                "keys",
                vec![record(&["é", "b"])],
                2,
                2 * size_of::<Value>() + 3,
            ),
            (
                "get_or",
                vec![record(&["a"]), Value::string("a"), Value::Null],
                1,
                size_of::<Value>(),
            ),
            (
                "set",
                vec![record(&["a", "b"]), Value::string("b"), Value::Null],
                2,
                2 * slot + 2,
            ),
            (
                "set",
                vec![record(&["a"]), Value::string("b"), Value::Null],
                2,
                2 * slot + 2,
            ),
            (
                "select",
                vec![
                    record(&["a", "b"]),
                    Value::list(vec![Value::string("b"), Value::string("a")]),
                ],
                2,
                8 * index_slot + 2 * slot + 2,
            ),
            (
                "merge",
                vec![record(&["a", "b"]), record(&["b", "c"])],
                3,
                12 * index_slot + 3 * slot + 3,
            ),
        ] {
            let mut exact = ResourceBudget::steps(10_000)
                .with_collection_items(items)
                .with_collection_bytes(bytes as u64);
            assert!(
                run(
                    name,
                    arguments.clone(),
                    &mut exact,
                    CancellationToken::never()
                )
                .is_ok(),
                "{name}"
            );
            assert_eq!(exact.collection_items(), items, "{name}");
            assert_eq!(exact.collection_bytes(), bytes as u64, "{name}");
            let mut short = ResourceBudget::steps(10_000).with_collection_bytes(bytes as u64 - 1);
            assert!(
                budget_error(run(
                    name,
                    arguments.clone(),
                    &mut short,
                    CancellationToken::never()
                )),
                "{name}"
            );
            assert!(short.collection_bytes() < bytes as u64);
            let mut short = ResourceBudget::steps(10_000).with_collection_items(items - 1);
            assert!(
                budget_error(run(
                    name,
                    arguments.clone(),
                    &mut short,
                    CancellationToken::never()
                )),
                "{name}"
            );
            // Reusing an exhausted budget never replenishes its counters.
            assert!(
                budget_error(run(name, arguments, &mut exact, CancellationToken::never())),
                "{name}"
            );
        }
    }

    #[test]
    fn long_key_comparison_and_copy_charge_each_started_chunk() {
        let key = "x".repeat(4096 * 3 + 1);
        let input = record(&[&key]);
        let mut exact = ResourceBudget::steps(5);
        assert_eq!(
            run(
                "has",
                vec![input.clone(), Value::string(&key)],
                &mut exact,
                CancellationToken::never()
            )
            .unwrap_or_else(|_| panic!("unexpected failure")),
            Value::Bool(true)
        );
        assert_eq!(exact.used(), 5);
        assert!(budget_error(run(
            "has",
            vec![input.clone(), Value::string(&key)],
            &mut ResourceBudget::steps(4),
            CancellationToken::never()
        )));
        assert!(budget_error(run(
            "keys",
            vec![input],
            &mut ResourceBudget::steps(4),
            CancellationToken::never()
        )));
        assert!(key_excerpt("\n\r\t\u{1b}\"\\").contains("\\n\\r\\t\\u{1b}"));
        assert!(key_excerpt(&key).len() < 100);
    }

    #[test]
    fn merge_growth_is_bounded_for_distinct_overlapping_and_long_prefix_keys() {
        for size in [16usize, 64, 256, 1024] {
            for overlap in [false, true] {
                let left: Value = Record::from_unique_entries(
                    (0..size)
                        .map(|index| {
                            (
                                Arc::from(format!("key-{index:05}")),
                                Value::Int(index as i64),
                            )
                        })
                        .collect(),
                )
                .into();
                let right: Value = Record::from_unique_entries(
                    (0..size)
                        .rev()
                        .map(|index| {
                            (
                                Arc::from(format!(
                                    "key-{:05}",
                                    if overlap { index } else { index + size }
                                )),
                                Value::Null,
                            )
                        })
                        .collect(),
                )
                .into();
                let arguments = vec![left.clone(), right.clone()];
                let n = 2 * size;
                let bound = (8 * n * n.ilog2() as usize + 12 * n) as u64;
                let mut budget = ResourceBudget::steps(bound);
                let Value::Record(result) = run(
                    "merge",
                    arguments.clone(),
                    &mut budget,
                    CancellationToken::never(),
                )
                .unwrap_or_else(|_| panic!("unexpected failure")) else {
                    panic!("merge returns a Record")
                };
                assert_eq!(result.entries().len(), if overlap { size } else { n });
                for (index, (key, value)) in result.entries().iter().enumerate() {
                    if index < size {
                        assert_eq!(key.as_ref(), format!("key-{index:05}"));
                        assert_eq!(
                            *value,
                            if overlap {
                                Value::Null
                            } else {
                                Value::Int(index as i64)
                            }
                        );
                    } else {
                        assert_eq!(key.as_ref(), format!("key-{:05}", 3 * size - index - 1));
                        assert_eq!(*value, Value::Null);
                    }
                }
                assert!(budget.used() < bound);
                let mut short = ResourceBudget::steps(budget.used() - 1);
                assert!(budget_error(run(
                    "merge",
                    arguments,
                    &mut short,
                    CancellationToken::never()
                )));
                assert_eq!(left, record_for_growth(size));
            }
        }
        let prefix = "x".repeat(4096 * 4);
        let left = record(&[&format!("{prefix}a"), &format!("{prefix}b")]);
        let right = record(&[&format!("{prefix}c"), &format!("{prefix}a")]);
        let mut budget = ResourceBudget::steps(1000);
        assert!(
            run(
                "merge",
                vec![left.clone(), right.clone()],
                &mut budget,
                CancellationToken::never()
            )
            .is_ok()
        );
        assert!(
            budget.used() > 50,
            "key bytes must be charged alongside comparisons"
        );
        assert!(budget_error(run(
            "merge",
            vec![left, right],
            &mut ResourceBudget::steps(50),
            CancellationToken::never()
        )));
    }

    fn record_for_growth(size: usize) -> Value {
        Record::from_unique_entries(
            (0..size)
                .map(|index| {
                    (
                        Arc::from(format!("key-{index:05}")),
                        Value::Int(index as i64),
                    )
                })
                .collect(),
        )
        .into()
    }

    #[test]
    fn every_record_loop_and_long_key_scan_cancels_without_partial_results() {
        let large = record_for_growth(512);
        let huge_key = "x".repeat(4096 * 40);
        for (name, arguments) in [
            ("keys", vec![large.clone()]),
            ("has", vec![large.clone(), Value::string("missing")]),
            (
                "get_or",
                vec![large.clone(), Value::string("missing"), Value::Null],
            ),
            (
                "set",
                vec![large.clone(), Value::string("new"), Value::Null],
            ),
            (
                "select",
                vec![large.clone(), Value::list(vec![Value::string("key-00000")])],
            ),
            ("merge", vec![large.clone(), large]),
            ("keys", vec![record(&[&huge_key])]),
            ("has", vec![record(&[&huge_key]), Value::string(&huge_key)]),
            (
                "set",
                vec![record(&[]), Value::string(&huge_key), Value::Null],
            ),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&polls);
            let token =
                CancellationToken::from_fn(move || seen.fetch_add(1, AtomicOrdering::SeqCst) >= 10);
            assert!(
                matches!(
                    run(name, arguments, &mut ResourceBudget::default(), token),
                    Err(Abort::Cancelled(_))
                ),
                "{name}"
            );
            assert_eq!(polls.load(AtomicOrdering::SeqCst), 11);
        }
    }

    #[test]
    fn merge_matches_an_independent_ordered_reference_over_all_small_key_sets() {
        let keys = ["", "é", "é", "x"];
        for left_bits in 0..16 {
            for right_bits in 0..16 {
                let left_entries = keys
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| left_bits & (1 << index) != 0)
                    .map(|(index, key)| ((*key).to_owned(), Value::Int(index as i64)))
                    .collect::<Vec<_>>();
                let right_entries = keys
                    .iter()
                    .rev()
                    .enumerate()
                    .filter(|(index, _)| right_bits & (1 << index) != 0)
                    .map(|(_, key)| ((*key).to_owned(), Value::Null))
                    .collect::<Vec<_>>();
                let left: Value = Record::new(left_entries.clone())
                    .unwrap_or_else(|_| panic!("unexpected failure"))
                    .into();
                let right: Value = Record::new(right_entries.clone())
                    .unwrap_or_else(|_| panic!("unexpected failure"))
                    .into();
                let mut expected = left_entries;
                for (key, value) in right_entries {
                    if let Some(existing) =
                        expected.iter_mut().find(|(candidate, _)| *candidate == key)
                    {
                        existing.1 = value;
                    } else {
                        expected.push((key, value));
                    }
                }
                let expected: Value = Record::new(expected)
                    .unwrap_or_else(|_| panic!("unexpected failure"))
                    .into();
                assert_eq!(
                    run(
                        "merge",
                        vec![left.clone(), right.clone()],
                        &mut ResourceBudget::default(),
                        CancellationToken::never()
                    )
                    .unwrap_or_else(|_| panic!("unexpected failure")),
                    expected
                );
            }
        }
    }

    #[test]
    fn cancellation_at_every_poll_covers_sort_search_and_retention_phases() {
        let prefix = "é".repeat(4096);
        let a = format!("{prefix}a");
        let b = format!("{prefix}b");
        let input = record(&[&b, &a]);
        for (name, arguments) in [
            ("keys", vec![input.clone()]),
            ("has", vec![input.clone(), Value::string(&a)]),
            (
                "get_or",
                vec![input.clone(), Value::string(&a), Value::Null],
            ),
            (
                "set",
                vec![input.clone(), Value::string("new"), Value::Null],
            ),
            (
                "select",
                vec![
                    input.clone(),
                    Value::list(vec![Value::string(&a), Value::string(&b)]),
                ],
            ),
            ("merge", vec![input.clone(), record(&[&a, "new"])]),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&polls);
            let token = CancellationToken::from_fn(move || {
                seen.fetch_add(1, AtomicOrdering::SeqCst);
                false
            });
            assert!(
                run(
                    name,
                    arguments.clone(),
                    &mut ResourceBudget::default(),
                    token
                )
                .is_ok()
            );
            let completed_polls = polls.load(AtomicOrdering::SeqCst);
            for stop_at in 0..completed_polls {
                let seen = Arc::new(AtomicUsize::new(0));
                let counted = Arc::clone(&seen);
                let token = CancellationToken::from_fn(move || {
                    counted.fetch_add(1, AtomicOrdering::SeqCst) >= stop_at
                });
                assert!(
                    matches!(
                        run(
                            name,
                            arguments.clone(),
                            &mut ResourceBudget::default(),
                            token
                        ),
                        Err(Abort::Cancelled(_))
                    ),
                    "{name}, cancellation poll {stop_at} of {completed_polls}"
                );
                assert_eq!(seen.load(AtomicOrdering::SeqCst), stop_at + 1);
            }
        }
    }
}
