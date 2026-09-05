use std::fmt;

use crate::Span;

/// A syntax value paired with the exact source range that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstNode<T> {
    kind: T,
    span: Span,
}

impl<T> AstNode<T> {
    #[must_use]
    pub const fn new(kind: T, span: Span) -> Self {
        Self { kind, span }
    }

    #[must_use]
    pub const fn kind(&self) -> &T {
        &self.kind
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    #[must_use]
    pub fn into_kind(self) -> T {
        self.kind
    }
}

/// A complete source file after parsing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
    statements: Vec<Statement>,
    span: Span,
}

impl Script {
    #[must_use]
    pub const fn new(statements: Vec<Statement>, span: Span) -> Self {
        Self { statements, span }
    }

    #[must_use]
    pub fn statements(&self) -> &[Statement] {
        &self.statements
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// An identifier spelling. Its text remains in the originating source file.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Identifier {
    span: Span,
}

impl Identifier {
    #[must_use]
    pub const fn new(span: Span) -> Self {
        Self { span }
    }

    #[must_use]
    pub const fn span(self) -> Span {
        self.span
    }
}

pub type Statement = AstNode<StatementKind>;

/// The statement forms ratified for the initial grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatementKind {
    Import(ImportStatement),
    ModuleImport(ModuleAliasImport),
    ModuleExport(ModuleExportStatement),
    NominalType(NominalTypeDeclaration),
    VariantType(VariantTypeDeclaration),
    Declaration(Declaration),
    Assignment(Assignment),
    Environment(EnvironmentStatement),
    Function(FunctionDefinition),
    If(IfStatement),
    While(WhileStatement),
    For(ForStatement),
    Match(MatchStatement),
    Try(TryStatement),
    Throw(Expression),
    Control(ControlTransfer),
    Job(JobStatement),
}

/// One static top-level source dependency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportStatement {
    /// Explicit imported names. Empty only for a load-only import.
    pub names: Vec<Identifier>,
    /// The complete nonempty single-quoted path literal.
    pub path: Span,
}

/// One Flash 2 module imported under a required local alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleAliasImport {
    pub source: ModuleImportSource,
    pub alias: Identifier,
}

/// The closed module origins constructible by the Flash 2 foundation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleImportSource {
    /// A statically named source path resolved through the injected canonicalizer.
    Local { path: Span },
    /// A compiled standard module descriptor; it is never source-loaded.
    Standard {
        namespace: Identifier,
        module: Identifier,
        span: Span,
    },
}

impl ModuleImportSource {
    #[must_use]
    pub const fn span(self) -> Span {
        match self {
            Self::Local { path } | Self::Standard { span: path, .. } => path,
        }
    }
}

/// One explicit module export list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleExportStatement {
    pub names: Vec<Identifier>,
}

/// The first nominal Flash 2 record type form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalTypeDeclaration {
    pub name: Identifier,
    pub type_parameters: Vec<TypeParameter>,
    pub fields: Vec<NominalTypeField>,
}

/// One closed nominal Flash 2 variant type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VariantTypeDeclaration {
    pub name: Identifier,
    pub type_parameters: Vec<TypeParameter>,
    pub variants: Vec<VariantDeclaration>,
}

/// One constructor in a closed nominal variant declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VariantDeclaration {
    pub name: Identifier,
    pub payload: Vec<TypeReference>,
    pub span: Span,
}

/// One invariant generic parameter and its closed constraint set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeParameter {
    pub name: Identifier,
    pub constraints: Vec<TypeConstraint>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TypeConstraint {
    Equal,
    Ordered,
}

/// One exact field in a nominal record declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalTypeField {
    pub name: Identifier,
    pub value_type: TypeReference,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Declaration {
    pub mutable: bool,
    /// The complete binding or destructuring pattern.
    pub pattern: Pattern,
    /// The first binding retained for frozen-v1 observers.
    pub name: Identifier,
    pub type_annotation: Option<TypeReference>,
    pub value: Expression,
}

impl fmt::Debug for Declaration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Declaration");
        debug.field("mutable", &self.mutable);
        if !matches!(self.pattern, Pattern::Binding(identifier) if identifier == self.name) {
            debug.field("pattern", &self.pattern);
        }
        debug
            .field("name", &self.name)
            .field("type_annotation", &self.type_annotation)
            .field("value", &self.value)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Assignment {
    pub target: VariableReference,
    pub value: Expression,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnvironmentStatement {
    Export { name: Identifier, value: Expression },
    Unset { name: Identifier },
}

#[derive(Clone, Eq, PartialEq)]
pub struct FunctionDefinition {
    pub documentation: Option<DocumentationBlock>,
    pub name: Identifier,
    pub type_parameters: Vec<TypeParameter>,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<TypeReference>,
    pub body: Block,
}

impl fmt::Debug for FunctionDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("FunctionDefinition");
        debug
            .field("documentation", &self.documentation)
            .field("name", &self.name);
        if !self.type_parameters.is_empty() {
            debug.field("type_parameters", &self.type_parameters);
        }
        debug
            .field("parameters", &self.parameters)
            .field("return_type", &self.return_type)
            .field("body", &self.body)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentationBlock {
    pub lines: Vec<Span>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Parameter {
    pub pattern: Pattern,
    /// The first binding retained for frozen-v1 observers.
    pub name: Identifier,
    pub type_annotation: Option<TypeReference>,
    pub span: Span,
}

impl fmt::Debug for Parameter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Parameter");
        if !matches!(self.pattern, Pattern::Binding(identifier) if identifier == self.name) {
            debug.field("pattern", &self.pattern);
        }
        debug
            .field("name", &self.name)
            .field("type_annotation", &self.type_annotation)
            .field("span", &self.span)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeReference {
    pub name: Identifier,
    pub arguments: Vec<Self>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub statements: Vec<Statement>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IfStatement {
    pub condition: ConditionalChain,
    pub then_block: Block,
    pub else_branch: Option<ElseBranch>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElseBranch {
    Block(Block),
    If(Box<AstNode<IfStatement>>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhileStatement {
    pub condition: ConditionalChain,
    pub body: Block,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForStatement {
    pub binding: Identifier,
    pub iterable: Expression,
    pub body: Block,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchStatement {
    pub value: Expression,
    pub arms: Vec<MatchArm>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expression>,
    pub body: Block,
    pub span: Span,
}

/// One runtime-error handler. The catch binding is immutable and visible only
/// while evaluating `catch_block`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TryStatement {
    pub try_block: Block,
    pub catch_binding: Identifier,
    pub catch_block: Block,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Pattern {
    Wildcard(Span),
    Literal(Literal),
    Binding(Identifier),
    List(ListPattern),
    NominalRecord(NominalRecordPattern),
    Variant(VariantPattern),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListPattern {
    pub elements: Vec<Pattern>,
    pub rest: Option<Identifier>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalRecordPattern {
    pub name: QualifiedName,
    pub fields: Vec<PatternField>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternField {
    pub name: Identifier,
    pub pattern: Pattern,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VariantPattern {
    pub constructor: QualifiedName,
    pub payload: Vec<Pattern>,
    pub span: Span,
}

/// A statically qualified `name` or `name::member` path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QualifiedName {
    pub segments: Vec<Identifier>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlTransfer {
    Break,
    Continue,
    Return(Option<Expression>),
}

/// A conditional chain and its optional statement-level background marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobStatement {
    pub chain: ConditionalChain,
    pub background_span: Option<Span>,
}

/// One `$name` reference, including both its complete and name-only spans.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VariableReference {
    pub name: Identifier,
    pub span: Span,
}

pub type Expression = AstNode<ExpressionKind>;

/// The exact value family produced by one command substitution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandCaptureKind {
    Text,
    Bytes,
}

/// One command substitution and its explicit or shorthand capture contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSubstitution {
    capture: CommandCaptureKind,
    modifier_span: Option<Span>,
    chain: Box<ConditionalChain>,
}

impl CommandSubstitution {
    #[must_use]
    pub const fn new(
        capture: CommandCaptureKind,
        modifier_span: Option<Span>,
        chain: Box<ConditionalChain>,
    ) -> Self {
        Self {
            capture,
            modifier_span,
            chain,
        }
    }

    #[must_use]
    pub const fn capture(&self) -> CommandCaptureKind {
        self.capture
    }

    #[must_use]
    pub const fn modifier_span(&self) -> Option<Span> {
        self.modifier_span
    }

    #[must_use]
    pub const fn chain(&self) -> &ConditionalChain {
        &self.chain
    }
}

/// Expression syntax before name resolution or evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExpressionKind {
    Literal(Literal),
    Variable(VariableReference),
    Symbol(Identifier),
    Qualified(QualifiedName),
    List(Vec<Expression>),
    Record(Vec<RecordEntry>),
    NominalRecord(NominalRecordExpression),
    Closure(Closure),
    CommandSubstitution(CommandSubstitution),
    GroupedJob(Box<ConditionalChain>),
    Call(CallExpression),
    Index(IndexExpression),
    Member(MemberExpression),
    Unary(UnaryExpression),
    Binary(BinaryExpression),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Literal {
    kind: LiteralKind,
    span: Span,
}

impl Literal {
    #[must_use]
    pub const fn new(kind: LiteralKind, span: Span) -> Self {
        Self { kind, span }
    }

    #[must_use]
    pub const fn kind(&self) -> &LiteralKind {
        &self.kind
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Literal source forms. Numeric decoding is deliberately left to evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiteralKind {
    Null,
    Boolean(bool),
    Integer,
    Float,
    SingleQuoted,
    DoubleQuoted(Vec<WordPart>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordEntry {
    pub key: RecordKey,
    pub value: Expression,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalRecordExpression {
    pub name: QualifiedName,
    pub fields: Vec<NominalRecordFieldExpression>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalRecordFieldExpression {
    pub name: Identifier,
    pub value: Expression,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordKey {
    Identifier(Identifier),
    SingleQuoted(Span),
    DoubleQuoted(WordPart),
}

#[derive(Clone, Eq, PartialEq)]
pub struct Closure {
    pub parameters: Vec<Parameter>,
    pub result_type: Option<TypeReference>,
    pub body: Box<ConditionalChain>,
    pub span: Span,
}

impl fmt::Debug for Closure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Closure");
        debug.field("parameters", &self.parameters);
        if self.result_type.is_some() {
            debug.field("result_type", &self.result_type);
        }
        debug
            .field("body", &self.body)
            .field("span", &self.span)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CallExpression {
    pub callee: Box<Expression>,
    pub type_arguments: Vec<TypeReference>,
    pub arguments: Vec<Expression>,
}

impl fmt::Debug for CallExpression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("CallExpression");
        debug.field("callee", &self.callee);
        if !self.type_arguments.is_empty() {
            debug.field("type_arguments", &self.type_arguments);
        }
        debug.field("arguments", &self.arguments).finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexExpression {
    pub target: Box<Expression>,
    pub index: Box<Expression>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberExpression {
    pub target: Box<Expression>,
    pub member: Identifier,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnaryExpression {
    pub operator: AstNode<UnaryOperator>,
    pub operand: Box<Expression>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnaryOperator {
    Not,
    Positive,
    Negative,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinaryExpression {
    pub left: Box<Expression>,
    pub operator: AstNode<BinaryOperator>,
    pub right: Box<Expression>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BinaryOperator {
    Multiply,
    Divide,
    Remainder,
    Add,
    Subtract,
    Range,
    RangeInclusive,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    Equal,
    NotEqual,
}

/// An `||` chain, whose terms are `&&` chains in source order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalChain {
    or_terms: Vec<AndChain>,
    operators: Vec<AstNode<ConditionalOperator>>,
    span: Span,
}

impl ConditionalChain {
    #[must_use]
    pub fn new(
        or_terms: Vec<AndChain>,
        operators: Vec<AstNode<ConditionalOperator>>,
        span: Span,
    ) -> Self {
        Self {
            or_terms,
            operators,
            span,
        }
    }

    #[must_use]
    pub fn from_pipeline(pipeline: Pipeline) -> Self {
        let span = pipeline.span();
        Self {
            or_terms: vec![AndChain::from_pipeline(pipeline)],
            operators: Vec::new(),
            span,
        }
    }

    #[must_use]
    pub fn or_terms(&self) -> &[AndChain] {
        &self.or_terms
    }

    #[must_use]
    pub fn operators(&self) -> &[AstNode<ConditionalOperator>] {
        &self.operators
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConditionalOperator {
    Or,
}

/// One `&&` chain, whose terms are pipelines in source order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AndChain {
    and_terms: Vec<Pipeline>,
    operators: Vec<AstNode<AndOperator>>,
    span: Span,
}

impl AndChain {
    #[must_use]
    pub fn new(and_terms: Vec<Pipeline>, operators: Vec<AstNode<AndOperator>>, span: Span) -> Self {
        Self {
            and_terms,
            operators,
            span,
        }
    }

    #[must_use]
    pub fn from_pipeline(pipeline: Pipeline) -> Self {
        let span = pipeline.span();
        Self {
            and_terms: vec![pipeline],
            operators: Vec::new(),
            span,
        }
    }

    #[must_use]
    pub fn and_terms(&self) -> &[Pipeline] {
        &self.and_terms
    }

    #[must_use]
    pub fn operators(&self) -> &[AstNode<AndOperator>] {
        &self.operators
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AndOperator {
    And,
}

/// Pipeline stages and the operators between adjacent stages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    stages: Vec<Stage>,
    operators: Vec<AstNode<PipeOperator>>,
    span: Span,
}

impl Pipeline {
    #[must_use]
    pub fn new(stages: Vec<Stage>, operators: Vec<AstNode<PipeOperator>>, span: Span) -> Self {
        Self {
            stages,
            operators,
            span,
        }
    }

    #[must_use]
    pub fn from_stage(stage: Stage) -> Self {
        let span = stage.span();
        Self {
            stages: vec![stage],
            operators: Vec::new(),
            span,
        }
    }

    #[must_use]
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    #[must_use]
    pub fn operators(&self) -> &[AstNode<PipeOperator>] {
        &self.operators
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PipeOperator {
    Stdout,
    StdoutAndStderr,
}

pub type Stage = AstNode<StageKind>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageKind {
    Command(CommandStage),
    Expression(Expression),
}

/// A command head followed by its complete source-order item sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandStage {
    pub head: CommandHead,
    pub items: Vec<CommandItem>,
}

impl CommandStage {
    /// Visits command-local redirections in the same order as the source items.
    pub fn redirections(&self) -> impl Iterator<Item = &Redirection> {
        self.items.iter().filter_map(|item| match item.kind() {
            CommandItemKind::Redirection(redirection) => Some(redirection),
            CommandItemKind::Word(_) | CommandItemKind::Spread(_) | CommandItemKind::Closure(_) => {
                None
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandHead {
    kind: CommandHeadKind,
    word: Word,
    span: Span,
}

impl CommandHead {
    #[must_use]
    pub const fn new(kind: CommandHeadKind, word: Word, span: Span) -> Self {
        Self { kind, word, span }
    }

    #[must_use]
    pub const fn kind(&self) -> CommandHeadKind {
        self.kind
    }

    #[must_use]
    pub const fn word(&self) -> &Word {
        &self.word
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandHeadKind {
    Bare,
    ForcedExternal,
}

pub type CommandItem = AstNode<CommandItemKind>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandItemKind {
    Word(Word),
    Spread(VariableReference),
    Closure(Closure),
    Redirection(Redirection),
}

/// One command word and its adjacent source parts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Word {
    parts: Vec<WordPart>,
    span: Span,
}

impl Word {
    #[must_use]
    pub const fn new(parts: Vec<WordPart>, span: Span) -> Self {
        Self { parts, span }
    }

    #[must_use]
    pub fn parts(&self) -> &[WordPart] {
        &self.parts
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

pub type WordPart = AstNode<WordPartKind>;

/// A source part contributing to exactly one ordinary command word.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordPartKind {
    Bare,
    BareEscape,
    SingleQuoted,
    DoubleQuoted(Vec<WordPart>),
    DoubleText,
    DoubleEscape,
    Variable(Identifier),
    BracedInterpolation(Box<Expression>),
    CommandSubstitution(CommandSubstitution),
}

pub type Redirection = AstNode<RedirectionKind>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirectionKind {
    Input {
        descriptor: Option<IoNumber>,
        operator_span: Span,
        target: Word,
    },
    File(FileRedirection),
    Duplicate {
        descriptor: IoNumber,
        operator_span: Span,
        target: IoNumber,
    },
    Close {
        descriptor: IoNumber,
        operator_span: Span,
        target_span: Span,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRedirection {
    pub descriptor: Option<IoNumber>,
    pub mode: OutputMode,
    pub operator_span: Span,
    pub target: Word,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OutputMode {
    Truncate,
    Append,
}

/// A parsed decimal descriptor spelling and its exact source range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IoNumber {
    span: Span,
}

impl IoNumber {
    #[must_use]
    pub const fn new(span: Span) -> Self {
        Self { span }
    }

    #[must_use]
    pub const fn span(self) -> Span {
        self.span
    }
}
