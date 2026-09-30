//! Budgeted compiled operations share the evaluator's live control state.

use super::{Eval, Evaluator, RuntimeErrorKind};
use crate::Value;
use crate::module::substitute_type;
use crate::operation::{
    OperationDescriptor, OperationError, OperationInputType, StandardOperation,
};
use opaal_syntax::Span;

const CHUNK: usize = 4096;

/// Poll and charge before the first byte of each bounded unit of scan work.
#[derive(Default)]
struct ScanWork {
    remaining: usize,
}

impl ScanWork {
    fn visit(&mut self, evaluator: &mut Evaluator<'_, '_>, bytes: usize, span: Span) -> Eval<()> {
        for _ in 0..bytes {
            if self.remaining == 0 {
                evaluator.check_cancel(span)?;
                evaluator.charge(span)?;
                self.remaining = CHUNK;
            }
            self.remaining -= 1;
        }
        Ok(())
    }
}

/// Linear literal search, including adversarial repeated-prefix patterns.
/// The prefix table is bounded by the caller's allocation budget.
struct LiteralSearch<'a> {
    pattern: &'a [u8],
    prefix: Vec<usize>,
    cursor: usize,
    matched: usize,
    work: ScanWork,
}

impl<'a> LiteralSearch<'a> {
    fn new(pattern: &'a str, evaluator: &mut Evaluator<'_, '_>, span: Span) -> Eval<Self> {
        debug_assert!(!pattern.is_empty());
        evaluator.check_cancel(span)?;
        let bytes = pattern
            .len()
            .checked_mul(size_of::<usize>())
            .ok_or_else(|| evaluator.error(RuntimeErrorKind::ResourceBudgetExceeded, span))?;
        evaluator.retain_collection_bytes(bytes, span)?;
        let pattern = pattern.as_bytes();
        let mut work = ScanWork::default();
        let mut prefix = Vec::with_capacity(pattern.len());
        work.visit(evaluator, 1, span)?;
        prefix.push(0);
        let mut matched = 0;
        for index in 1..pattern.len() {
            work.visit(evaluator, 1, span)?;
            while matched > 0 && pattern[index] != pattern[matched] {
                work.visit(evaluator, 1, span)?;
                matched = prefix[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            prefix.push(matched);
        }
        Ok(Self {
            pattern,
            prefix,
            cursor: 0,
            matched: 0,
            work,
        })
    }

    fn next(
        &mut self,
        input: &str,
        evaluator: &mut Evaluator<'_, '_>,
        span: Span,
    ) -> Eval<Option<usize>> {
        let input = input.as_bytes();
        while self.cursor < input.len() {
            self.work.visit(evaluator, 1, span)?;
            let byte = input[self.cursor];
            while self.matched > 0 && byte != self.pattern[self.matched] {
                self.work.visit(evaluator, 1, span)?;
                self.matched = self.prefix[self.matched - 1];
            }
            if byte == self.pattern[self.matched] {
                self.matched += 1;
            }
            self.cursor += 1;
            if self.matched == self.pattern.len() {
                // Restart after the match: split and replace never overlap.
                self.matched = 0;
                return Ok(Some(self.cursor - self.pattern.len()));
            }
        }
        Ok(None)
    }
}

impl Evaluator<'_, '_> {
    pub(super) fn execute_operation(
        &mut self,
        descriptor: &OperationDescriptor,
        arguments: Vec<Value>,
        type_arguments: &[crate::module::ValueType],
        span: Span,
    ) -> Eval<Value> {
        if descriptor.implementation() == StandardOperation::Length {
            return descriptor
                .execute_value_with_types(
                    arguments.into_iter().next().expect("arity checked"),
                    type_arguments,
                )
                .map_err(|error| self.operation(error, span));
        }
        let overload = descriptor.value_overload().expect("value operation");
        let substitutions = descriptor
            .type_parameters()
            .iter()
            .cloned()
            .zip(type_arguments.iter().cloned())
            .collect();
        for (argument, parameter) in arguments.iter().zip(overload.parameters()) {
            let OperationInputType::Value(expected) = parameter.input() else {
                unreachable!("value parameter")
            };
            // Lists are validated during their budgeted traversal below.
            let accepted = match (expected, argument) {
                (crate::module::ValueType::List(_), Value::List(_)) => true,
                _ => substitute_type(expected, &substitutions).accepts(argument),
            };
            if !accepted {
                return Err(self.operation(
                    OperationError::NoMatchingOverload {
                        operation: descriptor.id().qualified_name(),
                        input: argument.family_name().to_owned(),
                    },
                    span,
                ));
            }
        }
        match descriptor.implementation() {
            StandardOperation::Map | StandardOperation::Filter | StandardOperation::Fold => {
                self.list_operation(descriptor, &arguments, &substitutions, span)
            }
            _ => self.string_operation(descriptor, &arguments, span),
        }
    }

    fn invalid_string_argument(
        &self,
        descriptor: &OperationDescriptor,
        message: impl Into<String>,
        span: Span,
    ) -> super::Abort {
        self.operation(
            OperationError::InvalidArgument {
                operation: descriptor.id().qualified_name(),
                message: message.into(),
            },
            span,
        )
    }

    fn append_text(&mut self, output: &mut String, text: &str, span: Span) -> Eval<()> {
        let mut cursor = 0;
        while cursor < text.len() {
            self.check_cancel(span)?;
            self.charge(span)?;
            let mut end = (cursor + CHUNK).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.retain_collection_bytes(end - cursor, span)?;
            output.push_str(&text[cursor..end]);
            cursor = end;
        }
        Ok(())
    }

    fn copy_text(&mut self, text: &str, span: Span) -> Eval<Value> {
        let mut output = String::new();
        self.append_text(&mut output, text, span)?;
        Ok(Value::string(output))
    }

    fn string_operation(
        &mut self,
        descriptor: &OperationDescriptor,
        arguments: &[Value],
        span: Span,
    ) -> Eval<Value> {
        use StandardOperation::{
            Contains, DecodeUtf8, EndsWith, Join, Replace, Split, StartsWith, Trim,
        };
        self.check_cancel(span)?;
        if descriptor.implementation() == Join {
            let [Value::List(values), Value::String(separator)] = arguments else {
                unreachable!("validated arguments")
            };
            let mut output = String::new();
            for (index, value) in values.iter().enumerate() {
                self.check_cancel(span)?;
                self.charge(span)?;
                let Value::String(text) = value else {
                    return Err(self.invalid_string_argument(
                        descriptor,
                        "join requires String items",
                        span,
                    ));
                };
                if index > 0 {
                    self.append_text(&mut output, separator, span)?;
                }
                self.append_text(&mut output, text, span)?;
            }
            return Ok(Value::string(output));
        }
        if descriptor.implementation() == DecodeUtf8 {
            let [Value::Bytes(bytes)] = arguments else {
                unreachable!("validated arguments")
            };
            let mut cursor = 0;
            while cursor < bytes.len() {
                self.check_cancel(span)?;
                self.charge(span)?;
                let end = (cursor + CHUNK).min(bytes.len());
                match std::str::from_utf8(&bytes[cursor..end]) {
                    Ok(_) => cursor = end,
                    Err(error) if error.error_len().is_none() && end < bytes.len() => {
                        cursor += error.valid_up_to();
                    }
                    Err(error) => {
                        return Err(self.invalid_string_argument(
                            descriptor,
                            format!("invalid UTF8 at byte {}", cursor + error.valid_up_to()),
                            span,
                        ));
                    }
                }
            }
            // Every byte has been validated in bounded chunks; conversion performs
            // no second uninterruptible scan. No unsafe UTF8 constructor is needed.
            let mut output = String::new();
            cursor = 0;
            while cursor < bytes.len() {
                self.check_cancel(span)?;
                self.charge(span)?;
                let mut end = (cursor + CHUNK).min(bytes.len());
                let text = loop {
                    match std::str::from_utf8(&bytes[cursor..end]) {
                        Ok(text) => break text,
                        Err(error) => end = cursor + error.valid_up_to(),
                    }
                };
                self.retain_collection_bytes(text.len(), span)?;
                output.push_str(text);
                cursor = end;
            }
            return Ok(Value::string(output));
        }
        let Value::String(input) = &arguments[0] else {
            unreachable!("validated arguments")
        };
        if descriptor.implementation() == Trim {
            let mut work = ScanWork::default();
            let mut start = 0;
            for character in input.chars() {
                work.visit(self, character.len_utf8(), span)?;
                if !character.is_whitespace() {
                    break;
                }
                start += character.len_utf8();
            }
            let mut end = input.len();
            for character in input[start..].chars().rev() {
                work.visit(self, character.len_utf8(), span)?;
                if !character.is_whitespace() {
                    break;
                }
                end -= character.len_utf8();
            }
            return self.copy_text(&input[start..end], span);
        }
        let Value::String(pattern) = &arguments[1] else {
            unreachable!("validated arguments")
        };
        if matches!(descriptor.implementation(), StartsWith | EndsWith) {
            if pattern.len() > input.len() {
                return Ok(Value::Bool(false));
            }
            let candidate = if descriptor.implementation() == StartsWith {
                &input.as_bytes()[..pattern.len()]
            } else {
                &input.as_bytes()[input.len() - pattern.len()..]
            };
            for (left, right) in candidate
                .chunks(CHUNK)
                .zip(pattern.as_bytes().chunks(CHUNK))
            {
                self.check_cancel(span)?;
                self.charge(span)?;
                if left != right {
                    return Ok(Value::Bool(false));
                }
            }
            return Ok(Value::Bool(true));
        }
        if pattern.is_empty() {
            return if descriptor.implementation() == Contains {
                Ok(Value::Bool(true))
            } else {
                Err(self.invalid_string_argument(
                    descriptor,
                    "separator or pattern must be nonempty",
                    span,
                ))
            };
        }
        let mut search = LiteralSearch::new(pattern, self, span)?;
        if descriptor.implementation() == Contains {
            return Ok(Value::Bool(search.next(input, self, span)?.is_some()));
        }
        let mut cursor = 0;
        let mut output = String::new();
        let mut fields = Vec::new();
        while let Some(position) = search.next(input, self, span)? {
            self.check_cancel(span)?;
            self.charge(span)?;
            if descriptor.implementation() == Split {
                self.retain_string_field(&mut fields, &input[cursor..position], span)?;
            } else {
                self.append_text(&mut output, &input[cursor..position], span)?;
                let Value::String(replacement) = &arguments[2] else {
                    unreachable!("validated arguments")
                };
                self.append_text(&mut output, replacement, span)?;
            }
            cursor = position + pattern.len();
        }
        match descriptor.implementation() {
            Split => {
                self.retain_string_field(&mut fields, &input[cursor..], span)?;
                Ok(Value::list(fields))
            }
            Replace => {
                self.append_text(&mut output, &input[cursor..], span)?;
                Ok(Value::string(output))
            }
            _ => unreachable!("compiled String implementation"),
        }
    }

    fn retain_string_field(&mut self, fields: &mut Vec<Value>, text: &str, span: Span) -> Eval<()> {
        self.check_cancel(span)?;
        if !self.budget.charge_collection_items(1) {
            return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
        }
        let value = self.copy_text(text, span)?;
        fields.push(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;
    use crate::eval::{
        Abort, CancellationToken, Clock, EvaluationPolicy, Instant, PureEvaluationHost,
        ResourceBudget,
    };
    use crate::module::{ModuleId, RuntimeBindingTypes};
    use crate::operation::standard_operation;
    use opaal_syntax::{SourceFile, SourceId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn run(
        name: &str,
        arguments: Vec<Value>,
        budget: &mut ResourceBudget,
        cancel: CancellationToken,
    ) -> Eval<Value> {
        let source = Arc::new(SourceFile::new(
            SourceId::new(1),
            "operations.opaal",
            "call",
        ));
        let mut environment = Environment::new();
        let mut host = PureEvaluationHost {
            environment: &mut environment,
            policy: EvaluationPolicy::PureOpaal,
        };
        let mut evaluator = Evaluator {
            source,
            binding_types: Arc::new(RuntimeBindingTypes::default()),
            current_result_type: None,
            current_type_arguments: std::collections::BTreeMap::new(),
            budgeted_callback: false,
            cancel,
            budget,
            host: &mut host,
        };
        let descriptor = standard_operation(&ModuleId::standard("std", "string"), name).unwrap();
        evaluator.execute_operation(
            &descriptor,
            arguments,
            &[],
            evaluator.source.span(0..4).unwrap(),
        )
    }

    #[test]
    fn strict_utf8_preserves_bom_and_newlines_across_chunk_boundaries() {
        for text in [
            "",
            "\u{feff} café\r\n",
            &format!("{}é終\n", "a".repeat(CHUNK - 1)),
        ] {
            assert_eq!(
                run(
                    "decode_utf8",
                    vec![Value::bytes(text.as_bytes().to_vec())],
                    &mut ResourceBudget::default(),
                    CancellationToken::never()
                )
                .unwrap_or_else(|_| panic!("unexpected operation failure")),
                Value::string(text)
            );
        }
        for bytes in [
            vec![0xff],
            vec![b'a', 0xc3],
            vec![0xed, 0xa0, 0x80],
            [vec![b'a'; CHUNK - 1], vec![0xc3, 0x28]].concat(),
        ] {
            let result = run(
                "decode_utf8",
                vec![Value::bytes(bytes)],
                &mut ResourceBudget::default(),
                CancellationToken::never(),
            );
            let Err(Abort::Error(error)) = result else {
                panic!("invalid UTF8 must fail")
            };
            assert!(error.to_string().contains("invalid UTF8 at byte"));
        }
    }

    #[test]
    fn strict_utf8_handles_every_scalar_width_at_chunk_edges_and_exact_error_offsets() {
        for padding in CHUNK - 4..=CHUNK + 1 {
            for scalar in ["a", "é", "終", "😀"] {
                let text = format!("{}{}\r\n", "x".repeat(padding), scalar.repeat(3));
                assert_eq!(
                    run(
                        "decode_utf8",
                        vec![Value::bytes(text.as_bytes().to_vec())],
                        &mut ResourceBudget::default(),
                        CancellationToken::never(),
                    )
                    .unwrap_or_else(|_| panic!("valid UTF8 must succeed")),
                    Value::string(text)
                );
            }
            for invalid in [
                vec![0xff],
                vec![0xc0, 0x80],
                vec![0xc3],
                vec![0xe2, 0x82],
                vec![0xed, 0xa0, 0x80],
                vec![0xf4, 0x90, 0x80, 0x80],
            ] {
                let bytes = [vec![b'x'; padding], invalid].concat();
                let Err(Abort::Error(error)) = run(
                    "decode_utf8",
                    vec![Value::bytes(bytes)],
                    &mut ResourceBudget::default(),
                    CancellationToken::never(),
                ) else {
                    panic!("invalid UTF8 must fail")
                };
                assert_eq!(
                    error.to_string(),
                    format!("std::string::decode_utf8: invalid UTF8 at byte {padding}")
                );
            }
        }
    }

    #[test]
    fn anchored_scans_charge_each_started_chunk_and_output_shares_byte_budget() {
        for name in ["starts_with", "ends_with"] {
            for length in [1, CHUNK, CHUNK + 1, CHUNK * 3] {
                let text = Value::string("x".repeat(length));
                let steps = length.div_ceil(CHUNK) as u64;
                assert_eq!(
                    run(
                        name,
                        vec![text.clone(), text.clone()],
                        &mut ResourceBudget::steps(steps),
                        CancellationToken::never(),
                    )
                    .unwrap_or_else(|_| panic!("exact scan budget must succeed")),
                    Value::Bool(true)
                );
                assert!(matches!(
                    run(
                        name,
                        vec![text.clone(), text],
                        &mut ResourceBudget::steps(steps - 1),
                        CancellationToken::never(),
                    ),
                    Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)
                ));
            }
        }
        let mut budget = ResourceBudget::default().with_collection_bytes(3);
        assert_eq!(
            run(
                "join",
                vec![
                    Value::list(vec![Value::string("é"), Value::string("")]),
                    Value::string("/")
                ],
                &mut budget,
                CancellationToken::never(),
            )
            .unwrap_or_else(|_| panic!("exact output budget must succeed")),
            Value::string("é/")
        );
        assert_eq!(budget.collection_bytes(), 3);
        for _ in 0..2 {
            assert!(matches!(
                run("trim", vec![Value::string("x")], &mut budget, CancellationToken::never()),
                Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)
            ));
            assert_eq!(budget.collection_bytes(), 3);
        }
    }

    #[test]
    fn allocations_succeed_at_the_exact_limit_and_fail_before_excess_retention() {
        let arguments = vec![
            Value::string("aaa"),
            Value::string("a"),
            Value::string("0123456789"),
        ];
        let total = size_of::<usize>() as u64 + 30;
        let mut budget = ResourceBudget::default().with_collection_bytes(total);
        assert_eq!(
            run(
                "replace",
                arguments.clone(),
                &mut budget,
                CancellationToken::never()
            )
            .unwrap_or_else(|_| panic!("unexpected operation failure")),
            Value::string("012345678901234567890123456789")
        );
        assert_eq!(budget.collection_bytes(), total);
        let mut budget = ResourceBudget::default().with_collection_bytes(total - 1);
        assert!(
            matches!(run("replace", arguments, &mut budget, CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
        );
        assert_eq!(budget.collection_bytes(), size_of::<usize>() as u64 + 20);
    }

    #[derive(Clone)]
    struct PollClock(Arc<AtomicU64>);
    impl Clock for PollClock {
        fn now(&self) -> Instant {
            Instant::from_nanos(self.0.fetch_add(1, Ordering::Relaxed))
        }
    }

    #[test]
    fn long_scans_and_output_growth_cancel_during_work() {
        for (name, arguments) in [
            (
                "contains",
                vec![Value::string("a".repeat(CHUNK * 40)), Value::string("aab")],
            ),
            ("trim", vec![Value::string(" ".repeat(CHUNK * 40))]),
            (
                "replace",
                vec![
                    Value::string("aaa"),
                    Value::string("a"),
                    Value::string("x".repeat(CHUNK * 40)),
                ],
            ),
            ("decode_utf8", vec![Value::bytes(vec![b'a'; CHUNK * 40])]),
            (
                "starts_with",
                vec![
                    Value::string("a".repeat(CHUNK * 40)),
                    Value::string("a".repeat(CHUNK * 40)),
                ],
            ),
            (
                "ends_with",
                vec![
                    Value::string("a".repeat(CHUNK * 40)),
                    Value::string("a".repeat(CHUNK * 40)),
                ],
            ),
            (
                "split",
                vec![Value::string("/".repeat(CHUNK * 40)), Value::string("/")],
            ),
            (
                "join",
                vec![
                    Value::list(vec![Value::string(""); CHUNK * 40]),
                    Value::string(""),
                ],
            ),
        ] {
            let polls = Arc::new(AtomicU64::new(0));
            let token =
                CancellationToken::deadline(PollClock(Arc::clone(&polls)), Instant::from_nanos(10));
            let mut budget = ResourceBudget::default();
            assert!(
                matches!(
                    run(name, arguments, &mut budget, token),
                    Err(Abort::Cancelled(_))
                ),
                "{name} must cancel inside work"
            );
            assert!(polls.load(Ordering::Relaxed) < 20);
        }
    }

    #[test]
    fn repeated_prefix_search_matches_the_literal_reference() {
        let alphabet = ["", "a", "aa", "aaa", "aba", "b", "é", "aé", "終"];
        for first in alphabet {
            for second in alphabet {
                let input = format!("{first}{second}");
                for pattern in alphabet.into_iter().filter(|pattern| !pattern.is_empty()) {
                    let args = vec![Value::string(input.as_str()), Value::string(pattern)];
                    assert_eq!(
                        run(
                            "contains",
                            args.clone(),
                            &mut ResourceBudget::default(),
                            CancellationToken::never()
                        )
                        .unwrap_or_else(|_| panic!("unexpected operation failure")),
                        Value::Bool(input.contains(pattern))
                    );
                    assert_eq!(
                        run(
                            "split",
                            args.clone(),
                            &mut ResourceBudget::default(),
                            CancellationToken::never()
                        )
                        .unwrap_or_else(|_| panic!("unexpected operation failure")),
                        Value::list(input.split(pattern).map(Value::string).collect())
                    );
                    let mut args = args;
                    args.push(Value::string("β"));
                    assert_eq!(
                        run(
                            "replace",
                            args,
                            &mut ResourceBudget::default(),
                            CancellationToken::never()
                        )
                        .unwrap_or_else(|_| panic!("unexpected operation failure")),
                        Value::string(input.replace(pattern, "β"))
                    );
                }
            }
        }
    }
}
