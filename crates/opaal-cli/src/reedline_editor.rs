use std::borrow::Cow;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};
use reedline::{
    ColumnarMenu, Completer, CompletionResult, Emacs, ExternalOutput, Highlighter, Hinter, History,
    KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, SearchQuery, Signal, Span, StyledText, Suggestion,
    ValidationResult, Validator, default_emacs_keybindings,
};

use crate::completion::{CompletionCatalog, CompletionEngine};
use crate::editor::{
    EditorError, EditorEvent, EditorEventSource, EditorExternalPrint, EditorPrompt, LineEditor,
};
use crate::highlight::{HighlightKind, SyntaxHighlighter};
use crate::hint::{HintCatalog, HintEngine, MAX_HINT_HISTORY_ENTRIES};
use crate::history::{EditorHistory, HistoryError, HistorySelection};

/// macOS/Linux terminal editor backed by the pinned Reedline implementation.
pub struct ReedlineEditor {
    inner: Reedline,
    completion: Arc<RwLock<CompletionEngine>>,
    notices: ExternalNotices,
    break_signal: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
struct ExternalNotices(Arc<Mutex<NoticeState>>);

#[derive(Default)]
enum NoticeState {
    #[default]
    Idle,
    Queued(String),
    Drawing,
    Drawn,
}

impl ExternalNotices {
    fn state(&self) -> MutexGuard<'_, NoticeState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mark_drawn(&self) {
        // Reedline calls this on the next loop iteration, after painting the notice.
        let mut state = self.state();
        if matches!(*state, NoticeState::Drawing) {
            *state = NoticeState::Drawn;
        }
    }
}

impl ExternalOutput for ExternalNotices {
    fn drain(&mut self) -> io::Result<Vec<String>> {
        let mut state = self.state();
        let NoticeState::Queued(line) = &mut *state else {
            return Ok(Vec::new());
        };
        let line = std::mem::take(line);
        *state = NoticeState::Drawing;
        Ok(vec![line])
    }
}

impl ReedlineEditor {
    #[must_use]
    pub fn new() -> Self {
        let completion = default_completion();
        let (inner, notices, break_signal) = editor_engine(Arc::clone(&completion));
        Self {
            inner,
            completion,
            notices,
            break_signal,
        }
    }

    /// Constructs the editor with an OPAAL history policy.
    pub fn with_history(selection: HistorySelection) -> Result<Self, HistoryError> {
        let history = EditorHistory::open(selection)?;
        let completion = default_completion();
        let (inner, notices, break_signal) = editor_engine(Arc::clone(&completion));
        Ok(Self {
            inner: inner.with_history(history.into_backend()),
            completion,
            notices,
            break_signal,
        })
    }
}

fn default_completion() -> Arc<RwLock<CompletionEngine>> {
    let registry = opaal_runtime::builtin::standard_registry();
    let scope = opaal_runtime::ScopeStack::new();
    Arc::new(RwLock::new(CompletionEngine::new(
        CompletionCatalog::from_runtime(&registry, &scope),
    )))
}

fn editor_engine(
    completion: Arc<RwLock<CompletionEngine>>,
) -> (Reedline, ExternalNotices, Arc<AtomicBool>) {
    // Bind Tab to the completion menu so the installed completer is reachable.
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(COMPLETION_MENU.to_owned()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let notices = ExternalNotices::default();
    let drawn_notices = notices.clone();
    let break_signal = Arc::new(AtomicBool::new(false));
    let inner = Reedline::create()
        .with_cwd(Some(String::new()))
        .with_validator(Box::new(OpaalValidator))
        .with_highlighter(Box::new(ReedlineSyntaxHighlighter))
        .with_completer(Box::new(ReedlineCompleter { engine: completion }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name(COMPLETION_MENU),
        )))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_hinter(Box::new(ReedlineHinter::new()))
        .with_external_printer(notices.clone())
        .with_break_signal(Arc::clone(&break_signal))
        .with_poll_interval(Duration::from_millis(25))
        .with_idle_callback(Box::new(move || drawn_notices.mark_drawn()));
    (inner, notices, break_signal)
}

const COMPLETION_MENU: &str = "completion_menu";

impl Default for ReedlineEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl LineEditor for ReedlineEditor {
    fn write_notice(&mut self, rendered: &str) -> Result<(), EditorError> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        output
            .write_all(rendered.as_bytes())
            .and_then(|()| output.flush())
            .map_err(|error| EditorError::with_source("notice write failed", error))
    }

    fn set_completion_catalog(&mut self, catalog: CompletionCatalog) {
        let mut completion = self
            .completion
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        completion.install_catalog(catalog);
    }

    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        let prompt = ReedlinePrompt { prompt };
        let signal = self.read_reedline(&prompt)?;
        self.finish_signal(signal)
    }

    fn read_line_with_events(
        &mut self,
        prompt: &EditorPrompt,
        events: &mut dyn EditorEventSource,
    ) -> Result<EditorEvent, EditorError> {
        self.break_signal.store(false, Ordering::Relaxed);
        let prompt = ReedlinePrompt { prompt };
        let signal = thread::scope(|scope| {
            let (result_sender, result_receiver) = mpsc::sync_channel(1);
            let inner = &mut self.inner;
            scope.spawn(move || {
                let _ = result_sender.send(inner.read_line(&prompt));
            });

            let mut pending: Option<EditorExternalPrint> = None;
            let mut bridge_error: Option<EditorError> = None;
            loop {
                if bridge_error.is_none()
                    && pending.is_none()
                    && let Some(event) = events.next_external_print()
                {
                    *self.notices.state() = NoticeState::Queued(event.rendered().to_owned());
                    pending = Some(event);
                }

                let drawn = matches!(*self.notices.state(), NoticeState::Drawn);
                if drawn
                    && let Some(event) = pending.take()
                    && let Err(error) = events.acknowledge_external_print(&event)
                {
                    bridge_error = Some(error);
                    self.break_signal.store(true, Ordering::Relaxed);
                }

                match result_receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(result) => {
                        // An undrawn notice stays with the session for the next prompt.
                        let notice = std::mem::take(&mut *self.notices.state());
                        if bridge_error.is_none()
                            && result.is_ok()
                            && matches!(notice, NoticeState::Drawing | NoticeState::Drawn)
                            && let Some(event) = pending.take()
                        {
                            events.acknowledge_external_print(&event)?;
                        }
                        if let Some(error) = bridge_error {
                            return Err(error);
                        }
                        return result.map_err(|error| {
                            EditorError::with_source("line editor failed", error)
                        });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        *self.notices.state() = NoticeState::Idle;
                        return Err(EditorError::new("line editor worker stopped"));
                    }
                }
            }
        })?;
        self.finish_signal(signal)
    }
}

impl ReedlineEditor {
    fn read_reedline(&mut self, prompt: &dyn Prompt) -> Result<Signal, EditorError> {
        self.inner
            .read_line(prompt)
            .map_err(|error| EditorError::with_source("line editor failed", error))
    }

    fn finish_signal(&mut self, signal: Signal) -> Result<EditorEvent, EditorError> {
        if matches!(signal, Signal::Success(_)) {
            self.inner.sync_history().map_err(|error| {
                EditorError::with_source("cannot synchronize interactive history", error)
            })?;
        }
        map_signal(signal)
    }
}

struct OpaalValidator;

impl Validator for OpaalValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let source = SourceFile::new(SourceId::new(0), "<interactive>", line);
        let outcome = parse_opaal(&source);
        match outcome {
            ParseOutcome::Incomplete(_) => ValidationResult::Incomplete,
            ParseOutcome::Complete(_) | ParseOutcome::Invalid(_) => ValidationResult::Complete,
        }
    }
}

struct ReedlineSyntaxHighlighter;

impl Highlighter for ReedlineSyntaxHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        for segment in SyntaxHighlighter::new().highlight(line) {
            styled.push((highlight_style(segment.kind()), segment.text().to_owned()));
        }
        styled
    }
}

struct ReedlineCompleter {
    engine: Arc<RwLock<CompletionEngine>>,
}

impl Completer for ReedlineCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let suggestions = self
            .engine
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .complete(line, pos)
            .into_iter()
            .map(|completion| {
                let replacement = completion.replacement();
                Suggestion {
                    value: completion.value().to_owned(),
                    description: Some(format!("{:?}", completion.kind())),
                    span: Span::new(replacement.start, replacement.end),
                    append_whitespace: completion.append_whitespace(),
                    ..Suggestion::default()
                }
            })
            .collect::<Vec<_>>();
        CompletionResult::fresh(suggestions)
    }
}

struct ReedlineHinter {
    engine: HintEngine,
    current_hint: String,
}

impl ReedlineHinter {
    const fn new() -> Self {
        Self {
            engine: HintEngine::new(),
            current_hint: String::new(),
        }
    }
}

impl Hinter for ReedlineHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        let mut query = SearchQuery::last_with_prefix(line.to_owned(), history.session());
        query.limit = Some(MAX_HINT_HISTORY_ENTRIES as i64);
        let catalog = history.search(query).map_or_else(
            |_| HintCatalog::default(),
            |items| HintCatalog::new(items.into_iter().map(|item| item.command_line)),
        );
        self.current_hint = self
            .engine
            .hint(line, pos, &catalog)
            .map_or_else(String::new, |hint| hint.suffix().to_owned());

        if use_ansi_coloring && !self.current_hint.is_empty() {
            hint_style().paint(&self.current_hint).to_string()
        } else {
            self.current_hint.clone()
        }
    }

    fn complete_hint(&self) -> String {
        self.current_hint.clone()
    }

    fn next_hint_token(&self) -> String {
        self.current_hint.clone()
    }
}

fn hint_style() -> Style {
    Style::new().fg(Color::DarkGray).italic()
}

fn highlight_style(kind: HighlightKind) -> Style {
    match kind {
        HighlightKind::Plain => Style::new(),
        HighlightKind::Comment => Style::new().fg(Color::DarkGray).italic(),
        HighlightKind::Keyword => Style::new().fg(Color::Purple).bold(),
        HighlightKind::Literal => Style::new().fg(Color::Yellow),
        HighlightKind::String => Style::new().fg(Color::Green),
        HighlightKind::Escape => Style::new().fg(Color::LightYellow),
        HighlightKind::Interpolation => Style::new().fg(Color::Cyan),
        HighlightKind::Operator => Style::new().fg(Color::LightBlue),
        HighlightKind::Delimiter => Style::new().fg(Color::Blue),
        HighlightKind::Invalid => Style::new().fg(Color::Red).underline(),
    }
}

fn map_signal(signal: Signal) -> Result<EditorEvent, EditorError> {
    match signal {
        Signal::Success(buffer) => Ok(EditorEvent::Submitted(buffer)),
        Signal::CtrlC(_) => Ok(EditorEvent::Cancelled),
        Signal::CtrlD => Ok(EditorEvent::EndOfInput),
        Signal::HostCommand(command) => Ok(EditorEvent::HostCommand(command)),
        Signal::ExternalBreak(buffer) => Ok(EditorEvent::ExternalBreak(buffer)),
        _ => Err(EditorError::new(
            "line editor returned an unsupported event",
        )),
    }
}

struct ReedlinePrompt<'prompt> {
    prompt: &'prompt EditorPrompt,
}

impl Prompt for ReedlinePrompt<'_> {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.prompt.primary())
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.prompt.continuation())
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reedline_signal_crosses_as_an_editor_owned_event() {
        let cases = [
            (
                Signal::Success("echo hello".to_owned()),
                EditorEvent::Submitted("echo hello".to_owned()),
            ),
            (
                Signal::CtrlC("discarded input".to_owned()),
                EditorEvent::Cancelled,
            ),
            (Signal::CtrlD, EditorEvent::EndOfInput),
            (
                Signal::HostCommand("host-action".to_owned()),
                EditorEvent::HostCommand("host-action".to_owned()),
            ),
            (
                Signal::ExternalBreak("partial input".to_owned()),
                EditorEvent::ExternalBreak("partial input".to_owned()),
            ),
        ];

        for (signal, expected) in cases {
            assert_eq!(
                map_signal(signal).expect("known signal should map"),
                expected
            );
        }
    }

    #[test]
    fn external_notice_is_acknowledgeable_after_the_draw() {
        let mut notices = ExternalNotices::default();
        *notices.state() = NoticeState::Queued("completed".to_owned());
        notices.mark_drawn();
        assert!(matches!(*notices.state(), NoticeState::Queued(_)));
        assert_eq!(notices.drain().unwrap(), ["completed"]);
        assert!(matches!(*notices.state(), NoticeState::Drawing));
        assert!(notices.drain().unwrap().is_empty());
        notices.mark_drawn();
        assert!(matches!(*notices.state(), NoticeState::Drawn));
    }

    #[test]
    fn prompt_bridge_preserves_primary_and_continuation_text() {
        let prompt = EditorPrompt::new(">> ", "...> ");
        let bridge = ReedlinePrompt { prompt: &prompt };

        assert_eq!(bridge.render_prompt_left(), ">> ");
        assert_eq!(bridge.render_prompt_multiline_indicator(), "...> ");
    }

    #[test]
    fn parser_validation_continues_only_structurally_incomplete_input() {
        let validator = OpaalValidator;

        for complete in [
            "",
            "echo hello",
            "if true {\n    echo yes\n}",
            "def ready()\n{\nreturn 5\n}",
            "def missing()\nlet next = 1\n",
        ] {
            assert!(matches!(
                validator.validate(complete),
                ValidationResult::Complete
            ));
        }

        let invalid = SourceFile::new(SourceId::new(7), "<test-invalid>", "else");
        assert!(matches!(parse_opaal(&invalid), ParseOutcome::Invalid(_)));
        assert!(matches!(
            validator.validate(invalid.text()),
            ValidationResult::Complete
        ));

        for incomplete in [
            "echo hello |",
            "echo >",
            "echo \"unterminated",
            "if true {",
            "def waiting()\n",
            "def waiting() -> Int # signature\n",
            "def waiting()\n{\nreturn 5\n",
        ] {
            assert!(matches!(
                validator.validate(incomplete),
                ValidationResult::Incomplete
            ));
        }
    }

    #[test]
    fn reedline_highlighter_preserves_source_and_maps_semantic_styles() {
        let source = "let name = \"{value}\" # note";
        let styled = ReedlineSyntaxHighlighter.highlight(source, source.len());

        assert_eq!(styled.raw_string(), source);
        assert!(styled.buffer.iter().any(|(style, text)| *style
            == highlight_style(HighlightKind::Keyword)
            && text == "let"));
        assert!(styled.buffer.iter().any(|(style, text)| {
            *style == highlight_style(HighlightKind::Interpolation) && text == "{"
        }));
        assert!(styled.buffer.iter().any(|(style, text)| {
            *style == highlight_style(HighlightKind::Comment) && text == "# note"
        }));
    }

    #[test]
    fn reedline_completer_preserves_replacement_span_and_spacing_policy() {
        let registry = opaal_runtime::builtin::standard_registry();
        let scope = opaal_runtime::ScopeStack::new();
        let mut completer = ReedlineCompleter {
            engine: Arc::new(RwLock::new(CompletionEngine::new(
                CompletionCatalog::from_runtime(&registry, &scope),
            ))),
        };

        let suggestions = completer.complete("pw", 2);
        let pwd = suggestions
            .suggestions()
            .iter()
            .find(|suggestion| suggestion.value == "pwd")
            .expect("standard command should be bridged");
        assert_eq!(pwd.span, Span::new(0, 2));
        assert!(pwd.append_whitespace);
    }

    #[test]
    fn reedline_hinter_uses_exact_raw_suffix_for_display_and_acceptance() {
        use reedline::{FileBackedHistory, HistoryItem};

        let mut history = FileBackedHistory::new(4).expect("in-memory history should initialize");
        history
            .save(HistoryItem::from_command_line("echo λ-world"))
            .expect("history entry should save");
        let mut hinter = ReedlineHinter::new();

        let raw = hinter.handle("echo λ", "echo λ".len(), &history, false, "");
        assert_eq!(raw, "-world");
        assert_eq!(hinter.complete_hint(), "-world");
        assert_eq!(hinter.next_hint_token(), "-world");

        let styled = hinter.handle("echo λ", "echo λ".len(), &history, true, "");
        assert_eq!(styled, hint_style().paint("-world").to_string());
        assert_eq!(hinter.complete_hint(), "-world");
    }
}
