//! The R console: R's output scrolls by in a display-only terminal, which
//! handles ANSI colors and `\r` progress bars the way RStudio's console does,
//! and code is typed into an R editor below it.

use anyhow::{Context as _, Result};
use editor::actions::{MoveToEnd, Newline};
use editor::{Editor, EditorElement, EditorStyle};
use gpui::{
    Action, App, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Pixels, Subscription, Task, TextStyle, WeakEntity, Window, actions, px, relative,
};
use jupyter_protocol::{IsCompleteReplyStatus, JupyterMessageContent, MediaType, Stdio};
use language::Point;
use project::Project;
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry, SessionState};
use settings::Settings as _;
use std::path::PathBuf;
use terminal::Terminal;
use terminal::terminal_settings::TerminalSettings;
use terminal_view::TerminalView;
use theme_settings::ThemeSettings;
use ui::{CommonAnimationExt as _, IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

actions!(
    r_console,
    [
        /// Opens the R console and moves focus to its input.
        ToggleFocus,
        /// Runs the code typed in the console, or continues it on a new line if it is incomplete.
        Submit,
        /// Shows the previous command from the console history.
        PreviousHistory,
        /// Shows the next command from the console history.
        NextHistory,
        /// Interrupts R if it is running code; otherwise clears the console input.
        Escape,
        /// Interrupts the code R is running.
        Interrupt,
        /// Clears the console output.
        ClearConsole,
        /// Restarts R.
        RestartR,
    ]
);

const CONSOLE_PANEL_KEY: &str = "RConsolePanel";

/// SGR codes used for the console's own text, so they follow the terminal theme.
const ECHO_STYLE: &str = "34";
const STDERR_STYLE: &str = "31";
const NOTICE_STYLE: &str = "2";
const WARNING_STYLE: &str = "33";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ConsolePanel>(window, cx);
        });
    })
    .detach();
}

pub struct ConsolePanel {
    workspace: WeakEntity<Workspace>,
    workspace_id: EntityId,
    session: Option<Entity<RSession>>,
    terminal: Entity<Terminal>,
    terminal_view: Entity<TerminalView>,
    input: Entity<Editor>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// Whether the last character written to the terminal ended a line.
    at_line_start: bool,
    /// The prompt of a `readline()`-style request R is waiting on.
    input_request: Option<InputRequest>,
    history: Vec<String>,
    history_index: Option<usize>,
    /// The input typed before browsing history, restored when browsing past the end.
    history_draft: String,
    last_state: Option<SessionState>,
    _completeness_check: Option<Task<()>>,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

struct InputRequest {
    prompt: String,
    password: bool,
}

impl ConsolePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_handle = workspace.weak_handle();
        let workspace_id = workspace_handle.entity_id();
        let project = workspace.project().clone();

        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                TerminalSettings::get_global(cx).cursor_shape,
                TerminalSettings::get_global(cx).alternate_scroll,
                None,
                0,
                cx.background_executor(),
                util::paths::PathStyle::local(),
            )
            .subscribe(cx)
        });
        terminal.update(cx, |terminal, cx| {
            terminal.write_raw_output(HIDE_CURSOR, cx)
        });
        let terminal_view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                workspace_handle.clone(),
                workspace.database_id(),
                project.downgrade(),
                window,
                cx,
            )
        });

        let input = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 12, window, cx);
            editor.set_placeholder_text("Type R code and press Enter", window, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor
        });
        Self::set_input_language(&input, &project, cx);

        let focus_handle = cx.focus_handle();
        let mut subscriptions = vec![cx.on_focus(&focus_handle, window, |this, window, cx| {
            this.input.focus_handle(cx).focus(window, cx);
        })];
        if let Some(registry) = SessionRegistry::global(cx) {
            subscriptions.push(cx.subscribe_in(
                &registry,
                window,
                |this, _, event: &SessionAdded, window, cx| {
                    if event.workspace_id == this.workspace_id {
                        this.set_session(event.session.clone(), window, cx);
                    }
                },
            ));
        }

        let mut this = Self {
            workspace: workspace_handle,
            workspace_id,
            session: None,
            terminal,
            terminal_view,
            input,
            focus_handle,
            position: DockPosition::Bottom,
            at_line_start: true,
            input_request: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            last_state: None,
            _completeness_check: None,
            _session_subscription: None,
            _subscriptions: subscriptions,
        };
        let existing_session = SessionRegistry::global(cx)
            .and_then(|registry| registry.read(cx).session(workspace_id).cloned());
        if let Some(session) = existing_session {
            this.set_session(session, window, cx);
        }
        this
    }

    fn set_input_language(input: &Entity<Editor>, project: &Entity<Project>, cx: &mut App) {
        let languages = project.read(cx).languages().clone();
        let Some(buffer) = input.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        cx.spawn(async move |cx| {
            let language = languages
                .language_for_name(runtime::R_LANGUAGE_NAME)
                .await?;
            buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn set_session(
        &mut self,
        session: Entity<RSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._session_subscription =
            Some(cx.subscribe_in(&session, window, Self::handle_session_event));
        self.last_state = None;
        self.session = Some(session);
        self.report_state(cx);
        cx.notify();
    }

    pub fn session(&self) -> Option<&Entity<RSession>> {
        self.session.as_ref()
    }

    /// Runs `code` in R and shows it in the console as if it had been typed,
    /// for code sent from the editor.
    pub fn execute(&mut self, code: &str, cx: &mut Context<Self>) -> Result<()> {
        let session = self
            .session
            .clone()
            .context("R has not started for this project")?;
        let code = code.trim_end_matches(['\n', '\r']).to_string();
        self.echo_input(&code, cx);
        session.update(cx, |session, cx| session.execute(code.clone(), cx))?;
        self.push_history(code);
        Ok(())
    }

    fn handle_session_event(
        &mut self,
        session: &Entity<RSession>,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::Output { content, .. } => self.write_output(content, cx),
            SessionEvent::ExecutionFinished { .. } => {
                // An interrupt cancels any pending `readline()`.
                if !session.read(cx).is_waiting_for_input() {
                    self.finish_input_request(window, cx);
                }
            }
            SessionEvent::InputRequested { prompt, password } => {
                self.input_request = Some(InputRequest {
                    prompt: prompt.clone(),
                    password: *password,
                });
                self.input
                    .update(cx, |editor, cx| editor.set_masked(*password, cx));
                self.input.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
            SessionEvent::StateChanged => self.report_state(cx),
            SessionEvent::PromptChanged | SessionEvent::WorkingDirectoryChanged => cx.notify(),
            SessionEvent::ClearConsole => self.clear(cx),
            SessionEvent::ShowMessage(message) => self.write_notice(message, WARNING_STYLE, cx),
            SessionEvent::OpenEditor { path, line, column } => {
                self.open_editor(path.clone(), *line, *column, window, cx)
            }
            SessionEvent::LanguageServerStarted { .. } => {}
        }
    }

    /// Writes a line about the session's state when it changes in a way the
    /// user should see in the scrollback.
    fn report_state(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let session = session.read(cx);
        let state = session.state().clone();
        let previous = self.last_state.replace(state.clone());
        if previous.as_ref() == Some(&state) {
            return;
        }
        let version = session
            .r_installation()
            .and_then(|r| r.version.clone())
            .unwrap_or_default();
        match &state {
            SessionState::Unavailable(message) => self.write_notice(message, WARNING_STYLE, cx),
            SessionState::Restricted => {
                self.write_notice("R will start once you trust this folder.", NOTICE_STYLE, cx)
            }
            SessionState::Restarting => self.write_notice("Restarting R…", NOTICE_STYLE, cx),
            SessionState::Idle
                if matches!(
                    previous,
                    None | Some(SessionState::Starting) | Some(SessionState::Restarting)
                ) =>
            {
                self.write_notice(format!("R {version}").trim_end(), NOTICE_STYLE, cx)
            }
            SessionState::Exited(message) => self.write_notice(message, WARNING_STYLE, cx),
            SessionState::Starting | SessionState::Idle | SessionState::Busy => {}
        }
        cx.notify();
    }

    fn write_output(&mut self, content: &JupyterMessageContent, cx: &mut Context<Self>) {
        match content {
            JupyterMessageContent::StreamContent(stream) => match stream.name {
                Stdio::Stderr => self.write_styled(&stream.text, STDERR_STYLE, cx),
                Stdio::Stdout => self.write(&stream.text, cx),
            },
            JupyterMessageContent::ExecuteResult(result) => self.write_media(&result.data, cx),
            JupyterMessageContent::DisplayData(display) => self.write_media(&display.data, cx),
            JupyterMessageContent::ErrorOutput(error) => {
                self.ensure_line_start(cx);
                self.write_styled(&format!("{}\n", error.evalue), STDERR_STYLE, cx);
            }
            _ => {}
        }
    }

    fn write_media(&mut self, media: &jupyter_protocol::Media, cx: &mut Context<Self>) {
        let plain_text = media
            .content
            .iter()
            .find_map(|media_type| match media_type {
                MediaType::Plain(text) => Some(text.clone()),
                _ => None,
            });
        match plain_text {
            Some(text) => {
                self.write(&text, cx);
                self.ensure_line_start(cx);
            }
            None => self.write_notice(
                "This output can't be shown in the console yet.",
                NOTICE_STYLE,
                cx,
            ),
        }
    }

    fn echo_input(&mut self, code: &str, cx: &mut Context<Self>) {
        let (prompt, continuation) = self.prompts(cx);
        let mut echo = String::new();
        for (index, line) in code.split('\n').enumerate() {
            echo.push_str(if index == 0 { &prompt } else { &continuation });
            echo.push_str(line);
            echo.push('\n');
        }
        self.ensure_line_start(cx);
        self.write_styled(&echo, ECHO_STYLE, cx);
    }

    fn prompts(&self, cx: &App) -> (String, String) {
        match &self.session {
            Some(session) => {
                let prompt = session.read(cx).prompt();
                (
                    prompt.input_prompt.clone(),
                    prompt.continuation_prompt.clone(),
                )
            }
            None => ("> ".to_string(), "+ ".to_string()),
        }
    }

    fn write_notice(&mut self, text: &str, style: &str, cx: &mut Context<Self>) {
        self.ensure_line_start(cx);
        self.write_styled(&format!("{text}\n"), style, cx);
    }

    fn write_styled(&mut self, text: &str, style: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // Reset before a trailing newline so the style doesn't bleed into the
        // next line's background.
        let (body, newline) = match text.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (text, ""),
        };
        self.write(&format!("\x1b[{style}m{body}\x1b[0m{newline}"), cx);
    }

    fn write(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(last) = text.chars().last() else {
            return;
        };
        self.at_line_start = last == '\n';
        self.terminal.update(cx, |terminal, cx| {
            terminal.write_output(text.as_bytes(), cx)
        });
    }

    fn ensure_line_start(&mut self, cx: &mut Context<Self>) {
        if !self.at_line_start {
            self.write("\n", cx);
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            terminal.replace_display_output(HIDE_CURSOR, cx).log_err();
        });
        self.at_line_start = true;
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        let code = self.input.read(cx).text(cx);
        let Some(session) = self.session.clone() else {
            self.write_notice("R has not started for this project.", WARNING_STYLE, cx);
            return;
        };

        if let Some(request) = self.input_request.take() {
            let shown = if request.password {
                "*".repeat(code.chars().count())
            } else {
                code.clone()
            };
            self.ensure_line_start(cx);
            self.write_styled(&format!("{}{shown}\n", request.prompt), ECHO_STYLE, cx);
            session
                .update(cx, |session, _| session.send_input(code))
                .log_err();
            self.finish_input_request(window, cx);
            return;
        }

        if code.trim().is_empty() {
            self.echo_input("", cx);
            self.clear_input(window, cx);
            return;
        }

        match session.read(cx).state() {
            SessionState::Idle => {
                let check = session.update(cx, |session, cx| session.is_complete(code.clone(), cx));
                self._completeness_check = Some(cx.spawn_in(window, async move |this, cx| {
                    let status = check.await;
                    this.update_in(cx, |this, window, cx| {
                        this._completeness_check = None;
                        // Leave input that changed while R was checking it alone.
                        if this.input.read(cx).text(cx) != code {
                            return;
                        }
                        match status {
                            Ok(IsCompleteReplyStatus::Incomplete) => {
                                this.input.update(cx, |editor, cx| {
                                    editor.move_to_end(&MoveToEnd, window, cx);
                                    editor.newline(&Newline, window, cx);
                                });
                            }
                            // Invalid code still runs, so R reports the syntax error.
                            _ => this.run_input(code, window, cx),
                        }
                    })
                    .log_err();
                }));
            }
            // R is busy, so the check would wait behind the running code; send
            // the code now and let R queue it, as RStudio does.
            SessionState::Busy | SessionState::Starting => self.run_input(code, window, cx),
            SessionState::Restricted
            | SessionState::Restarting
            | SessionState::Unavailable(_)
            | SessionState::Exited(_) => {
                self.write_notice("R is not running.", WARNING_STYLE, cx);
            }
        }
    }

    fn run_input(&mut self, code: String, window: &mut Window, cx: &mut Context<Self>) {
        match self.execute(&code, cx) {
            Ok(()) => self.clear_input(window, cx),
            Err(error) => self.write_notice(&format!("{error:#}"), WARNING_STYLE, cx),
        }
    }

    fn clear_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.history_index = None;
        self.history_draft.clear();
    }

    fn finish_input_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input_request = None;
        self.input
            .update(cx, |editor, cx| editor.set_masked(false, cx));
        self.clear_input(window, cx);
        cx.notify();
    }

    fn push_history(&mut self, code: String) {
        if self.history.last() != Some(&code) {
            self.history.push(code);
        }
        self.history_index = None;
    }

    /// Whether the input's cursor is on its first and last line.
    fn cursor_edges(&self, cx: &mut Context<Self>) -> (bool, bool) {
        self.input.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let head = editor.selections.newest::<Point>(&snapshot).head();
            let last_row = editor.buffer().read(cx).snapshot(cx).max_point().row;
            (head.row == 0, head.row == last_row)
        })
    }

    fn previous_history(
        &mut self,
        _: &PreviousHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (on_first_line, _) = self.cursor_edges(cx);
        if !on_first_line {
            self.input.update(cx, |editor, cx| {
                editor.move_up(&zed_actions::editor::MoveUp, window, cx)
            });
            return;
        }
        let Some(last) = self.history.len().checked_sub(1) else {
            return;
        };
        let index = match self.history_index {
            None => {
                self.history_draft = self.input.read(cx).text(cx);
                last
            }
            Some(index) => index.saturating_sub(1),
        };
        self.show_history_entry(index, window, cx);
    }

    fn next_history(&mut self, _: &NextHistory, window: &mut Window, cx: &mut Context<Self>) {
        let (_, on_last_line) = self.cursor_edges(cx);
        if !on_last_line {
            self.input.update(cx, |editor, cx| {
                editor.move_down(&zed_actions::editor::MoveDown, window, cx)
            });
            return;
        }
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.show_history_entry(index + 1, window, cx);
        } else {
            let draft = std::mem::take(&mut self.history_draft);
            self.history_index = None;
            self.set_input_text(&draft, window, cx);
        }
    }

    fn show_history_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.history.get(index).cloned() else {
            return;
        };
        self.history_index = Some(index);
        self.set_input_text(&entry, window, cx);
    }

    fn set_input_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end(&MoveToEnd, window, cx);
        });
    }

    fn escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        let busy = self
            .session
            .as_ref()
            .is_some_and(|session| *session.read(cx).state() == SessionState::Busy);
        if busy {
            self.interrupt(&Interrupt, window, cx);
        } else {
            self.clear_input(window, cx);
        }
    }

    fn interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session
                .update(cx, |session, _| session.interrupt())
                .log_err();
        }
    }

    fn clear_console(&mut self, _: &ClearConsole, _: &mut Window, cx: &mut Context<Self>) {
        self.clear(cx);
    }

    fn restart(&mut self, _: &RestartR, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session.clone() {
            self.finish_input_request(window, cx);
            session.update(cx, |session, cx| session.restart(window, cx));
        }
    }

    fn open_editor(
        &mut self,
        path: PathBuf,
        line: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
        });
        cx.spawn_in(window, async move |_, cx| {
            let item = open.await?;
            // R reports 1-based lines and columns.
            if line > 0
                && let Some(editor) = item.downcast::<Editor>()
            {
                let point = Point::new(line - 1, column.saturating_sub(1));
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(point, window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn input_style(&self, cx: &App) -> EditorStyle {
        let settings = ThemeSettings::get_global(cx);
        let theme = cx.theme();
        EditorStyle {
            background: theme.colors().editor_background,
            local_player: theme.players().local(),
            text: TextStyle {
                color: theme.colors().text,
                font_family: settings.buffer_font.family.clone(),
                font_features: settings.buffer_font.features.clone(),
                font_size: settings.buffer_font_size(cx).into(),
                font_weight: settings.buffer_font.weight,
                line_height: relative(settings.buffer_line_height.value()),
                ..Default::default()
            },
            syntax: theme.syntax().clone(),
            ..Default::default()
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, version, working_directory, busy) = match &self.session {
            Some(session) => {
                let session = session.read(cx);
                let status = match session.state() {
                    SessionState::Unavailable(_) => Some("R unavailable"),
                    SessionState::Restricted => Some("Waiting for folder trust"),
                    SessionState::Starting => Some("Starting R…"),
                    SessionState::Restarting => Some("Restarting R…"),
                    SessionState::Exited(_) => Some("R stopped"),
                    SessionState::Idle | SessionState::Busy => None,
                };
                let version = session
                    .r_installation()
                    .and_then(|r| r.version.clone())
                    .map(|version| format!("R {version}"));
                let working_directory = session.working_directory().map(|directory| {
                    directory
                        .strip_prefix(util::paths::home_dir())
                        .map(|relative| PathBuf::from("~").join(relative))
                        .unwrap_or_else(|_| directory.clone())
                        .to_string_lossy()
                        .into_owned()
                });
                (
                    status,
                    version,
                    working_directory,
                    *session.state() == SessionState::Busy,
                )
            }
            None => (Some("R is not running"), None, None, false),
        };
        let running = self.session.as_ref().is_some_and(|session| {
            matches!(
                session.read(cx).state(),
                SessionState::Idle | SessionState::Busy
            )
        });

        h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .children(version.map(|version| Label::new(version).size(LabelSize::Small)))
            .children(status.map(|status| {
                Label::new(status)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .children(working_directory.map(|directory| {
                Label::new(directory)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate()
            }))
            .child(div().flex_1())
            .when(busy, |this| {
                this.child(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
            })
            .child(
                IconButton::new("interrupt-r", IconName::Stop)
                    .icon_size(IconSize::Small)
                    .disabled(!busy)
                    .tooltip(Tooltip::text("Interrupt R"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.interrupt(&Interrupt, window, cx)),
                    ),
            )
            .child(
                IconButton::new("clear-r-console", IconName::Eraser)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Clear Console"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.clear_console(&ClearConsole, window, cx)
                    })),
            )
            .child(
                IconButton::new("restart-r", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(
                        !running
                            && !matches!(
                                self.session
                                    .as_ref()
                                    .map(|session| session.read(cx).state().clone()),
                                Some(SessionState::Exited(_))
                            ),
                    )
                    .tooltip(Tooltip::text("Restart R"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.restart(&RestartR, window, cx)),
                    ),
            )
    }
}

impl Render for ConsolePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = match &self.input_request {
            Some(request) => request.prompt.clone(),
            None => self.prompts(cx).0,
        };
        v_flex()
            .key_context("RConsole")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::previous_history))
            .on_action(cx.listener(Self::next_history))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::interrupt))
            .on_action(cx.listener(Self::clear_console))
            .on_action(cx.listener(Self::restart))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.terminal_view.clone()))
            .child(
                h_flex()
                    .items_start()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(prompt).buffer_font(cx).color(Color::Accent))
                    .child(
                        div()
                            .flex_1()
                            .child(EditorElement::new(&self.input, self.input_style(cx))),
                    ),
            )
    }
}

impl Focusable for ConsolePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ConsolePanel {}

impl Panel for ConsolePanel {
    fn persistent_name() -> &'static str {
        "R Console"
    }

    fn panel_key() -> &'static str {
        CONSOLE_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Code)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("R Console")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        true
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use gpui::{TestAppContext, VisualTestContext};
    use jupyter_protocol::{
        ErrorOutput, ExecuteResult, ExecutionCount, InputRequest as JupyterInputRequest,
        IsCompleteReply, JupyterMessage, KernelInfoReply, Media, StreamContent,
    };
    use languages::r::{RInstallationSource, RToolchainData};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    struct Harness {
        panel: Entity<ConsolePanel>,
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
        stdin: mpsc::Receiver<JupyterMessage>,
    }

    impl Harness {
        async fn new(cx: &mut TestAppContext) -> (Self, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings = SettingsStore::test(cx);
                cx.set_global(settings);
                theme_settings::init(theme::LoadThemes::JustBase, cx);
                editor::init(cx);
            });
            let fs = FakeFs::new(cx.executor());
            let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
            let window =
                cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .unwrap();
            let cx = VisualTestContext::from_window(window.into(), cx).into_mut();

            let panel = workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| ConsolePanel::new(workspace, window, cx))
            });
            let session = cx.new(|cx| {
                RSession::new(
                    Some(RToolchainData {
                        r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
                        version: Some("4.5.3".to_string()),
                        source: RInstallationSource::Rig,
                    }),
                    PathBuf::from("/projects/analysis"),
                    Default::default(),
                    None,
                    cx,
                )
            });
            let (requests_tx, mut requests) = mpsc::channel(100);
            let (stdin_tx, stdin) = mpsc::channel(100);
            session.update(cx, |session, cx| {
                session.connect_for_test(requests_tx, stdin_tx, cx)
            });
            panel.update_in(cx, |panel, window, cx| {
                panel.set_session(session.clone(), window, cx)
            });

            // Startup: answer kernel_info so R is idle, and drop the comm opens.
            let kernel_info = requests.try_recv().unwrap();
            for _ in 0..3 {
                requests.try_recv().unwrap();
            }
            let harness = Self {
                panel,
                session,
                requests,
                stdin,
            };
            harness.receive(
                JupyterMessage::new(kernel_info_reply(), Some(&kernel_info)),
                cx,
            );
            cx.run_until_parked();
            (harness, cx)
        }

        fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
            self.session.update_in(cx, |session, window, cx| {
                session.receive_for_test(&message, window, cx)
            });
            cx.run_until_parked();
        }

        fn type_input(&self, text: &str, cx: &mut VisualTestContext) {
            let input = self.panel.read_with(cx, |panel, _| panel.input.clone());
            input.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.move_to_end(&MoveToEnd, window, cx);
            });
        }

        fn input_text(&self, cx: &mut VisualTestContext) -> String {
            self.panel
                .read_with(cx, |panel, cx| panel.input.read(cx).text(cx))
        }

        fn press_enter(&self, cx: &mut VisualTestContext) {
            self.panel
                .update_in(cx, |panel, window, cx| panel.submit(&Submit, window, cx));
            cx.run_until_parked();
        }

        fn output(&self, cx: &mut VisualTestContext) -> String {
            self.panel
                .read_with(cx, |panel, cx| panel.terminal.read(cx).get_content())
        }
    }

    fn kernel_info_reply() -> KernelInfoReply {
        serde_json::from_value(serde_json::json!({
            "status": "ok",
            "protocol_version": "5.3",
            "implementation": "ark",
            "implementation_version": "0.1.252",
            "language_info": { "name": "R", "version": "4.5.3" },
            "banner": "",
            "help_links": [],
            "debugger": false
        }))
        .unwrap()
    }

    fn idle(request: &JupyterMessage) -> JupyterMessage {
        JupyterMessage::new(jupyter_protocol::Status::idle(), Some(request))
    }

    #[gpui::test]
    async fn test_runs_complete_input_and_shows_output(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        assert!(harness.output(cx).contains("R 4.5.3"));

        harness.type_input("1 + 1", cx);
        harness.press_enter(cx);
        let check = harness.requests.try_recv().unwrap();
        let JupyterMessageContent::IsCompleteRequest(request) = &check.content else {
            panic!(
                "expected is_complete_request, got {}",
                check.header.msg_type
            );
        };
        assert_eq!(request.code, "1 + 1");
        harness.receive(
            JupyterMessage::new(
                IsCompleteReply::new(IsCompleteReplyStatus::Complete, String::new()),
                Some(&check),
            ),
            cx,
        );

        let execute = harness.requests.try_recv().unwrap();
        let JupyterMessageContent::ExecuteRequest(request) = &execute.content else {
            panic!("expected execute_request, got {}", execute.header.msg_type);
        };
        assert_eq!(request.code, "1 + 1");
        assert_eq!(harness.input_text(cx), "");
        assert!(harness.output(cx).contains("> 1 + 1"));

        harness.receive(
            JupyterMessage::new(
                ExecuteResult::new(
                    ExecutionCount::new(1),
                    Media {
                        content: vec![MediaType::Plain("[1] 2".into())],
                    },
                ),
                Some(&execute),
            ),
            cx,
        );
        harness.receive(idle(&execute), cx);
        assert!(harness.output(cx).contains("[1] 2"));
    }

    #[gpui::test]
    async fn test_incomplete_input_continues_on_a_new_line(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.type_input("mean(", cx);
        harness.press_enter(cx);
        let check = harness.requests.try_recv().unwrap();
        harness.receive(
            JupyterMessage::new(
                IsCompleteReply::new(IsCompleteReplyStatus::Incomplete, String::new()),
                Some(&check),
            ),
            cx,
        );
        assert!(harness.input_text(cx).starts_with("mean(\n"));
        assert!(
            harness.requests.try_recv().is_err(),
            "nothing should run yet"
        );
    }

    #[gpui::test]
    async fn test_multi_line_input_is_echoed_with_continuation_prompts(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.panel.update(cx, |panel, cx| {
            panel
                .execute("f <- function(x) {\n  x + 1\n}\n", cx)
                .unwrap()
        });
        let execute = harness.requests.try_recv().unwrap();
        let JupyterMessageContent::ExecuteRequest(request) = &execute.content else {
            panic!("expected execute_request");
        };
        assert_eq!(request.code, "f <- function(x) {\n  x + 1\n}");
        let output = harness.output(cx);
        assert!(output.contains("> f <- function(x) {"), "{output}");
        assert!(output.contains("+   x + 1"), "{output}");
        assert!(output.contains("+ }"), "{output}");
    }

    #[gpui::test]
    async fn test_history_navigation(cx: &mut TestAppContext) {
        let (harness, cx) = Harness::new(cx).await;
        harness.panel.update(cx, |panel, cx| {
            panel.execute("x <- 1", cx).unwrap();
            panel.execute("y <- 2", cx).unwrap();
        });
        harness.type_input("draft", cx);

        let previous = |cx: &mut VisualTestContext| {
            harness.panel.update_in(cx, |panel, window, cx| {
                panel.previous_history(&PreviousHistory, window, cx)
            })
        };
        let next = |cx: &mut VisualTestContext| {
            harness.panel.update_in(cx, |panel, window, cx| {
                panel.next_history(&NextHistory, window, cx)
            })
        };

        previous(cx);
        assert_eq!(harness.input_text(cx), "y <- 2");
        previous(cx);
        assert_eq!(harness.input_text(cx), "x <- 1");
        previous(cx);
        assert_eq!(harness.input_text(cx), "x <- 1");
        next(cx);
        assert_eq!(harness.input_text(cx), "y <- 2");
        next(cx);
        assert_eq!(harness.input_text(cx), "draft");
    }

    #[gpui::test]
    async fn test_readline_prompt_is_answered_from_the_input(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.receive(
            JupyterMessage::new(
                JupyterInputRequest {
                    prompt: "Name: ".into(),
                    password: false,
                },
                None,
            ),
            cx,
        );
        harness.panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel
                    .input_request
                    .as_ref()
                    .map(|request| request.prompt.as_str()),
                Some("Name: ")
            )
        });

        harness.type_input("Ada", cx);
        harness.press_enter(cx);
        let reply = harness.stdin.try_recv().unwrap();
        let JupyterMessageContent::InputReply(reply) = &reply.content else {
            panic!("expected input_reply");
        };
        assert_eq!(reply.value, "Ada");
        assert!(
            harness.requests.try_recv().is_err(),
            "readline input must not run as code"
        );
        assert!(harness.output(cx).contains("Name: Ada"));
        assert!(
            harness
                .panel
                .read_with(cx, |panel, _| panel.input_request.is_none())
        );
    }

    #[gpui::test]
    async fn test_stderr_and_errors_are_shown(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.panel.update(cx, |panel, cx| {
            panel.execute("log(-1); stop('boom')", cx).unwrap()
        });
        let execute = harness.requests.try_recv().unwrap();
        harness.receive(
            JupyterMessage::new(
                StreamContent {
                    name: Stdio::Stderr,
                    text: "Warning message:\nIn log(-1) : NaNs produced\n".into(),
                },
                Some(&execute),
            ),
            cx,
        );
        harness.receive(
            JupyterMessage::new(
                ErrorOutput {
                    ename: String::new(),
                    evalue: "Error:\n! boom".into(),
                    traceback: Vec::new(),
                },
                Some(&execute),
            ),
            cx,
        );
        let output = harness.output(cx);
        assert!(output.contains("In log(-1) : NaNs produced"), "{output}");
        assert!(output.contains("! boom"), "{output}");
    }

    #[gpui::test]
    async fn test_clear_console(cx: &mut TestAppContext) {
        let (harness, cx) = Harness::new(cx).await;
        harness
            .panel
            .update(cx, |panel, cx| panel.execute("x <- 1", cx).unwrap());
        assert!(harness.output(cx).contains("x <- 1"));
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.clear_console(&ClearConsole, window, cx)
        });
        assert!(!harness.output(cx).contains("x <- 1"));
    }
}
