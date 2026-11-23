use crate::Span;
use crate::ast::*;

/// Maximum number of enclosing constructs on a path that includes a tuple.
pub const MAX_TUPLE_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TupleDepthError {
    Cancelled,
    Exceeded(Span),
}

/// Checks the tuple depth of an AST, including trees provided by Rust callers.
pub fn check_tuple_depth(
    script: &Script,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<(), TupleDepthError> {
    let mut walk = Walk::default();
    walk.extend(script.statements().iter().map(Node::Statement));
    walk.run(is_cancelled)
}

#[derive(Clone, Copy)]
enum Node<'a> {
    Statement(&'a Statement),
    Else(&'a ElseBranch),
    Expression(&'a Expression),
    Pattern(&'a Pattern),
    Type(&'a TypeReference),
    Chain(&'a ConditionalChain),
    Closure(&'a Closure),
    WordPart(&'a WordPart),
}

#[derive(Clone, Copy, Default)]
struct Path {
    depth: usize,
    tuple: bool,
    excess: Option<Span>,
}

#[derive(Default)]
struct Walk<'a> {
    pending: Vec<(Node<'a>, Path)>,
    path: Path,
}

impl<'a> Walk<'a> {
    fn push(&mut self, node: Node<'a>) {
        self.pending.push((node, self.path));
    }

    fn extend(&mut self, nodes: impl DoubleEndedIterator<Item = Node<'a>>) {
        self.pending
            .extend(nodes.rev().map(|node| (node, self.path)));
    }

    fn enter(&mut self, span: Span, tuple: bool) -> Result<(), TupleDepthError> {
        self.path.tuple |= tuple;
        self.path.depth += 1;
        if self.path.depth > MAX_TUPLE_DEPTH {
            let first = *self.path.excess.get_or_insert(span);
            if self.path.tuple {
                return Err(TupleDepthError::Exceeded(first));
            }
        }
        Ok(())
    }

    fn run(&mut self, is_cancelled: &dyn Fn() -> bool) -> Result<(), TupleDepthError> {
        let mut visited = 0_usize;
        let mut seen_tuple = false;
        while let Some((node, path)) = self.pending.pop() {
            visited += 1;
            seen_tuple |= match node {
                Node::Expression(expression) => {
                    matches!(expression.kind(), ExpressionKind::Tuple(_))
                }
                Node::Pattern(pattern) => matches!(pattern, Pattern::Tuple(_)),
                Node::Type(reference) => matches!(reference.kind, TypeReferenceKind::Tuple(_)),
                _ => false,
            };
            if (seen_tuple || visited.is_multiple_of(256)) && is_cancelled() {
                return Err(TupleDepthError::Cancelled);
            }
            self.path = path;
            match node {
                Node::Statement(statement) => self.statement(statement.kind()),
                Node::Else(branch) => match branch {
                    ElseBranch::Block(block) => self.block(block),
                    ElseBranch::If(branch) => self.if_statement(branch.kind()),
                },
                Node::Expression(expression) => self.expression(expression)?,
                Node::Pattern(pattern) => self.pattern(pattern)?,
                Node::Type(reference) => self.type_reference(reference)?,
                Node::Chain(chain) => self.chain(chain),
                Node::Closure(closure) => {
                    self.enter(closure.span, false)?;
                    self.push(Node::Chain(&closure.body));
                    self.parameters(&closure.parameters, closure.result_type.as_ref());
                }
                Node::WordPart(part) => match part.kind() {
                    WordPartKind::DoubleQuoted(parts) => {
                        self.extend(parts.iter().map(Node::WordPart));
                    }
                    WordPartKind::Interpolation(expression) => {
                        self.push(Node::Expression(expression));
                    }
                    WordPartKind::Bare
                    | WordPartKind::BareEscape
                    | WordPartKind::SingleQuoted
                    | WordPartKind::DoubleText
                    | WordPartKind::DoubleEscape
                    | WordPartKind::EscapedBrace => {}
                },
            }
        }
        Ok(())
    }

    fn block(&mut self, block: &'a Block) {
        self.extend(block.statements.iter().map(Node::Statement));
    }

    fn parameters(&mut self, parameters: &'a [Parameter], result: Option<&'a TypeReference>) {
        if let Some(result) = result {
            self.push(Node::Type(result));
        }
        for parameter in parameters.iter().rev() {
            if let Some(annotation) = &parameter.type_annotation {
                self.push(Node::Type(annotation));
            }
            self.push(Node::Pattern(&parameter.pattern));
        }
    }

    fn if_statement(&mut self, branch: &'a IfStatement) {
        if let Some(otherwise) = &branch.else_branch {
            self.push(Node::Else(otherwise));
        }
        self.block(&branch.then_block);
        self.push(Node::Chain(&branch.condition));
    }

    fn statement(&mut self, statement: &'a StatementKind) {
        match statement {
            StatementKind::NominalType(declaration) => {
                self.extend(
                    declaration
                        .fields
                        .iter()
                        .map(|field| Node::Type(&field.value_type)),
                );
            }
            StatementKind::VariantType(declaration) => {
                self.extend(
                    declaration
                        .variants
                        .iter()
                        .flat_map(|variant| variant.payload.iter().map(Node::Type)),
                );
            }
            StatementKind::Declaration(declaration) => {
                self.push(Node::Expression(&declaration.value));
                if let Some(annotation) = &declaration.type_annotation {
                    self.push(Node::Type(annotation));
                }
                self.push(Node::Pattern(&declaration.pattern));
            }
            StatementKind::IndexedAssignment(assignment) => {
                self.push(Node::Expression(&assignment.value));
                self.push(Node::Expression(&assignment.index));
            }
            StatementKind::Assignment(Assignment { value, .. })
            | StatementKind::Environment(EnvironmentStatement::Export { value, .. })
            | StatementKind::Throw(value)
            | StatementKind::Control(ControlTransfer::Return(Some(value))) => {
                self.push(Node::Expression(value));
            }
            StatementKind::Function(function) => {
                self.block(&function.body);
                self.parameters(&function.parameters, function.return_type.as_ref());
            }
            StatementKind::Action(action) => {
                self.block(&action.body);
                self.parameters(&action.parameters, Some(&action.return_type));
            }
            StatementKind::If(branch) => self.if_statement(branch),
            StatementKind::While(statement) => {
                self.block(&statement.body);
                self.push(Node::Chain(&statement.condition));
            }
            StatementKind::For(statement) => {
                self.block(&statement.body);
                self.push(Node::Expression(&statement.iterable));
                if !matches!(statement.binding, Pattern::Binding(_)) {
                    self.push(Node::Pattern(&statement.binding));
                }
            }
            StatementKind::Match(statement) => {
                for arm in statement.arms.iter().rev() {
                    self.block(&arm.body);
                    if let Some(guard) = &arm.guard {
                        self.push(Node::Expression(guard));
                    }
                    self.push(Node::Pattern(&arm.pattern));
                }
                self.push(Node::Expression(&statement.value));
            }
            StatementKind::Try(statement) => {
                self.block(&statement.catch_block);
                self.block(&statement.try_block);
            }
            StatementKind::Job(job) => self.push(Node::Chain(&job.chain)),
            StatementKind::ModuleImport(_)
            | StatementKind::ModuleMemberImport(_)
            | StatementKind::ModuleImportGroup(_)
            | StatementKind::ModuleExport(_)
            | StatementKind::Task(_)
            | StatementKind::Environment(EnvironmentStatement::Unset { .. })
            | StatementKind::Control(
                ControlTransfer::Break | ControlTransfer::Continue | ControlTransfer::Return(None),
            ) => {}
        }
    }

    fn expression(&mut self, expression: &'a Expression) -> Result<(), TupleDepthError> {
        let kind = expression.kind();
        let has_children = match kind {
            ExpressionKind::Name(_) | ExpressionKind::Qualified(_) | ExpressionKind::Closure(_) => {
                false
            }
            ExpressionKind::Literal(literal) => matches!(literal.kind(),
                LiteralKind::DoubleQuoted(parts) if parts.iter().any(|part| matches!(part.kind(), WordPartKind::Interpolation(_)))),
            ExpressionKind::List(items) => !items.is_empty(),
            ExpressionKind::Record(entries) => !entries.is_empty(),
            ExpressionKind::NominalRecord(record) => !record.fields.is_empty(),
            _ => true,
        };
        if has_children {
            self.enter(expression.span(), matches!(kind, ExpressionKind::Tuple(_)))?;
        }
        match kind {
            ExpressionKind::Literal(literal) => {
                if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
                    self.extend(parts.iter().map(Node::WordPart));
                }
            }
            ExpressionKind::Name(_) | ExpressionKind::Qualified(_) => {}
            ExpressionKind::List(items) => {
                self.extend(items.iter().map(|item| Node::Expression(item.expression())));
            }
            ExpressionKind::Tuple(tuple) => {
                self.extend(tuple.elements.iter().map(Node::Expression));
            }
            ExpressionKind::Record(entries) => {
                for entry in entries.iter().rev() {
                    self.push(Node::Expression(&entry.value));
                    if let RecordKey::DoubleQuoted(part) = &entry.key {
                        self.push(Node::WordPart(part));
                    }
                }
            }
            ExpressionKind::NominalRecord(record) => {
                self.extend(
                    record
                        .fields
                        .iter()
                        .map(|field| Node::Expression(&field.value)),
                );
            }
            ExpressionKind::Closure(closure) => self.push(Node::Closure(closure)),
            ExpressionKind::GroupedJob(chain) => self.push(Node::Chain(chain)),
            ExpressionKind::If(branch) => {
                self.push(Node::Expression(&branch.fallback));
                for arm in branch.arms.iter().rev() {
                    self.push(Node::Expression(&arm.value));
                    self.push(Node::Chain(&arm.condition));
                }
            }
            ExpressionKind::Call(call) => {
                self.extend(call.arguments.iter().map(Node::Expression));
                self.extend(call.type_arguments.iter().map(Node::Type));
                self.push(Node::Expression(&call.callee));
            }
            ExpressionKind::Index(index) => {
                self.push(Node::Expression(&index.index));
                self.push(Node::Expression(&index.target));
            }
            ExpressionKind::Member(member) => self.push(Node::Expression(&member.target)),
            ExpressionKind::Unary(unary) => self.push(Node::Expression(&unary.operand)),
            ExpressionKind::Binary(binary) => {
                self.push(Node::Expression(&binary.right));
                self.push(Node::Expression(&binary.left));
            }
        }
        Ok(())
    }

    fn pattern(&mut self, pattern: &'a Pattern) -> Result<(), TupleDepthError> {
        match pattern {
            Pattern::Tuple(tuple) => {
                self.enter(tuple.open, true)?;
                self.extend(tuple.elements.iter().map(Node::Pattern));
            }
            Pattern::List(list) if !list.elements.is_empty() => {
                self.enter(list.span, false)?;
                self.extend(list.elements.iter().map(Node::Pattern));
            }
            Pattern::NominalRecord(record) if !record.fields.is_empty() => {
                self.enter(record.span, false)?;
                self.extend(
                    record
                        .fields
                        .iter()
                        .map(|field| Node::Pattern(&field.pattern)),
                );
            }
            Pattern::Variant(variant) if !variant.payload.is_empty() => {
                self.enter(variant.span, false)?;
                self.extend(variant.payload.iter().map(Node::Pattern));
            }
            Pattern::Literal(literal) => {
                if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
                    self.extend(parts.iter().map(Node::WordPart));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn type_reference(&mut self, reference: &'a TypeReference) -> Result<(), TupleDepthError> {
        let (arguments, tuple) = match &reference.kind {
            TypeReferenceKind::Named { arguments, .. } => (arguments, false),
            TypeReferenceKind::Tuple(tuple) => (&tuple.elements, true),
        };
        if tuple || !arguments.is_empty() {
            self.enter(reference.span, tuple)?;
            self.extend(arguments.iter().map(Node::Type));
        }
        Ok(())
    }

    fn chain(&mut self, chain: &'a ConditionalChain) {
        let stages = chain
            .or_terms()
            .iter()
            .flat_map(AndChain::and_terms)
            .flat_map(Pipeline::stages);
        for stage in stages.rev() {
            match stage.kind() {
                StageKind::Expression(expression) => self.push(Node::Expression(expression)),
                StageKind::Command(command) => {
                    for item in command.items.iter().rev() {
                        match item.kind() {
                            CommandItemKind::Closure(closure) => self.push(Node::Closure(closure)),
                            CommandItemKind::Spread(expression) => {
                                self.push(Node::Expression(expression))
                            }
                            CommandItemKind::Word(word) => {
                                self.extend(word.parts().iter().map(Node::WordPart));
                            }
                            CommandItemKind::Redirection(redirection) => match redirection.kind() {
                                RedirectionKind::Input { target, .. }
                                | RedirectionKind::File(FileRedirection { target, .. }) => {
                                    self.extend(target.parts().iter().map(Node::WordPart));
                                }
                                RedirectionKind::Duplicate { .. }
                                | RedirectionKind::Close { .. } => {}
                            },
                        }
                    }
                    self.extend(command.head.word().parts().iter().map(Node::WordPart));
                }
            }
        }
    }
}
