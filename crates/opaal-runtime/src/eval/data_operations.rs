//! Host-free codecs and a strict decoder sharing the caller's live budget.

use std::io::{self, Read};
use std::sync::Arc;

use opaal_syntax::Span;

use super::{Abort, Eval, Evaluator, RuntimeErrorKind};
use crate::operation::{OperationDescriptor, StandardOperation};
use crate::operational::ModuleError;
use crate::{Record, Value};

const MAX_JSON_INPUT: usize = 8 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const CHUNK: usize = 4096;

impl Evaluator<'_, '_> {
    pub(super) fn legacy_data_operation(
        &mut self,
        descriptor: &OperationDescriptor,
        arguments: &[Value],
        span: Span,
    ) -> Eval<Value> {
        let result = match descriptor.implementation() {
            StandardOperation::TomlDecode => match &arguments[0] {
                Value::Bytes(bytes) => crate::data::toml_decode(bytes),
                other => Err(legacy_type_error(0, "Bytes", other)),
            },
            StandardOperation::DataGet => (|| {
                let Value::List(keys) = &arguments[1] else {
                    return Err(legacy_type_error(1, "List[String]", &arguments[1]));
                };
                // Preserve the released conversion, including item error indices.
                let keys = keys
                    .iter()
                    .enumerate()
                    .map(|(index, key)| match key {
                        Value::String(key) => Ok(key.to_string()),
                        other => Err(legacy_type_error(index, "String", other)),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                crate::data::get(
                    &arguments[0],
                    &keys.iter().map(String::as_str).collect::<Vec<_>>(),
                )
                .cloned()
            })(),
            StandardOperation::JsonEncode => {
                crate::data::json_encode(&arguments[0]).map(Value::bytes)
            }
            _ => unreachable!("legacy data descriptor"),
        };
        result.map_err(|error| self.operational_abort(error, span))
    }

    pub(super) fn json_decode(&mut self, arguments: &[Value], span: Span) -> Eval<Value> {
        let [Value::Bytes(input)] = arguments else {
            unreachable!("validated Bytes input")
        };
        self.check_cancel(span)?;
        if input.len() > MAX_JSON_INPUT {
            return Err(self.operational_abort(
                ModuleError::invalid("DATA014", "JSON input exceeds 8 MiB"),
                span,
            ));
        }
        // Validate the entire document without an unbounded UTF8 scan or copy.
        let mut cursor = 0;
        while cursor < input.len() {
            self.check_cancel(span)?;
            self.charge(span)?;
            let end = (cursor + CHUNK).min(input.len());
            match std::str::from_utf8(&input[cursor..end]) {
                Ok(_) => cursor = end,
                Err(error) if error.error_len().is_none() && end < input.len() => {
                    cursor += error.valid_up_to()
                }
                Err(error) => {
                    return Err(self.operational_abort(
                        ModuleError::invalid(
                            "DATA015",
                            format!("invalid JSON UTF8 at byte {}", cursor + error.valid_up_to()),
                        ),
                        span,
                    ));
                }
            }
        }
        let mut parser = JsonDecoder {
            input,
            cursor: 0,
            scan_remaining: 0,
            output_remaining: 0,
            evaluator: self,
            span,
        };
        let value = parser.value(1)?;
        parser.whitespace()?;
        if parser.peek().is_some() {
            return Err(parser.malformed());
        }
        Ok(value)
    }
}

fn legacy_type_error(index: usize, expected: &str, actual: &Value) -> ModuleError {
    ModuleError::invalid(
        "OPERATION001",
        format!(
            "controlled argument {index} requires {expected}, found {}",
            actual.family_name()
        ),
    )
}

struct JsonDecoder<'a, 'b, 'host, 'sources> {
    input: &'a [u8],
    cursor: usize,
    scan_remaining: usize,
    output_remaining: usize,
    evaluator: &'b mut Evaluator<'host, 'sources>,
    span: Span,
}

impl JsonDecoder<'_, '_, '_, '_> {
    fn visit(&mut self) -> Eval<()> {
        self.evaluator.check_cancel(self.span)?;
        self.evaluator.charge(self.span)
    }

    fn failure(&self, code: &'static str, message: impl Into<String>) -> Abort {
        self.evaluator
            .operational_abort(ModuleError::invalid(code, message), self.span)
    }

    fn malformed(&self) -> Abort {
        self.failure("DATA016", format!("invalid JSON at byte {}", self.cursor))
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.cursor).copied()
    }

    fn bump(&mut self) -> Eval<u8> {
        if self.scan_remaining == 0 {
            self.visit()?;
            self.scan_remaining = CHUNK;
        }
        let byte = self.peek().ok_or_else(|| self.malformed())?;
        self.cursor += 1;
        self.scan_remaining -= 1;
        Ok(byte)
    }

    fn expect(&mut self, expected: u8) -> Eval<()> {
        if self.peek() != Some(expected) {
            return Err(self.malformed());
        }
        self.bump()?;
        Ok(())
    }

    fn whitespace(&mut self) -> Eval<()> {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.bump()?;
        }
        Ok(())
    }

    fn retain_item<T>(&mut self) -> Eval<()> {
        self.visit()?;
        self.evaluator
            .retain_collection_bytes(size_of::<T>(), self.span)?;
        if self.evaluator.budget.charge_collection_items(1) {
            Ok(())
        } else {
            Err(self
                .evaluator
                .error(RuntimeErrorKind::ResourceBudgetExceeded, self.span))
        }
    }

    fn value(&mut self, depth: usize) -> Eval<Value> {
        self.visit()?;
        self.whitespace()?;
        if depth > MAX_JSON_DEPTH {
            return Err(self.failure("DATA017", "JSON nesting exceeds 64"));
        }
        match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(Value::Null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(Value::Bool(false))
            }
            Some(b'"') => self.string().map(Value::string),
            Some(b'[') => self.container(depth, false),
            Some(b'{') => self.container(depth, true),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.malformed()),
        }
    }

    fn literal(&mut self, expected: &[u8]) -> Eval<()> {
        for byte in expected {
            self.expect(*byte)?;
        }
        Ok(())
    }

    fn container(&mut self, depth: usize, object: bool) -> Eval<Value> {
        // Nested data traversal shares the live depth of its invoking callback
        // or function, as well as the decoder's independent depth64 ceiling.
        if !self.evaluator.budget.enter_call() {
            return Err(self
                .evaluator
                .error(RuntimeErrorKind::ResourceBudgetExceeded, self.span));
        }
        let result = if object {
            self.object(depth)
        } else {
            self.array(depth)
        };
        self.evaluator.budget.leave_call();
        result
    }

    fn array(&mut self, depth: usize) -> Eval<Value> {
        self.expect(b'[')?;
        self.whitespace()?;
        let mut items = Vec::new();
        if self.peek() != Some(b']') {
            loop {
                // Charge the slot before constructing or retaining its value.
                self.retain_item::<Value>()?;
                items.push(self.value(depth + 1)?);
                self.whitespace()?;
                if self.peek() != Some(b',') {
                    break;
                }
                self.bump()?;
            }
        }
        self.expect(b']')?;
        Ok(Value::list(items))
    }

    fn object(&mut self, depth: usize) -> Eval<Value> {
        self.expect(b'{')?;
        self.whitespace()?;
        let mut entries: Vec<(Arc<str>, Value)> = Vec::new();
        if self.peek() != Some(b'}') {
            loop {
                self.retain_item::<(Arc<str>, Value)>()?;
                self.whitespace()?;
                let key = self.string()?;
                self.whitespace()?;
                self.expect(b':')?;
                let value = self.value(depth + 1)?;
                entries.push((Arc::from(key), value));
                self.whitespace()?;
                if self.peek() != Some(b',') {
                    break;
                }
                self.bump()?;
            }
        }
        self.expect(b'}')?;
        // Share the bounded key-work/index machinery, avoiding Record::new's
        // linear duplicate scan per field. Output retains document order.
        let indices = self.evaluator.sorted_record_indices(
            entries.len(),
            |index| entries[index].0.as_ref(),
            self.span,
        )?;
        for pair in indices.windows(2) {
            if self
                .evaluator
                .record_compare(&entries[pair[0]].0, &entries[pair[1]].0, self.span)?
                .is_eq()
            {
                let key = &entries[pair[0]].0;
                let mut excerpt: String = key
                    .chars()
                    .take(32)
                    .flat_map(char::escape_default)
                    .collect();
                if key.chars().nth(32).is_some() {
                    excerpt.push('…');
                }
                return Err(
                    self.failure("DATA018", format!("JSON object repeated key \"{excerpt}\""))
                );
            }
        }
        Ok(Value::Record(Record::from_unique_entries(entries)))
    }

    fn push_char(&mut self, output: &mut String, character: char) -> Eval<()> {
        let bytes = character.len_utf8();
        if self.output_remaining < bytes {
            self.visit()?;
            self.output_remaining = CHUNK;
        }
        self.output_remaining -= bytes;
        self.evaluator.retain_collection_bytes(bytes, self.span)?;
        output.push(character);
        Ok(())
    }

    fn string(&mut self) -> Eval<String> {
        self.expect(b'"')?;
        let mut output = String::new();
        loop {
            let start = self.cursor;
            let byte = self.bump()?;
            let character = match byte {
                b'"' => return Ok(output),
                0..=0x1f => return Err(self.malformed()),
                b'\\' => match self.bump()? {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let first = self.hex4()?;
                        let scalar = if (0xd800..=0xdbff).contains(&first) {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            let second = self.hex4()?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return Err(self.malformed());
                            }
                            0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                        } else {
                            first
                        };
                        char::from_u32(scalar).ok_or_else(|| self.malformed())?
                    }
                    _ => return Err(self.malformed()),
                },
                0x20..=0x7f => char::from(byte),
                _ => {
                    // UTF8 was validated above. Convert only this scalar,
                    // polling across chunk boundaries while consuming bytes.
                    let width = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => unreachable!("validated UTF8"),
                    };
                    for _ in 1..width {
                        self.bump()?;
                    }
                    std::str::from_utf8(&self.input[start..self.cursor])
                        .expect("validated scalar")
                        .chars()
                        .next()
                        .expect("one scalar")
                }
            };
            self.push_char(&mut output, character)?;
        }
    }

    fn hex4(&mut self) -> Eval<u32> {
        let mut scalar = 0;
        for _ in 0..4 {
            let digit = self.bump()?;
            let value = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return Err(self.malformed()),
            };
            scalar = scalar * 16 + u32::from(value);
        }
        Ok(scalar)
    }

    fn number(&mut self) -> Eval<Value> {
        let start = self.cursor;
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
        ) {
            self.bump()?;
        }
        // serde owns syntax and numeric rounding. Its second scan also polls
        // and charges per chunk, even for a hostile multi-megabyte token.
        let mut abort = None;
        let reader = NumberReader {
            input: &self.input[start..self.cursor],
            cursor: 0,
            evaluator: self.evaluator,
            span: self.span,
            abort: &mut abort,
        };
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        let result = crate::format::json_number(&mut deserializer).and_then(|value| {
            deserializer.end()?;
            Ok(value)
        });
        if let Some(abort) = abort {
            return Err(abort);
        }
        result.map_err(|_| self.failure("DATA016", format!("invalid JSON number at byte {start}")))
    }
}

struct NumberReader<'a, 'b, 'host, 'sources> {
    input: &'a [u8],
    cursor: usize,
    evaluator: &'b mut Evaluator<'host, 'sources>,
    span: Span,
    abort: &'b mut Option<Abort>,
}

impl Read for NumberReader<'_, '_, '_, '_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.cursor == self.input.len() || output.is_empty() {
            return Ok(0);
        }
        if self.cursor.is_multiple_of(CHUNK) {
            let result = self
                .evaluator
                .check_cancel(self.span)
                .and_then(|()| self.evaluator.charge(self.span));
            if let Err(abort) = result {
                *self.abort = Some(abort);
                return Err(io::Error::other("JSON number evaluation stopped"));
            }
        }
        let count = output
            .len()
            .min(CHUNK - self.cursor % CHUNK)
            .min(self.input.len() - self.cursor);
        output[..count].copy_from_slice(&self.input[self.cursor..self.cursor + count]);
        self.cursor += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CancellationToken, EvaluationPolicy, PureEvaluationHost, ResourceBudget};
    use super::*;
    use crate::module::{ModuleAliasRegistry, ModuleId, NominalTypeId, RuntimeBindingTypes};
    use crate::operation::standard_operation;
    use crate::{Environment, NominalRecordValue, ScopeStack};
    use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run(
        name: &str,
        input: Value,
        budget: &mut ResourceBudget,
        cancel: CancellationToken,
    ) -> Eval<Value> {
        let source = Arc::new(SourceFile::new(SourceId::new(1), "data.opaal", "call"));
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
            standard_effects_allowed: true,
            cancel,
            budget,
            host: &mut host,
        };
        let operation = standard_operation(&ModuleId::standard("std", "data"), name).unwrap();
        let arguments = if name == "get" {
            vec![input, Value::list(vec![])]
        } else {
            vec![input]
        };
        evaluator.execute_operation(
            &operation,
            arguments,
            &[],
            evaluator.source.span(0..4).unwrap(),
        )
    }

    fn decode(
        input: Vec<u8>,
        budget: &mut ResourceBudget,
        cancel: CancellationToken,
    ) -> Eval<Value> {
        run("json_decode", Value::bytes(input), budget, cancel)
    }

    fn is_budget_error(result: Eval<Value>) -> bool {
        matches!(result, Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
    }

    fn error_message(result: Eval<Value>) -> String {
        match result {
            Err(Abort::Error(error)) => error.to_string(),
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn json_input_and_depth_limits_are_inclusive_and_independent_of_stream_limits() {
        for length in [MAX_JSON_INPUT - 1, MAX_JSON_INPUT, MAX_JSON_INPUT + 1] {
            let mut input = vec![b' '; length];
            input[..4].copy_from_slice(b"null");
            let result = decode(
                input,
                &mut ResourceBudget::opaal(),
                CancellationToken::never(),
            );
            if length <= MAX_JSON_INPUT {
                assert!(matches!(result, Ok(Value::Null)));
            } else {
                assert!(error_message(result).contains("DATA014"));
            }
        }
        for containers in [62, 63, 64] {
            let input =
                format!("{}0{}", "[".repeat(containers), "]".repeat(containers)).into_bytes();
            let mut chunks = vec![input.clone()].into_iter();
            assert!(matches!(
                crate::format::from_json(
                    crate::format::JsonMode::Document,
                    Box::new(move || chunks.next()),
                    MAX_JSON_INPUT
                )
                .pull(),
                crate::format::FromJsonStep::Value(_)
            ));
            let result = decode(
                input,
                &mut ResourceBudget::opaal(),
                CancellationToken::never(),
            );
            if containers < MAX_JSON_DEPTH {
                assert!(result.is_ok());
            } else {
                assert!(error_message(result).contains("DATA017"));
            }
        }
        let empty_at_limit = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(
            decode(
                empty_at_limit.into_bytes(),
                &mut ResourceBudget::opaal(),
                CancellationToken::never()
            )
            .is_ok()
        );
    }

    #[test]
    fn json_nesting_shares_live_depth_and_unwinds_on_every_outcome() {
        for input in [b"[[0]]".as_slice(), br#"{"x":[0]}"#, br#"[{"x":0}]"#] {
            let mut short = ResourceBudget::opaal().with_call_depth(1);
            assert!(is_budget_error(decode(
                input.to_vec(),
                &mut short,
                CancellationToken::never()
            )));
            assert_eq!(short.call_depth, 0);

            let mut exact = ResourceBudget::opaal().with_call_depth(2);
            assert!(decode(input.to_vec(), &mut exact, CancellationToken::never()).is_ok());
            assert_eq!(exact.peak_call_depth(), 2);
            assert_eq!(exact.call_depth, 0);

            let mut caller = ResourceBudget::opaal().with_call_depth(2);
            assert!(caller.enter_call());
            assert!(is_budget_error(decode(
                input.to_vec(),
                &mut caller,
                CancellationToken::never()
            )));
            assert_eq!(caller.call_depth, 1);
            caller.leave_call();
        }
        for input in [b"[[".as_slice(), br#"[{"a":0,"a":1}]"#] {
            let mut budget = ResourceBudget::opaal().with_call_depth(2);
            assert!(matches!(
                decode(input.to_vec(), &mut budget, CancellationToken::never()),
                Err(Abort::Error(_))
            ));
            assert_eq!(budget.call_depth, 0);
            assert_eq!(budget.peak_call_depth(), 2);
        }
        for stop in 0..32 {
            let count = Arc::new(AtomicUsize::new(0));
            let token =
                CancellationToken::from_fn(move || count.fetch_add(1, Ordering::SeqCst) >= stop);
            let mut budget = ResourceBudget::opaal().with_call_depth(2);
            let _ = decode(b"[[0]]".to_vec(), &mut budget, token);
            assert_eq!(budget.call_depth, 0, "cancel poll {stop}");
        }
        let mut scalar = ResourceBudget::opaal().with_call_depth(0);
        assert!(decode(b"0".to_vec(), &mut scalar, CancellationToken::never()).is_ok());
        assert!(is_budget_error(decode(
            b"[]".to_vec(),
            &mut scalar,
            CancellationToken::never()
        )));
    }

    #[test]
    fn retained_strings_elements_keys_and_duplicate_workspace_charge_before_growth() {
        for (input, items, bytes) in [
            (b"[1,2]".to_vec(), 2, 2 * size_of::<Value>()),
            (br#""a\u00e9""#.to_vec(), 0, 3),
            (
                br#"{"z":1,"a":"x"}"#.to_vec(),
                2,
                2 * size_of::<(Arc<str>, Value)>() + 3 + 4 * size_of::<usize>(),
            ),
        ] {
            let mut exact = ResourceBudget::opaal()
                .with_collection_items(items)
                .with_collection_bytes(bytes as u64);
            assert!(decode(input.clone(), &mut exact, CancellationToken::never()).is_ok());
            assert_eq!(exact.collection_items, items);
            assert_eq!(exact.collection_bytes, bytes as u64);
            if items > 0 {
                assert!(is_budget_error(decode(
                    input.clone(),
                    &mut ResourceBudget::opaal().with_collection_items(items - 1),
                    CancellationToken::never()
                )));
            }
            assert!(is_budget_error(decode(
                input.clone(),
                &mut ResourceBudget::opaal().with_collection_bytes(bytes as u64 - 1),
                CancellationToken::never()
            )));
        }
        let input = br#"["a","b"]"#.to_vec();
        let mut budget = ResourceBudget::opaal().with_collection_items(3);
        assert!(decode(input.clone(), &mut budget, CancellationToken::never()).is_ok());
        assert!(is_budget_error(decode(
            input,
            &mut budget,
            CancellationToken::never()
        )));
        assert_eq!(budget.collection_items, 3);
    }

    #[test]
    fn every_observed_poll_can_cancel_validation_parse_output_numbers_and_key_sort() {
        let key = "é".repeat(CHUNK);
        for input in [
            format!("\"{}終\"", "a".repeat(CHUNK * 2)).into_bytes(),
            format!("0.{}1", "0".repeat(CHUNK * 2)).into_bytes(),
            format!("{{\"{key}b\": [1, 2], \"{key}a\": true, \"short\": null}}").into_bytes(),
            format!("{{\"{key}\":1,\"{key}\":2}}").into_bytes(),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&polls);
            let token = CancellationToken::from_fn(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                false
            });
            let _ = decode(input.clone(), &mut ResourceBudget::opaal(), token);
            let total = polls.load(Ordering::SeqCst);
            for stop in 0..total {
                let count = Arc::new(AtomicUsize::new(0));
                let token = CancellationToken::from_fn(move || {
                    count.fetch_add(1, Ordering::SeqCst) >= stop
                });
                assert!(
                    matches!(
                        decode(input.clone(), &mut ResourceBudget::opaal(), token),
                        Err(Abort::Cancelled(_))
                    ),
                    "stop {stop} of {total}"
                );
            }
        }
    }

    #[test]
    fn utf8_chunk_edges_and_long_numeric_tokens_keep_shared_rules_and_bounded_steps() {
        for width in ["é", "終", "😀"] {
            for offset in 0..width.len() {
                let text = format!("\"{}{width}\"", "a".repeat(CHUNK - 1 - offset));
                let expected = Value::string(&text[1..text.len() - 1]);
                assert_eq!(
                    decode(
                        text.into_bytes(),
                        &mut ResourceBudget::opaal(),
                        CancellationToken::never()
                    )
                    .ok(),
                    Some(expected)
                );
            }
        }
        let number = format!("0.{}1", "0".repeat(CHUNK * 2));
        let mut budget = ResourceBudget::opaal();
        assert!(
            decode(
                number.as_bytes().to_vec(),
                &mut budget,
                CancellationToken::never()
            )
            .is_ok()
        );
        let steps = budget.used_steps;
        assert!(steps >= 9); // Three chunks each for UTF8, token scan and serde.
        assert!(
            decode(
                number.as_bytes().to_vec(),
                &mut ResourceBudget::steps(steps),
                CancellationToken::never()
            )
            .is_ok()
        );
        assert!(is_budget_error(decode(
            number.into_bytes(),
            &mut ResourceBudget::steps(steps - 1),
            CancellationToken::never()
        )));
        let mut invalid = vec![b' '; CHUNK - 1];
        invalid.extend_from_slice(&[0xf0, 0x9f, 0x98]);
        assert!(
            error_message(decode(
                invalid,
                &mut ResourceBudget::opaal(),
                CancellationToken::never()
            ))
            .contains(&format!("byte {}", CHUNK - 1))
        );
    }

    #[test]
    fn released_calls_keep_per_call_limits_and_do_not_charge_legacy_results() {
        let input = Value::string("x".repeat(crate::data::MAX_DATA_BYTES - 2));
        let mut budget = ResourceBudget::steps(0)
            .with_collection_items(0)
            .with_collection_bytes(0);
        for _ in 0..3 {
            assert!(
                matches!(run("json_encode", input.clone(), &mut budget, CancellationToken::never()), Ok(Value::Bytes(bytes)) if bytes.len() == crate::data::MAX_DATA_BYTES)
            );
            assert!(matches!(
                run(
                    "get",
                    input.clone(),
                    &mut budget,
                    CancellationToken::never()
                ),
                Ok(Value::String(_))
            ));
        }
        let mut toml = b"a=1\n".to_vec();
        toml.resize(crate::data::MAX_DATA_BYTES, b' ');
        for _ in 0..2 {
            assert!(
                run(
                    "toml_decode",
                    Value::bytes(toml.clone()),
                    &mut budget,
                    CancellationToken::never()
                )
                .is_ok()
            );
        }
        toml.push(b' ');
        assert!(
            error_message(run(
                "toml_decode",
                Value::bytes(toml),
                &mut budget,
                CancellationToken::never()
            ))
            .contains("DATA001")
        );
        assert!(
            error_message(run(
                "json_encode",
                Value::string("x".repeat(crate::data::MAX_DATA_BYTES - 1)),
                &mut budget,
                CancellationToken::never()
            ))
            .contains("DATA010")
        );
        assert_eq!(
            (
                budget.used_steps,
                budget.collection_items,
                budget.collection_bytes
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn nested_secret_and_project_carriers_cannot_be_encoded_through_pure_dispatch() {
        for id in [
            NominalTypeId::standard("http", "SecretHeader"),
            NominalTypeId::project("secrets", "SecretIdentity"),
            NominalTypeId::project("tools", "ToolIdentity"),
        ] {
            let secret = Value::NominalRecord(Box::new(NominalRecordValue::new(
                id,
                vec![],
                vec![(Arc::from("_value"), Value::string("sensitive"))],
            )));
            let input = Value::list(vec![
                Record::new(vec![("nested".into(), secret)]).unwrap().into(),
            ]);
            let error = error_message(run(
                "json_encode",
                input,
                &mut ResourceBudget::opaal(),
                CancellationToken::never(),
            ));
            assert!(error.contains("DATA013") && !error.contains("sensitive"));
        }
    }

    #[test]
    fn deterministic_json_corpus_and_mutations_match_the_stream_parser() {
        let mut seed = 0x59b3_u64;
        for index in 0..512 {
            let mut text = String::new();
            for _ in 0..32 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push(char::from_u32((seed >> 32) as u32 % 0x110000).unwrap_or('\0'));
            }
            let input = serde_json::to_vec(&serde_json::json!({
                "z": [text, index, index as f64 / 7.0, true, null],
                "a": {"nested": text},
            }))
            .unwrap();
            let mut mutated = input.clone();
            let position = index % mutated.len();
            mutated[position] = (seed >> 16) as u8;
            for bytes in [input, mutated] {
                let mut chunks = vec![bytes.clone()].into_iter();
                let expected = crate::format::from_json(
                    crate::format::JsonMode::Document,
                    Box::new(move || chunks.next()),
                    MAX_JSON_INPUT,
                )
                .pull();
                let actual = decode(
                    bytes.clone(),
                    &mut ResourceBudget::opaal(),
                    CancellationToken::never(),
                );
                match expected {
                    crate::format::FromJsonStep::Value(expected) => assert_eq!(
                        actual.unwrap_or_else(|_| panic!("decoder rejected {bytes:?}")),
                        expected
                    ),
                    crate::format::FromJsonStep::Malformed { .. }
                    | crate::format::FromJsonStep::DuplicateKey { .. } => assert!(
                        matches!(actual, Err(Abort::Error(_))),
                        "decoder accepted {bytes:?}"
                    ),
                    other => panic!("unexpected corpus result: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn qualified_data_calls_never_invoke_an_authority_host_or_journal() {
        use super::super::{
            EvalLimits, EvaluationHost, HostedEvaluationOutcome, evaluate_with_host,
        };
        struct Host {
            environment: Environment,
            policy: EvaluationPolicy,
        }
        impl EvaluationHost for Host {
            fn environment(&mut self) -> &mut Environment {
                &mut self.environment
            }
            fn current_status(&self) -> Option<&crate::Status> {
                None
            }
            fn policy(&self) -> EvaluationPolicy {
                self.policy
            }
            fn invoke_operational(
                &mut self,
                _: &ModuleId,
                _: &str,
                _: Vec<Value>,
                _: &mut ResourceBudget,
            ) -> Option<Result<Value, ModuleError>> {
                panic!("pure data must not reach authority dispatch")
            }
            fn action_start(
                &mut self,
                _: &crate::module::ActionId,
            ) -> Option<Result<(), ModuleError>> {
                panic!("pure data must not create journal actions")
            }
            fn read_directory(
                &mut self,
                _: &std::path::Path,
            ) -> Result<Box<dyn opaal_platform::DirectoryStream>, RuntimeErrorKind> {
                panic!("pure data must not read the host")
            }
            fn execute_chain(
                &mut self,
                _: &opaal_syntax::ConditionalChain,
                _: &mut ScopeStack,
                _: super::super::EvaluationContext,
            ) -> Result<crate::Status, Abort> {
                panic!("pure data must not spawn")
            }
        }
        let source = Arc::new(SourceFile::new(
            SourceId::new(1),
            "data.opaal",
            "import std::data as data\nlet toml = data::toml_decode(input)\nlet selected = data::get(toml, ['a'])\ndata::json_decode(data::json_encode(selected))",
        ));
        let ParseOutcome::Complete(script) = parse_opaal(&source) else {
            panic!("fixture must parse")
        };
        let bindings = RuntimeBindingTypes::analyze_repl_source(
            &source,
            &script,
            &ModuleAliasRegistry::default(),
        )
        .unwrap();
        for policy in [
            EvaluationPolicy::PureOpaal,
            EvaluationPolicy::AmbientProcess,
            EvaluationPolicy::ControlledAction,
        ] {
            let mut scope = ScopeStack::new();
            scope
                .declare(
                    "input",
                    crate::BindingMutability::Immutable,
                    Value::bytes(b"a=7".to_vec()),
                )
                .unwrap();
            let result = evaluate_with_host(
                &script,
                Arc::clone(&source),
                &mut scope,
                &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::opaal()),
                Arc::new(bindings.clone()),
                &mut Host {
                    environment: Environment::new(),
                    policy,
                },
            );
            assert!(matches!(
                result,
                Ok(HostedEvaluationOutcome::Value(Value::Int(7)))
            ));
        }
    }
}
